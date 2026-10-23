//! Complete public test units around failure anchors, chosen from a provider outline.
//!
//! A failure line inside a test file is useful context only together with the whole failing test
//! and everything that test can depend on in the same file. This module chooses those lines from
//! the outline the language already produced (`SymbolKind::Test` for `describe`/`it`/`test`
//! callbacks); it parses nothing itself and never adds text that is not in the source.
//!
//! Limitation, stated rather than hidden: a narrowed context is offered only when no state can be
//! shown to flow from a dropped test into a kept one with the facts available here. The outline
//! drops locals inside suite bodies, so any statement in an enclosing suite that also holds a
//! dropped test, any module-level statement no symbol covers, and any module-level variable named
//! by both a kept and a dropped test make the answer `None` (the caller keeps the whole file).
//! Shared functions and types are kept by name, without any promise about their effects.

use crate::lang::{LineRange, Outline, Symbol, SymbolKind};
use std::collections::BTreeSet;

/// Most bytes of surrounding context (imports, helpers, enclosing suite setup and module-level
/// statements) kept beside the failing tests; more means setup cannot be preserved compactly.
const MAX_CONTEXT_BYTES: usize = 8 * 1024;

/// Byte spans, in source order and merged, that deliver every failing test named by `anchors`
/// (1-based lines) as a complete unit, or `None` when that is not provable from the outline.
///
/// Kept: each anchor's innermost enclosing `Test` symbol whole; every enclosing symbol's own lines
/// (header, closing line, comments, nested declarations and — only when no sibling test is dropped
/// from it — its setup statements) with sibling tests that hold no anchor removed; `imports`; every
/// module-level code line (not a comment) no top-level symbol covers; and top-level non-test
/// declarations whose name occurs in the kept text, transitively. `None` when an anchor lies outside every test, a needed
/// range falls outside `source`, possible shared state with a dropped test is found (see the module
/// documentation), or the context beyond the failing tests exceeds [`MAX_CONTEXT_BYTES`].
pub(super) fn localize(
    outline: &Outline,
    source: &str,
    anchors: &[u32],
    imports: &[LineRange],
) -> Option<Vec<(usize, usize)>> {
    let lines = line_spans(source);
    let count = u32::try_from(lines.len()).ok()?;
    if anchors.is_empty() || count == 0 {
        return None;
    }
    let valid =
        |range: LineRange| range.start >= 1 && range.start <= range.end && range.end <= count;
    let text_of = |line: u32| {
        let (start, end) = lines[line as usize - 1];
        &source[start..end]
    };
    let mut chains: Vec<Vec<&Symbol>> = Vec::new();
    for &line in anchors {
        let chain = chain_at(&outline.symbols, line);
        let innermost = chain
            .iter()
            .rposition(|symbol| symbol.kind == SymbolKind::Test)?;
        let chain = chain[..=innermost].to_vec();
        if !chain.iter().all(|symbol| valid(symbol.range)) {
            return None;
        }
        chains.push(chain);
    }
    let on_path = |symbol: &Symbol| {
        chains
            .iter()
            .flatten()
            .any(|member| std::ptr::eq(*member, symbol))
    };
    let is_target = |symbol: &Symbol| {
        chains
            .iter()
            .any(|chain| std::ptr::eq(chain[chain.len() - 1], symbol))
    };
    let mut kept = BTreeSet::new();
    let mut tests = BTreeSet::new();
    for chain in &chains {
        let target = chain[chain.len() - 1];
        insert(&mut kept, target.range);
        insert(&mut tests, target.range);
    }
    for chain in &chains {
        for enclosing in &chain[..chain.len() - 1] {
            insert(&mut kept, enclosing.range);
        }
    }
    for chain in &chains {
        for enclosing in &chain[..chain.len() - 1] {
            if is_target(enclosing) {
                continue;
            }
            let drops_sibling = enclosing
                .children
                .iter()
                .any(|child| child.kind == SymbolKind::Test && !on_path(child));
            // Suite statements outside every child (setup calls, shared bindings the outline does
            // not list) may carry state between a dropped sibling and the kept test.
            if drops_sibling
                && (enclosing.range.start + 1..enclosing.range.end).any(|line| {
                    !enclosing
                        .children
                        .iter()
                        .any(|child| child.range.start <= line && line <= child.range.end)
                        && is_statement(text_of(line))
                })
            {
                return None;
            }
            for child in &enclosing.children {
                if child.kind == SymbolKind::Test && !on_path(child) && valid(child.range) {
                    for line in child.range.start..=child.range.end {
                        kept.remove(&line);
                    }
                }
            }
        }
    }
    for &range in imports {
        if !valid(range) {
            return None;
        }
        insert(&mut kept, range);
    }
    let mut dropped: Vec<&Symbol> = Vec::new();
    for symbol in &outline.symbols {
        collect_dropped(symbol, &on_path, &is_target, &mut dropped);
    }
    // Module-level statements no top-level symbol covers (mocks, top-level hooks, unnamed
    // state) may set up the tests; keep them, unless a dropped test could share their state.
    let covered: BTreeSet<u32> = outline
        .symbols
        .iter()
        .filter(|symbol| valid(symbol.range))
        .flat_map(|symbol| symbol.range.start..=symbol.range.end)
        .chain(imports.iter().flat_map(|range| range.start..=range.end))
        .collect();
    for line in 1..=count {
        // Comments and blank lines outside every symbol set nothing up; only code lines count.
        if !covered.contains(&line) && is_statement(text_of(line)) {
            if !dropped.is_empty() {
                return None;
            }
            kept.insert(line);
        }
    }
    // Top-level helpers named by kept text, until no new helper is named.
    loop {
        let text = joined(source, &lines, &kept);
        let mut added = false;
        for symbol in &outline.symbols {
            if symbol.kind == SymbolKind::Test
                || !valid(symbol.range)
                || kept.contains(&symbol.range.start)
                || !mentions(&text, &symbol.name)
            {
                continue;
            }
            insert(&mut kept, symbol.range);
            added = true;
        }
        if !added {
            break;
        }
    }
    // A module-level variable named by kept text and by a dropped test may carry state between them.
    let kept_text = joined(source, &lines, &kept);
    let dropped_text: String = dropped
        .iter()
        .filter(|symbol| valid(symbol.range))
        .flat_map(|symbol| symbol.range.start..=symbol.range.end)
        .map(text_of)
        .collect();
    if outline.symbols.iter().any(|symbol| {
        matches!(symbol.kind, SymbolKind::Variable | SymbolKind::Constant)
            && mentions(&kept_text, &symbol.name)
            && mentions(&dropped_text, &symbol.name)
    }) {
        return None;
    }
    let context: usize = kept
        .iter()
        .filter(|line| !tests.contains(line))
        .map(|&line| text_of(line).len())
        .sum();
    if context > MAX_CONTEXT_BYTES {
        return None;
    }
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for line in kept {
        let (start, end) = lines[line as usize - 1];
        match spans.last_mut() {
            Some(last) if last.1 == start => last.1 = end,
            _ => spans.push((start, end)),
        }
    }
    Some(spans)
}

/// Collects tests left out of the reply: a test neither kept nor enclosing a kept test, whole.
fn collect_dropped<'a>(
    symbol: &'a Symbol,
    on_path: &dyn Fn(&Symbol) -> bool,
    is_target: &dyn Fn(&Symbol) -> bool,
    dropped: &mut Vec<&'a Symbol>,
) {
    if is_target(symbol) {
        return;
    }
    if symbol.kind == SymbolKind::Test && !on_path(symbol) {
        dropped.push(symbol);
        return;
    }
    if on_path(symbol) {
        for child in &symbol.children {
            collect_dropped(child, on_path, is_target, dropped);
        }
    }
}

/// Whether a line carries code rather than only blank space, a comment or a lone closing bracket.
fn is_statement(line: &str) -> bool {
    let trimmed = line.trim();
    !(trimmed.is_empty()
        || trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed
            .chars()
            .all(|c| matches!(c, '}' | ')' | ']' | ';' | ',')))
}

/// Byte span of every line, its newline included.
fn line_spans(source: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0;
    for (at, _) in source.match_indices('\n') {
        spans.push((start, at + 1));
        start = at + 1;
    }
    if start < source.len() {
        spans.push((start, source.len()));
    }
    spans
}

/// Symbols containing `line`, outermost first, following the first containing child per level.
fn chain_at(symbols: &[Symbol], line: u32) -> Vec<&Symbol> {
    let mut chain = Vec::new();
    let mut level = symbols;
    while let Some(symbol) = level
        .iter()
        .find(|symbol| symbol.range.start <= line && line <= symbol.range.end)
    {
        chain.push(symbol);
        level = &symbol.children;
    }
    chain
}

/// Adds every line of `range`.
fn insert(lines: &mut BTreeSet<u32>, range: LineRange) {
    lines.extend(range.start..=range.end);
}

/// The text of the `kept` lines, in order.
fn joined(source: &str, lines: &[(usize, usize)], kept: &BTreeSet<u32>) -> String {
    kept.iter()
        .map(|&line| {
            let (start, end) = lines[line as usize - 1];
            &source[start..end]
        })
        .collect()
}

/// Whether `name` occurs in `text` as a whole identifier (letters, digits, `_` and `$`) that can
/// refer to a binding: a member access such as `req.text` does not, a spread `...text` does.
fn mentions(text: &str, name: &str) -> bool {
    let identifier = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    if name.is_empty() || !name.chars().all(identifier) {
        return false;
    }
    text.match_indices(name).any(|(at, _)| {
        let before = &text[..at];
        let member = before.ends_with('.') && !before.ends_with("...");
        !member
            && !before.chars().next_back().is_some_and(identifier)
            && !text[at + name.len()..]
                .chars()
                .next()
                .is_some_and(identifier)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::SymbolPath;

    fn symbol(kind: SymbolKind, name: &str, start: u32, end: u32, children: Vec<Symbol>) -> Symbol {
        Symbol {
            path: SymbolPath::new(None, vec![name.to_owned()]),
            kind,
            name: name.to_owned(),
            range: LineRange::new(start, end),
            body: LineRange::new(start, end),
            signature: name.to_owned(),
            doc: None,
            children,
        }
    }

    fn test(name: &str, start: u32, end: u32, children: Vec<Symbol>) -> Symbol {
        symbol(SymbolKind::Test, name, start, end, children)
    }

    fn outline(source: &str, symbols: Vec<Symbol>) -> Outline {
        Outline {
            file: "src/unit.test.ts".into(),
            language: crate::lang::testing::ALPHA,
            line_count: line_spans(source).len() as u32,
            symbols,
        }
    }

    fn text(source: &str, spans: &[(usize, usize)]) -> String {
        spans
            .iter()
            .map(|&(start, end)| &source[start..end])
            .collect()
    }

    const SUITE: &str = "\
import { compose } from './compose'

function build(fn) {
  return [fn]
}

function unused() {
  return 0
}

describe('compose', () => {
  // cases share no state
  it('first', () => {
    expect(build(1)).toEqual([1])
  })
  it('fails', () => {
    expect(build(2)).toBe(3)
  })
})

it('top level', () => {
  expect(0).toBe(0)
})
";

    fn suite_symbols() -> Vec<Symbol> {
        vec![
            symbol(SymbolKind::Function, "build", 3, 5, vec![]),
            symbol(SymbolKind::Function, "unused", 7, 9, vec![]),
            test(
                "describe(\"compose\")",
                11,
                19,
                vec![
                    test("it(\"first\")", 13, 15, vec![]),
                    test("it(\"fails\")", 16, 18, vec![]),
                ],
            ),
            test("it(\"top level\")", 21, 23, vec![]),
        ]
    }

    /// The failing test with its suite header, comment and closing line, the import and the
    /// helper it names; the sibling test, the unused helper and the unrelated test stay out.
    #[test]
    fn failing_test_keeps_suite_frame_imports_and_named_helpers() {
        let outline = outline(SUITE, suite_symbols());
        let spans = localize(&outline, SUITE, &[17], &[LineRange::new(1, 1)]).unwrap();
        assert_eq!(
            text(SUITE, &spans),
            "\
import { compose } from './compose'
function build(fn) {
  return [fn]
}
describe('compose', () => {
  // cases share no state
  it('fails', () => {
    expect(build(2)).toBe(3)
  })
})
"
        );
    }

    /// Two anchors in two sibling tests keep both; neither removes the other. The caller always
    /// passes the import spans; an import not named as one is an uncovered module statement, and
    /// beside the dropped top-level test that is possible shared state.
    #[test]
    fn anchors_in_sibling_tests_keep_both() {
        let outline = outline(SUITE, suite_symbols());
        let imports = [LineRange::new(1, 1)];
        let kept = text(
            SUITE,
            &localize(&outline, SUITE, &[14, 17], &imports).unwrap(),
        );
        assert!(
            kept.contains("it('first'") && kept.contains("it('fails'"),
            "{kept}"
        );
        assert!(!kept.contains("top level"), "{kept}");
        assert_eq!(localize(&outline, SUITE, &[14, 17], &[]), None);
    }

    /// Suite setup is kept whole when no sibling test is dropped from that suite.
    #[test]
    fn suite_setup_stays_when_no_sibling_is_dropped() {
        let source = "\
describe('only', () => {
  let value = 0
  beforeEach(() => {
    value = 1
  })
  it('reads', () => {
    expect(value).toBe(2)
  })
})
";
        let outline = outline(
            source,
            vec![test(
                "describe(\"only\")",
                1,
                9,
                vec![test("it(\"reads\")", 6, 8, vec![])],
            )],
        );
        assert_eq!(
            text(source, &localize(&outline, source, &[7], &[]).unwrap()),
            source
        );
    }

    /// An earlier sibling can leave state behind (a counter, a mutated const object): with any
    /// suite statement beside a dropped sibling, nothing narrower than the file is offered.
    #[test]
    fn possible_state_from_a_dropped_sibling_is_none() {
        for setup in ["  let counter = 0", "  const cache = {}"] {
            let source = format!(
                "describe('state', () => {{\n{setup}\n  it('writes', () => {{\n    \
                 counter++; cache.a = 1\n  }})\n  it('reads', () => {{\n    \
                 expect(counter + cache.a).toBe(2)\n  }})\n}})\n"
            );
            let outline = outline(
                &source,
                vec![test(
                    "describe(\"state\")",
                    1,
                    9,
                    vec![
                        test("it(\"writes\")", 3, 5, vec![]),
                        test("it(\"reads\")", 6, 8, vec![]),
                    ],
                )],
            );
            assert_eq!(localize(&outline, &source, &[7], &[]), None, "{setup}");
        }
    }

    /// A module-level variable named by the kept test and by a dropped test may carry state;
    /// one named only by the kept test is kept as its helper.
    #[test]
    fn module_variables_shared_with_dropped_tests_are_none() {
        let source = "\
const store = new Map()
const answer = 42
it('writes', () => {
  store.set('a', 1)
})
it('reads', () => {
  expect(store.get('a') + answer).toBe(43)
})
";
        let symbols = || {
            vec![
                symbol(SymbolKind::Constant, "store", 1, 1, vec![]),
                symbol(SymbolKind::Constant, "answer", 2, 2, vec![]),
                test("it(\"writes\")", 3, 5, vec![]),
                test("it(\"reads\")", 6, 8, vec![]),
            ]
        };
        assert_eq!(
            localize(&outline(source, symbols()), source, &[7], &[]),
            None
        );
        let independent = source.replace("store.set('a', 1)", "expect(1).toBe(1)");
        let kept = text(
            &independent,
            &localize(&outline(&independent, symbols()), &independent, &[7], &[]).unwrap(),
        );
        assert!(
            kept.contains("const store") && kept.contains("const answer"),
            "{kept}"
        );
        assert!(!kept.contains("it('writes'"), "{kept}");
    }

    /// Module-level statements no symbol covers are kept with a lone test, helpers transitively;
    /// beside a dropped test they may share state, so nothing narrower is offered.
    #[test]
    fn module_statements_and_transitive_helpers() {
        let source = "\
vi.mock('./clock')
type Tuple = [number]
function wrap(value): Tuple {
  return [value]
}
it('wraps', () => {
  expect(wrap(1)).toEqual([2])
})
";
        let symbols = vec![
            symbol(SymbolKind::TypeAlias, "Tuple", 2, 2, vec![]),
            symbol(SymbolKind::Function, "wrap", 3, 5, vec![]),
            test("it(\"wraps\")", 6, 8, vec![]),
        ];
        assert_eq!(
            text(
                source,
                &localize(&outline(source, symbols), source, &[7], &[]).unwrap()
            ),
            source
        );
        let with_other = format!("{source}it('other', () => {{}})\n");
        let symbols = vec![
            symbol(SymbolKind::TypeAlias, "Tuple", 2, 2, vec![]),
            symbol(SymbolKind::Function, "wrap", 3, 5, vec![]),
            test("it(\"wraps\")", 6, 8, vec![]),
            test("it(\"other\")", 9, 9, vec![]),
        ];
        assert_eq!(
            localize(&outline(&with_other, symbols), &with_other, &[7], &[]),
            None
        );
    }

    /// Module-level comments outside every symbol are not setup: a large trailing comment block
    /// neither enters the context nor exceeds its limit.
    #[test]
    fn module_comments_are_not_context() {
        let padding: String = (0..300)
            .map(|index| format!("// padding comment {index:03} padding padding padding\n"))
            .collect();
        let source = format!("#[test]\nfn fails() {{\n    assert!(false);\n}}\n{padding}");
        let outline = outline(&source, vec![test("fails", 1, 4, vec![])]);
        let kept = text(&source, &localize(&outline, &source, &[3], &[]).unwrap());
        assert_eq!(kept, "#[test]\nfn fails() {\n    assert!(false);\n}\n");
    }

    /// An anchor outside every test (here inside a helper) is not a provable test unit.
    #[test]
    fn anchor_outside_tests_is_none() {
        let outline = outline(SUITE, suite_symbols());
        assert_eq!(localize(&outline, SUITE, &[4], &[]), None);
        assert_eq!(localize(&outline, SUITE, &[17, 4], &[]), None);
        assert_eq!(localize(&outline, SUITE, &[], &[]), None);
    }

    /// Setup too large to carry compactly, or a range outside the source, is refused.
    #[test]
    fn oversized_setup_or_invalid_range_is_none() {
        let setup: String = (0..400)
            .map(|index| format!("  const value_{index} = {index} // padding padding\n"))
            .collect();
        let source = format!("describe('big', () => {{\n{setup}  it('x', () => {{}})\n}})\n");
        let end = line_spans(&source).len() as u32;
        let big = outline(
            &source,
            vec![test(
                "describe(\"big\")",
                1,
                end,
                vec![test("it(\"x\")", end - 1, end - 1, vec![])],
            )],
        );
        assert_eq!(localize(&big, &source, &[end - 1], &[]), None);
        let outline = outline(SUITE, suite_symbols());
        assert_eq!(
            localize(&outline, SUITE, &[17], &[LineRange::new(90, 91)]),
            None
        );
    }

    /// Identifier matching ignores substrings of longer names; comments and closers are not code.
    #[test]
    fn helpers_match_identifiers_and_statements() {
        assert!(mentions("x = build(1)", "build"));
        assert!(!mentions("x = rebuild(1)", "build"));
        assert!(!mentions("x = build_all(1)", "build"));
        assert!(!mentions("x", "describe(\"a\")"));
        assert!(!mentions("await req.text()", "text"));
        assert!(mentions("f(...text)", "text"));
        assert!(is_statement("  let a = 1"));
        assert!(!is_statement("  // note"));
        assert!(!is_statement("  })"));
        assert!(!is_statement("   "));
    }
}
