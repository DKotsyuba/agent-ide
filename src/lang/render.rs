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

/// Renders one bounded directory level, counting regular files recursively inside child folders.
pub fn directory_outline(root: &Path, directory: &Path) -> std::io::Result<String> {
    use std::fs;

    let absolute = root.join(directory);
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in fs::read_dir(&absolute)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.')
            || matches!(
                name.as_ref(),
                "target" | "node_modules" | "__pycache__" | ".git"
            )
        {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            dirs.push((name.into_owned(), count_files(&entry.path())?));
        } else if kind.is_file() {
            files.push((name.into_owned(), entry.path()));
        }
    }
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let displayed = files.len().min(200);
    let title = if directory.as_os_str().is_empty() {
        String::new()
    } else {
        format!("{}/", directory.display())
    };
    let mut out = format!("{title}  ({} files, {} dirs)\n", files.len(), dirs.len());
    if !dirs.is_empty() {
        out.push_str("  dirs: ");
        out.push_str(
            &dirs
                .iter()
                .map(|(name, count)| format!("{name}/ {count}"))
                .collect::<Vec<_>>()
                .join(" · "),
        );
        out.push('\n');
    }
    for (name, path) in files.iter().take(displayed) {
        let (line_count, prefix) = read_file_lines_and_prefix(path)?;
        let doc = first_file_doc(path, &prefix);
        out.push_str(&format!("  {name:<20} {line_count:>5}"));
        if let Some(doc) = doc {
            out.push_str("  ");
            out.push_str(&clip(&doc, 60));
        }
        out.push('\n');
    }
    if files.len() > displayed {
        out.push_str(&format!("  … {} more files\n", files.len() - displayed));
    }
    Ok(out)
}

/// Counts file lines in constant memory and retains only the first 4 KiB for documentation.
fn read_file_lines_and_prefix(path: &Path) -> std::io::Result<(u32, Vec<u8>)> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut buffer = [0; 8192];
    let mut prefix = Vec::with_capacity(4096);
    let mut newlines = 0u64;
    let mut last = None;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let bytes = &buffer[..read];
        newlines += bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
        last = bytes.last().copied();
        let take = (4096 - prefix.len()).min(read);
        prefix.extend_from_slice(&bytes[..take]);
    }
    let lines = newlines + u64::from(last.is_some() && last != Some(b'\n'));
    Ok((lines.min(u32::MAX as u64) as u32, prefix))
}

/// Counts non-hidden regular files below a directory, without following symlinks.
fn count_files(directory: &Path) -> std::io::Result<usize> {
    let mut count = 0;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.')
            || matches!(
                name.as_ref(),
                "target" | "node_modules" | "__pycache__" | ".git"
            )
        {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            count += count_files(&entry.path())?;
        } else if kind.is_file() {
            count += 1;
        }
    }
    Ok(count)
}

/// Extracts the first module documentation line for the file's supported source language.
fn first_file_doc(path: &Path, bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let lines: Vec<_> = text.lines().collect();
    match path.extension()?.to_str()? {
        "rs" => {
            let mut docs = Vec::new();
            for line in &lines {
                let line = line.trim();
                if line.is_empty() && docs.is_empty() {
                    continue;
                }
                if let Some(doc) = line
                    .strip_prefix("//!")
                    .or_else(|| line.strip_prefix("///"))
                {
                    docs.push(doc.trim());
                } else if !line.starts_with("//") && !line.starts_with("#![") {
                    break;
                }
            }
            docs.into_iter()
                .find(|doc| !doc.is_empty())
                .map(str::to_owned)
        }
        "py" | "pyi" => {
            let first = lines.iter().find(|line| !line.trim().is_empty())?.trim();
            let quote = if first.starts_with("\"\"\"") {
                "\"\"\""
            } else if first.starts_with("'''") {
                "'''"
            } else {
                return None;
            };
            let content = first.trim_start_matches(quote).trim();
            let content = content.trim_end_matches(quote).trim();
            if !content.is_empty() {
                Some(content.to_owned())
            } else {
                lines
                    .iter()
                    .skip_while(|line| line.trim().is_empty())
                    .nth(1)
                    .map(|line| line.trim().to_owned())
                    .filter(|doc| !doc.is_empty())
            }
        }
        "ts" | "tsx" | "js" | "jsx" | "mts" | "cts" | "mjs" | "cjs" => {
            let first = lines.iter().find(|line| !line.trim().is_empty())?.trim();
            if let Some(comment) = first.strip_prefix("//") {
                return Some(comment.trim().to_owned()).filter(|s| !s.is_empty());
            }
            if let Some(comment) = first.strip_prefix("/**") {
                let line = comment
                    .trim()
                    .trim_end_matches("*/")
                    .trim()
                    .trim_start_matches('*')
                    .trim();
                let line = if line.is_empty() {
                    lines
                        .iter()
                        .skip(1)
                        .map(|line| line.trim().trim_start_matches('*').trim())
                        .find(|line| !line.is_empty())?
                } else {
                    line
                };
                return Some(line.to_owned());
            }
            None
        }
        _ => None,
    }
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

/// One caller or callee path and its location; `name` is `file#Owner/name` when resolved.
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
    /// Definition address: `file#symbol (lines a–b)`; read the body separately with `ide.read`.
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

    #[test]
    fn directory_outline_lists_docs_and_counts_child_files() {
        let root = std::env::temp_dir().join(format!(
            "outline-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("src/sub")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "//! Rust module docs\nfn a() {}\n").unwrap();
        std::fs::write(root.join("src/sub/x"), "x\n").unwrap();
        let text = directory_outline(&root, Path::new("src")).unwrap();
        assert_eq!(
            text,
            "src/  (1 files, 1 dirs)\n  dirs: sub/ 1\n  lib.rs                   2  Rust module docs\n"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn first_file_doc_extracts_rust_python_and_typescript_comments() {
        assert_eq!(
            first_file_doc(Path::new("a.rs"), b"//! Rust docs\nfn a() {}"),
            Some("Rust docs".into())
        );
        assert_eq!(
            first_file_doc(Path::new("a.py"), b"\"\"\"Python docs\"\"\"\n"),
            Some("Python docs".into())
        );
        assert_eq!(
            first_file_doc(Path::new("a.ts"), b"/** TS docs */\nexport {}"),
            Some("TS docs".into())
        );
    }

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

    /// The history section prints only when the card carries parsed commit entries, one
    /// two-space-indented `sha date subject` line each.
    #[test]
    fn symbol_card_prints_history_entries_only_when_present() {
        let mut card = SymbolCard {
            heading: "value — fn, src/lib.rs#value (lines 1–1)".into(),
            signature: Some("pub fn value() -> i32".into()),
            ..Default::default()
        };
        assert!(
            !symbol_card_text(&card).contains("history:"),
            "{}",
            symbol_card_text(&card)
        );
        card.history = vec![
            "59482ce 2026-09-26 tune value bound".into(),
            "6290c82 2026-09-26 cross-crate fixture".into(),
        ];
        assert!(
            symbol_card_text(&card).contains(
                "history: 2 last commits touching the definition\n  \
                 59482ce 2026-09-26 tune value bound\n  \
                 6290c82 2026-09-26 cross-crate fixture\n"
            ),
            "{}",
            symbol_card_text(&card)
        );
    }

    #[test]
    fn symbol_card_lists_usages_with_ceiling_and_groups() {
        let mut card = SymbolCard {
            heading: "Run — fn, a.rs#Run (lines 1–3)".into(),
            signature: Some("pub fn run()".into()),
            doc: Some("Runs.".into()),
            definition: Some("a.rs#Run  (lines 1–3)".into()),
            callers: vec![Call {
                name: "a.rs#Owner/caller".into(),
                file: "a.rs".into(),
                line: 8,
            }],
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
        assert!(text.starts_with("symbol: Run — fn, a.rs#Run (lines 1–3)\nsignature: pub fn run()\ndoc: Runs.\ndefinition a.rs#Run  (lines 1–3)\nusages: 35 in 3 files (src 28, tests 7)\n"));
        assert!(text.contains("… 5 more (ide.inspect sym-1)"));
        assert!(text.contains("callers: 1\n  a.rs#Owner/caller  a.rs:8\n"));
        assert_eq!(text.matches("run();").count(), MAX_USAGE_LINES);
    }
}
