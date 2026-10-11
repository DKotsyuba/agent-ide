//! File-level regions: the lines of a file that no outline symbol covers.
//!
//! Contract: `docs/contracts/symbol-address-v1.md` section 5. [`scan_gap`] is a deterministic line
//! scan driven by static per-language [`RegionRules`]; it is not a parser. The scanner knows
//! nothing about names or addresses: it returns raw regions in file order and
//! [`super::address::AddressMap::build`] names them.

use super::LineRange;

/// Static per-language facts the region scanner needs. A language returns its rules from
/// `LanguageSupport::region_rules`; a language without rules gets [`GENERIC_RULES`].
#[derive(Clone, Copy, Debug)]
pub struct RegionRules {
    /// Prefixes that make a (trimmed) line a comment line.
    pub line_comments: &'static [&'static str],
    /// Opening and closing marker of a block comment.
    pub block_comment: Option<(&'static str, &'static str)>,
    /// Case-insensitive line prefixes of header statements (`#![`, `<!doctype`, `@charset`).
    pub header_prefixes: &'static [&'static str],
    /// Line prefixes that start an import statement (after an optional `pub(..)`).
    pub import_prefixes: &'static [&'static str],
    /// Line prefixes of attribute or decorator lines that belong to the statement below them.
    pub attribute_prefixes: &'static [&'static str],
    /// Skip a leading `pub` / `pub(crate)`-style visibility word before testing
    /// `import_prefixes`.
    pub visibility_prefix: bool,
    /// An import statement ends only at a line ending in `;`; otherwise at bracket depth 0.
    pub semicolon_imports: bool,
    /// Blocks continue while the next line is indented deeper than the block's first line.
    pub indentation_blocks: bool,
    /// A block is a run of non-blank lines.
    pub blank_line_blocks: bool,
    /// Line prefix of the block that becomes `@main` (an entry-point guard).
    pub main_marker: Option<&'static str>,
    /// A leading triple-quoted string is part of the header (a module docstring).
    pub docstring_header: bool,
}

/// Rules for a registered language that states none: `//` and `/* */` comments, no imports.
pub const GENERIC_RULES: RegionRules = RegionRules {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    header_prefixes: &[],
    import_prefixes: &[],
    attribute_prefixes: &[],
    visibility_prefix: false,
    semicolon_imports: false,
    indentation_blocks: false,
    blank_line_blocks: false,
    main_marker: None,
    docstring_header: false,
};

/// Kind of a raw region; mirrors `address::RegionKind`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawKind {
    /// Leading comments and header statements.
    Header,
    /// One run of import statements.
    Imports,
    /// The entry-point guard block (`main_marker`).
    Main,
    /// A top-level statement block.
    Code,
    /// Blank and comment-only lines between units.
    After,
}

/// One region found by the scanner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawRegion {
    /// What the region is.
    pub kind: RawKind,
    /// Absolute 1-based inclusive lines.
    pub span: LineRange,
    /// Block leader for [`RawKind::Code`] (contract section 5), empty otherwise.
    pub leader: String,
}

/// Scans the gap `first..=last` (1-based absolute lines of `lines`, where `lines[0]` is line 1)
/// that no symbol covers and returns its regions in order, tiling the gap completely. `leading`
/// is true for the gap that starts at line 1: its leading trivia and header statements become the
/// `Header` region.
pub fn scan_gap(
    lines: &[&str],
    first: u32,
    last: u32,
    leading: bool,
    rules: &RegionRules,
) -> Vec<RawRegion> {
    let mut out = Vec::new();
    // A caller may pass a gap that runs past the text (a stale outline); only real lines count.
    let last = last.min(lines.len() as u32);
    if first == 0 || first > last {
        return out;
    }
    let (f, l) = (first as usize - 1, last as usize - 1);
    let trivia = trivia_flags(lines, f, l, rules);
    let is_trivia = |i: usize| trivia[i - f];
    let mut i = f;
    if leading {
        while i <= l {
            if is_trivia(i) {
                i += 1;
            } else if let Some(end) = header_statement_end(lines, i, l, rules) {
                i = end + 1;
            } else {
                break;
            }
        }
        if i > f {
            out.push(raw(RawKind::Header, f, i - 1, String::new()));
        }
    }
    while i <= l {
        if is_trivia(i) {
            let start = i;
            while i <= l && is_trivia(i) {
                i += 1;
            }
            out.push(raw(RawKind::After, start, i - 1, String::new()));
            continue;
        }
        let start = i;
        let mut at = i;
        while at <= l && is_attribute(lines[at], rules) {
            at += 1;
        }
        let at = at.min(l);
        if is_import_start(lines[at], rules) {
            let mut end = statement_end(lines, at, l, rules, true);
            loop {
                let mut k = end + 1;
                while k <= l && is_trivia(k) {
                    k += 1;
                }
                let mut a = k;
                while a <= l && is_attribute(lines[a], rules) {
                    a += 1;
                }
                if a <= l && k <= l && is_import_start(lines[a.min(l)], rules) {
                    end = statement_end(lines, a, l, rules, true);
                } else {
                    break;
                }
            }
            out.push(raw(RawKind::Imports, start, end, String::new()));
            i = end + 1;
        } else {
            let end = statement_end(lines, at, l, rules, false);
            let kind = match rules.main_marker {
                Some(marker) if lines[at].starts_with(marker) => RawKind::Main,
                _ => RawKind::Code,
            };
            out.push(raw(kind, start, end, leader(lines, start, end, rules)));
            i = end + 1;
        }
    }
    out
}

/// Builds a region from 0-based inclusive line indices.
fn raw(kind: RawKind, start: usize, end: usize, leader: String) -> RawRegion {
    RawRegion {
        kind,
        span: LineRange::new(start as u32 + 1, end as u32 + 1),
        leader,
    }
}

/// Per line of `f..=l`: blank or comment-only.
fn trivia_flags(lines: &[&str], f: usize, l: usize, rules: &RegionRules) -> Vec<bool> {
    let mut flags = Vec::with_capacity(l - f + 1);
    let mut in_block = false;
    for line in &lines[f..=l] {
        let t = line.trim();
        let mut trivia = false;
        if in_block {
            trivia = true;
            if let Some((_, close)) = rules.block_comment
                && let Some(at) = t.find(close)
            {
                in_block = false;
                trivia = t[at + close.len()..].trim().is_empty();
            }
        } else if t.is_empty() || rules.line_comments.iter().any(|p| t.starts_with(p)) {
            trivia = true;
        } else if let Some((open, close)) = rules.block_comment
            && let Some(inner) = t.strip_prefix(open)
        {
            match inner.find(close) {
                Some(at) => trivia = inner[at + close.len()..].trim().is_empty(),
                None => {
                    in_block = true;
                    trivia = true;
                }
            }
        }
        flags.push(trivia);
    }
    flags
}

/// End line index of a header statement starting at `i`, if line `i` is one.
fn header_statement_end(lines: &[&str], i: usize, l: usize, rules: &RegionRules) -> Option<usize> {
    let t = lines[i].trim();
    let lower = t.to_ascii_lowercase();
    if rules
        .header_prefixes
        .iter()
        .any(|p| lower.starts_with(&p.to_ascii_lowercase()))
    {
        return Some(i);
    }
    if rules.docstring_header {
        let body = t.trim_start_matches(['r', 'R', 'u', 'U', 'b', 'B']);
        for q in ["\"\"\"", "'''"] {
            if let Some(rest) = body.strip_prefix(q) {
                if rest.contains(q) {
                    return Some(i);
                }
                return (i + 1..=l).find(|&j| lines[j].contains(q)).or(Some(l));
            }
        }
    }
    None
}

/// Whether the line is an attribute or decorator line.
fn is_attribute(line: &str, rules: &RegionRules) -> bool {
    !line.starts_with(char::is_whitespace)
        && rules.attribute_prefixes.iter().any(|p| line.starts_with(p))
}

/// Whether the line starts an import statement.
fn is_import_start(line: &str, rules: &RegionRules) -> bool {
    if line.starts_with(char::is_whitespace) {
        return false;
    }
    let mut t = line;
    if rules.visibility_prefix
        && let Some(rest) = t.strip_prefix("pub")
    {
        t = if let Some(paren) = rest.strip_prefix('(') {
            paren.split_once(')').map_or("", |(_, r)| r).trim_start()
        } else {
            rest.trim_start()
        };
    }
    rules.import_prefixes.iter().any(|p| t.starts_with(p))
}

/// Bracket depth change of one line, skipping strings and trailing line comments.
fn depth_delta(line: &str, rules: &RegionRules) -> i32 {
    let mut depth = 0;
    let mut quote: Option<char> = None;
    let mut chars = line.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if let Some(q) = quote {
            if c == '\\' {
                chars.next();
            } else if c == q {
                quote = None;
            }
            continue;
        }
        if rules
            .line_comments
            .iter()
            .any(|p| line[at..].starts_with(p))
        {
            break;
        }
        match c {
            '"' | '\'' | '`' => {
                // A lifetime or label marker (`'a`) has no closing quote; treat it as plain text.
                if c != '\'' || line[at + 1..].contains('\'') {
                    quote = Some(c);
                }
            }
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
    }
    depth
}

/// The line without a trailing comment (`use a; // note`, `@import "x"; /* c */`), so a statement
/// that ends in `;` is recognised whatever follows it. Quotes are skipped like in
/// [`depth_delta`].
fn code_part<'a>(line: &'a str, rules: &RegionRules) -> &'a str {
    let mut quote: Option<char> = None;
    let mut chars = line.char_indices();
    while let Some((at, c)) = chars.next() {
        if let Some(q) = quote {
            if c == '\\' {
                chars.next();
            } else if c == q {
                quote = None;
            }
            continue;
        }
        let rest = &line[at..];
        if rules.line_comments.iter().any(|p| rest.starts_with(p))
            || rules
                .block_comment
                .is_some_and(|(open, _)| rest.starts_with(open))
        {
            return &line[..at];
        }
        if matches!(c, '"' | '`') || (c == '\'' && rest[1..].contains('\'')) {
            quote = Some(c);
        }
    }
    line
}

/// Leading whitespace width of a line.
fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Whether a non-blank line at column 0 continues the statement above it.
fn is_continuation(line: &str) -> bool {
    const STARTS: [&str; 6] = [".", "?", ":", "&&", "||", ")"];
    if STARTS.iter().any(|p| line.starts_with(p)) || line.starts_with(']') || line.starts_with('}')
    {
        return true;
    }
    ["else", "elif", "except", "finally", "catch"]
        .iter()
        .any(|w| {
            line.strip_prefix(w)
                .is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
        })
}

/// Last line index of the statement or block starting at `start`, never beyond `l`.
fn statement_end(
    lines: &[&str],
    start: usize,
    l: usize,
    rules: &RegionRules,
    import: bool,
) -> usize {
    let first_indent = indent(lines[start]);
    let mut depth = 0;
    let mut j = start;
    loop {
        depth += depth_delta(lines[j], rules);
        if j >= l {
            return l;
        }
        let t = code_part(lines[j], rules).trim_end();
        if depth <= 0 {
            if import {
                let open_semicolon = rules.semicolon_imports && !t.ends_with(';');
                if !open_semicolon && !t.ends_with('\\') {
                    return j;
                }
            } else if rules.blank_line_blocks {
                if lines[j + 1].trim().is_empty() {
                    return j;
                }
            } else if !is_attribute(lines[j], rules) {
                let next = (j + 1..=l).find(|&k| !lines[k].trim().is_empty());
                let Some(next) = next else { return j };
                let deeper = rules.indentation_blocks && indent(lines[next]) > first_indent;
                if !deeper {
                    if t.ends_with(';') || t.ends_with('}') {
                        return j;
                    }
                    if !lines[next].starts_with(char::is_whitespace)
                        && !is_continuation(lines[next])
                    {
                        return j;
                    }
                }
            }
        }
        j += 1;
    }
}

/// The block leader of lines `start..=end` (contract section 5).
fn leader(lines: &[&str], start: usize, end: usize, rules: &RegionRules) -> String {
    let code = (start..=end)
        .map(|i| lines[i])
        .find(|line| !is_attribute(line, rules))
        .unwrap_or("");
    let mut text = code.trim();
    for p in rules.line_comments {
        if let Some(at) = text.find(p) {
            text = &text[..at];
        }
    }
    let mut cut = text.len();
    let bytes = text.as_bytes();
    for (at, c) in text.char_indices() {
        let stop = match c {
            '(' | '{' | '[' | '=' | ';' | ',' => true,
            ':' => bytes.get(at + 1) != Some(&b':') && (at == 0 || bytes[at - 1] != b':'),
            _ => false,
        };
        if stop {
            cut = at;
            break;
        }
    }
    let collapsed = text[..cut].split_whitespace().collect::<Vec<_>>().join(" ");
    let clipped: String = collapsed.chars().take(32).collect();
    let clipped = clipped.trim_end().to_owned();
    if clipped.is_empty() {
        "stmt".to_owned()
    } else {
        clipped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRACED: RegionRules = RegionRules {
        line_comments: &["//"],
        block_comment: Some(("/*", "*/")),
        header_prefixes: &["#![", "#!"],
        import_prefixes: &["use ", "extern crate "],
        attribute_prefixes: &["#["],
        visibility_prefix: true,
        semicolon_imports: true,
        indentation_blocks: false,
        blank_line_blocks: false,
        main_marker: None,
        docstring_header: false,
    };

    fn kinds(src: &str, rules: &RegionRules) -> Vec<(RawKind, u32, u32, String)> {
        let lines: Vec<&str> = src.lines().collect();
        scan_gap(&lines, 1, lines.len() as u32, true, rules)
            .into_iter()
            .map(|r| (r.kind, r.span.start, r.span.end, r.leader))
            .collect()
    }

    #[test]
    fn rust_header_imports_code_tile_the_gap() {
        let src = "//! doc\n#![allow(x)]\n\nuse a::b;\n// c\nuse c::{\n    d,\n};\n\nthread_local! {\n    static X: u8 = 1;\n}\n";
        use RawKind::*;
        assert_eq!(
            kinds(src, &BRACED),
            vec![
                (Header, 1, 3, String::new()),
                (Imports, 4, 8, String::new()),
                (After, 9, 9, String::new()),
                (Code, 10, 12, "thread_local!".to_owned()),
            ]
        );
    }

    #[test]
    fn an_import_ends_at_a_semicolon_before_a_trailing_comment() {
        let src = "use a; // note\nuse b; /* c */\n\nfn_call();\n";
        use RawKind::*;
        assert_eq!(
            kinds(src, &BRACED),
            vec![
                (Imports, 1, 2, String::new()),
                (After, 3, 3, String::new()),
                (Code, 4, 4, "fn_call".to_owned()),
            ]
        );
    }

    #[test]
    fn leader_rules() {
        let l = |s: &str| leader(&[s], 0, 0, &BRACED);
        assert_eq!(l("module.exports = {"), "module.exports");
        assert_eq!(l("if TYPE_CHECKING:"), "if TYPE_CHECKING");
        assert_eq!(l("sys.path.insert(0, root)"), "sys.path.insert");
        assert_eq!(l("a::b::c();"), "a::b::c");
        assert_eq!(l("()"), "stmt");
    }
}
