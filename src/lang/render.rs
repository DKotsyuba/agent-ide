//! Compact model-facing renderings of outlines, symbol bodies and symbol cards.
//!
//! Everything here is plain text with 1-based lines, ceilings on list lengths and no internal
//! bookkeeping fields: the agent reads what a human would read in an editor's outline, "go to
//! definition" preview and "find usages" panel.

use std::path::Path;

use async_lsp::lsp_types as lsp;

use super::{LineRange, Outline, Symbol, SymbolKind, slice_lines};

/// Usages printed inline before the list collapses into "… N more".
pub const MAX_USAGE_LINES: usize = 30;
/// Callers or callees printed inline per level.
pub const MAX_CALL_LINES: usize = 20;
/// Doc paragraph ceiling in characters.
pub const MAX_DOC_CHARS: usize = 600;

/// Renders a file skeleton: one line per symbol, members indented, test modules collapsed.
pub fn outline_text(outline: &Outline) -> String {
    let mut out = format!(
        "{}  ({} lines, {})\n",
        outline.file.display(),
        outline.line_count,
        outline.language
    );
    let mut count = 0usize;
    let mut tests = 0usize;
    for symbol in &outline.symbols {
        render_symbol_line(symbol, 0, &mut out, &mut count, &mut tests);
    }
    out.push_str(&format!("  ({count} symbols"));
    if tests > 0 {
        out.push_str(&format!("; {tests} tests collapsed"));
    }
    out.push_str(")\n");
    out
}

fn render_symbol_line(
    symbol: &Symbol,
    depth: usize,
    out: &mut String,
    count: &mut usize,
    tests: &mut usize,
) {
    if symbol.kind == SymbolKind::Test {
        // A test module or test function: count it and its members, print one collapsed line.
        let mut members = 0usize;
        symbol.walk(&mut |_| members += 1);
        *tests += members;
        out.push_str(&format!(
            "{:>5}  {}{} [{} tests collapsed]\n",
            symbol.range.start,
            "  ".repeat(depth),
            symbol.signature,
            members
        ));
        return;
    }
    *count += 1;
    let doc = symbol
        .doc
        .as_deref()
        .map(|doc| first_line(doc, 72))
        .filter(|doc| !doc.is_empty())
        .map(|doc| format!("    // {doc}"))
        .unwrap_or_default();
    out.push_str(&format!(
        "{:>5}  {}{}{}\n",
        symbol.range.start,
        "  ".repeat(depth),
        symbol.signature,
        doc
    ));
    for child in &symbol.children {
        render_symbol_line(child, depth + 1, out, count, tests);
    }
}

/// Renders a symbol body or an explicit line range with line numbers, header included.
pub fn read_text(file: &Path, title: Option<&str>, range: LineRange, source: &str) -> String {
    let mut out = match title {
        Some(title) => format!("{title}  (lines {range})\n"),
        None => format!("{}  (lines {range})\n", file.display()),
    };
    let width = range.end.to_string().len();
    for (index, line) in slice_lines(source, range).split_inclusive('\n').enumerate() {
        let number = range.start as usize + index;
        out.push_str(&format!("{number:>width$}  {line}"));
        if !line.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// One usage line: relative path, line, and the source line text trimmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Usage {
    pub file: String,
    pub line: u32,
    pub text: String,
    pub is_test: bool,
}

/// One caller or callee line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub name: String,
    pub file: String,
    pub line: u32,
}

/// Everything a symbol card prints; absent parts are simply omitted.
#[derive(Clone, Debug, Default)]
pub struct SymbolCard {
    /// `BindingStatus — enum, src/assistance/host_binding.rs#BindingStatus (lines 330–346)`.
    pub heading: String,
    pub signature: Option<String>,
    pub doc: Option<String>,
    /// Definition snippet: file, range and numbered text, already rendered by [`read_text`].
    pub definition: Option<String>,
    pub usages: Vec<Usage>,
    pub callers: Vec<Call>,
    pub callees: Vec<Call>,
    /// Short sha, date and subject of recent commits touching the definition.
    pub history: Vec<String>,
    /// Detail reference for the full usage list, printed when usages were cut.
    pub more_detail: Option<String>,
}

/// Renders the card with the fixed ceilings.
pub fn symbol_card_text(card: &SymbolCard) -> String {
    let mut out = format!("symbol: {}\n", card.heading);
    if let Some(signature) = &card.signature {
        out.push_str(&format!("signature: {signature}\n"));
    }
    if let Some(doc) = &card.doc {
        let doc = clip(doc, MAX_DOC_CHARS);
        let mut lines = doc.lines();
        if let Some(first) = lines.next() {
            out.push_str(&format!("doc: {first}\n"));
            for line in lines {
                out.push_str(&format!("     {line}\n"));
            }
        }
    }
    if let Some(definition) = &card.definition {
        out.push_str("definition ");
        out.push_str(definition);
        if !definition.ends_with('\n') {
            out.push('\n');
        }
    }
    if !card.usages.is_empty() {
        let (src, tests): (Vec<_>, Vec<_>) = card.usages.iter().partition(|usage| !usage.is_test);
        let files = card
            .usages
            .iter()
            .map(|usage| usage.file.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        out.push_str(&format!(
            "usages: {} in {} files (src {}, tests {})\n",
            card.usages.len(),
            files,
            src.len(),
            tests.len()
        ));
        let width = card
            .usages
            .iter()
            .take(MAX_USAGE_LINES)
            .map(|usage| usage.file.len() + 1 + usage.line.to_string().len())
            .max()
            .unwrap_or(0);
        for usage in src.iter().chain(tests.iter()).take(MAX_USAGE_LINES) {
            let location = format!("{}:{}", usage.file, usage.line);
            out.push_str(&format!("  {location:<width$}  {}\n", usage.text));
        }
        let hidden = card.usages.len().saturating_sub(MAX_USAGE_LINES);
        if hidden > 0 {
            match &card.more_detail {
                Some(detail) => {
                    out.push_str(&format!("  … {hidden} more (ide.inspect {detail})\n"))
                }
                None => out.push_str(&format!("  … {hidden} more\n")),
            }
        }
    }
    render_calls(&mut out, "callers", &card.callers);
    render_calls(&mut out, "callees", &card.callees);
    if !card.history.is_empty() {
        out.push_str(&format!(
            "history: {} last commits touching the definition\n",
            card.history.len()
        ));
        for entry in &card.history {
            out.push_str(&format!("  {entry}\n"));
        }
    }
    out
}

fn render_calls(out: &mut String, label: &str, calls: &[Call]) {
    if calls.is_empty() {
        return;
    }
    out.push_str(&format!("{label}: {}\n", calls.len()));
    for call in calls.iter().take(MAX_CALL_LINES) {
        out.push_str(&format!("  {}  {}:{}\n", call.name, call.file, call.line));
    }
    if calls.len() > MAX_CALL_LINES {
        out.push_str(&format!("  … {} more\n", calls.len() - MAX_CALL_LINES));
    }
}

/// Text of one source line (1-based), trimmed, for usage listings.
pub fn line_text(source: &str, line: u32) -> String {
    source
        .lines()
        .nth(line.saturating_sub(1) as usize)
        .map(|text| clip(text.trim(), 120))
        .unwrap_or_default()
}

/// Relative display path for a location inside the worktree; absolute otherwise.
pub fn display_path(worktree: &Path, uri: &lsp::Url) -> String {
    match uri.to_file_path() {
        Ok(path) => path
            .strip_prefix(worktree)
            .map(|relative| relative.display().to_string())
            .unwrap_or_else(|_| path.display().to_string()),
        Err(_) => uri.to_string(),
    }
}

fn first_line(text: &str, max: usize) -> String {
    clip(text.lines().next().unwrap_or("").trim(), max)
}

/// Clips text to `max` characters, ending with `…` when truncated.
pub(super) fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let mut clipped: String = text.chars().take(max.saturating_sub(1)).collect();
        clipped.push('…');
        clipped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::{Language, SymbolPath};
    use std::path::PathBuf;

    fn symbol(name: &str, kind: SymbolKind, start: u32, end: u32, doc: Option<&str>) -> Symbol {
        Symbol {
            path: SymbolPath::new(Some(PathBuf::from("a.rs")), vec![name.into()]),
            kind,
            name: name.into(),
            range: LineRange::new(start, end),
            body: LineRange::new(start, end),
            signature: format!("pub fn {name}()"),
            doc: doc.map(str::to_owned),
            children: vec![],
        }
    }

    #[test]
    fn outline_prints_lines_docs_and_collapses_tests() {
        let mut tests = symbol("tests", SymbolKind::Test, 40, 90, None);
        tests.signature = "mod tests".into();
        tests.children = vec![symbol("it_works", SymbolKind::Test, 42, 50, None)];
        let outline = Outline {
            file: PathBuf::from("a.rs"),
            language: Language::Rust,
            line_count: 90,
            symbols: vec![
                symbol(
                    "run",
                    SymbolKind::Function,
                    3,
                    20,
                    Some("Runs it.\n\nMore."),
                ),
                tests,
            ],
        };
        let text = outline_text(&outline);
        assert_eq!(
            text,
            "a.rs  (90 lines, rust)\n    3  pub fn run()    // Runs it.\n   40  mod tests [2 tests collapsed]\n  (1 symbols; 2 tests collapsed)\n"
        );
    }

    #[test]
    fn read_numbers_lines_from_the_range_start() {
        let text = read_text(
            Path::new("a.rs"),
            Some("a.rs#run"),
            LineRange::new(9, 11),
            "1\n2\n3\n4\n5\n6\n7\n8\nfn run() {\n    x\n}\n",
        );
        assert_eq!(
            text,
            "a.rs#run  (lines 9–11)\n 9  fn run() {\n10      x\n11  }\n"
        );
    }

    #[test]
    fn symbol_card_lists_usages_with_ceiling_and_groups() {
        let mut card = SymbolCard {
            heading: "Run — fn, a.rs#Run (lines 1–3)".into(),
            signature: Some("pub fn run()".into()),
            doc: Some("Runs.".into()),
            ..Default::default()
        };
        for index in 0..35 {
            card.usages.push(Usage {
                file: format!("f{}.rs", index % 3),
                line: index + 1,
                text: "run();".into(),
                is_test: index % 5 == 0,
            });
        }
        card.more_detail = Some("sym-1".into());
        let text = symbol_card_text(&card);
        assert!(text.starts_with("symbol: Run — fn, a.rs#Run (lines 1–3)\nsignature: pub fn run()\ndoc: Runs.\nusages: 35 in 3 files (src 28, tests 7)\n"));
        assert!(text.contains("… 5 more (ide.inspect sym-1)"));
        assert_eq!(text.matches("run();").count(), MAX_USAGE_LINES);
    }
}
