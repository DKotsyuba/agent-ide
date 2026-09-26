//! Rust language support (v0.4): Cargo project detection, rust-analyzer symbol normalization,
//! insertion sites, `cargo test` selection, libtest output parsing and `rustfmt`.
//!
//! # Headers
//!
//! A symbol's header is the contiguous block of lines directly above its declaration made of
//! `///` doc comments and outer `#[...]` attributes (single- or multi-line). The block stops at a
//! blank line, a plain `//` comment, a `//!` inner doc (which documents the enclosing module, never
//! the item below it), an inner `#![...]` attribute or any other code. rust-analyzer normally
//! reports item ranges that already include the header (and sometimes directly attached plain
//! comments); normalization recomputes the header from the source so the range is the same
//! whether or not the server included it, and keeps any extra lines the server did include.
//!
//! # Impl naming rule
//!
//! rust-analyzer names impl blocks `impl Foo` and `impl Trait for Foo`. An inherent impl
//! (`impl Foo`, `impl<T> Foo<T>`, `impl crate::a::Foo`) is named by its bare type name (`Foo`) so
//! that `Foo/new` addresses a method and `Foo` alone still finds the type; [`Outline::find`]
//! backtracks across same-named siblings (the struct `Foo` and one or more `impl Foo` blocks). A
//! trait impl keeps the server's full name (`impl Display for Foo`) as its segment, so trait
//! methods are addressed as `impl Display for Foo/fmt`.
//!
//! The helpers marked `pub(super)` are shared with the Go module, whose line-oriented rules are
//! the same apart from the comment syntax.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use async_lsp::lsp_types as lsp;

use super::{
    CommandSource, InsertSite, InsertWhere, LangError, Language, LanguageProject, LanguageSupport,
    LineRange, Outline, ProjectCommand, ProjectCommands, Symbol, SymbolKind, SymbolPath,
    TestFailure, TestId, TestReport, TestSelection, TestTarget, kind_of, line_count, lines_of,
};

/// Longest signature kept on a symbol card, in characters (the ellipsis included).
const SIGNATURE_LIMIT: usize = 200;

/// More referencing tests than this are selected by their common module prefix instead of by name.
const MAX_NAMED_TESTS: usize = 8;

/// A failure's optional `file:line` location and its first message line, as parsed from output.
pub(super) type Located = (Option<(PathBuf, u32)>, String);

/// Stateless Rust implementation of [`LanguageSupport`].
#[derive(Clone, Copy, Debug, Default)]
pub struct RustSupport;

impl LanguageSupport for RustSupport {
    /// Always [`Language::Rust`].
    fn language(&self) -> Language {
        Language::Rust
    }

    /// Detects a Cargo project (package or workspace) by `root/Cargo.toml`.
    ///
    /// Environment facts: `toolchain` from `rust-toolchain.toml`/`rust-toolchain` (read, never
    /// resolved through rustup), `edition` from the first `edition = "…"` key in `Cargo.toml`
    /// (package or `[workspace.package]`), `rustfmt` naming a rustfmt config file when present.
    /// Commands come per slot from, in order of preference, a single-command `run:` line in
    /// `.github/workflows/*.y*ml` ([`CommandSource::Ci`]), a `build`/`check`/`test`/`lint`/`fmt`
    /// target in `Makefile` or `justfile` ([`CommandSource::Makefile`]), then the Cargo defaults
    /// ([`CommandSource::Manifest`]). There is no typecheck command. Entry points are the existing
    /// `src/main.rs`, `src/lib.rs` and `src/bin/*.rs`. Unreadable files are treated as absent.
    fn detect(&self, root: &Path) -> Option<LanguageProject> {
        let manifest = fs::read_to_string(root.join("Cargo.toml")).ok()?;
        let mut environment = Vec::new();
        if let Some(toolchain) = toolchain(root) {
            environment.push(("toolchain".to_owned(), toolchain));
        }
        if let Some(edition) = toml_string(&manifest, "edition") {
            environment.push(("edition".to_owned(), edition));
        }
        if let Some(config) = ["rustfmt.toml", ".rustfmt.toml"]
            .into_iter()
            .find(|name| root.join(name).is_file())
        {
            environment.push(("rustfmt".to_owned(), config.to_owned()));
        }

        let mut commands = ProjectCommands::default();
        for argv in ci_cargo_commands(root) {
            let slot = match argv
                .iter()
                .find(|arg| !arg.starts_with('+') && *arg != "cargo")
            {
                Some(sub) => match sub.as_str() {
                    "build" => "build",
                    "check" => "check",
                    "test" | "nextest" => "test",
                    "clippy" => "lint",
                    "fmt" => "format",
                    _ => continue,
                },
                None => continue,
            };
            fill(&mut commands, slot, argv, CommandSource::Ci);
        }
        for (target, program) in task_targets(root) {
            let slot = if target == "fmt" { "format" } else { target };
            let argv = vec![program.to_owned(), target.to_owned()];
            fill(&mut commands, slot, argv, CommandSource::Makefile);
        }
        for (slot, text) in [
            ("build", "cargo build"),
            ("check", "cargo check --workspace --all-targets"),
            ("test", "cargo test --workspace"),
            (
                "lint",
                "cargo clippy --workspace --all-targets -- -D warnings",
            ),
            ("format", "cargo fmt --all"),
        ] {
            fill(&mut commands, slot, argv_of(text), CommandSource::Manifest);
        }

        let mut entry_points: Vec<PathBuf> = ["src/main.rs", "src/lib.rs"]
            .into_iter()
            .map(PathBuf::from)
            .filter(|path| root.join(path).is_file())
            .collect();
        if let Ok(entries) = fs::read_dir(root.join("src/bin")) {
            let mut bins: Vec<PathBuf> = entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
                .filter_map(|path| path.file_name().map(|name| Path::new("src/bin").join(name)))
                .collect();
            bins.sort();
            entry_points.extend(bins);
        }

        Some(LanguageProject {
            language: Language::Rust,
            manifests: vec![PathBuf::from("Cargo.toml")],
            environment,
            interpreter: None,
            commands,
            entry_points,
        })
    }

    /// Normalizes rust-analyzer document symbols (see the module docs for headers and impl names).
    ///
    /// Kinds: `INTERFACE` (rust-analyzer's trait) becomes [`SymbolKind::Trait`], functions inside
    /// an impl or trait become [`SymbolKind::Method`], functions with a `#[test]`-like attribute
    /// (`test`, `tokio::test`, … — any attribute path ending in `test`) and modules named `tests`
    /// or carrying `#[cfg(test)]` become [`SymbolKind::Test`]; a test module's members keep their
    /// own kinds. `body` starts at the declaration line after the header.
    fn normalize(&self, file: &Path, source: &str, symbols: Vec<lsp::DocumentSymbol>) -> Outline {
        let lines = source_lines(source);
        let root = SymbolPath::new(Some(file.to_path_buf()), Vec::new());
        Outline {
            file: file.to_path_buf(),
            language: Language::Rust,
            line_count: line_count(source),
            symbols: symbols
                .into_iter()
                .map(|symbol| convert(&lines, symbol, &root, None))
                .collect(),
        }
    }

    /// Computes the insertion point; see `place` for the shared rules. A container is any kind
    /// that [`SymbolKind::is_container`] accepts plus a test module (`mod tests`), which is
    /// [`SymbolKind::Test`]; members of an empty container are indented four spaces deeper.
    fn insert_site(
        &self,
        source: &str,
        outline: &Outline,
        anchor: &SymbolPath,
        where_: InsertWhere,
    ) -> Result<InsertSite, LangError> {
        place(source, outline, anchor, where_, "    ", |symbol| {
            symbol.kind.is_container()
                || (symbol.kind == SymbolKind::Test && is_mod_signature(&symbol.signature))
        })
    }

    /// `.rs` files under a `tests` directory or named `*_test.rs`; nothing under `benches`.
    /// Expects a project-relative path: every component is examined.
    fn is_test_file(&self, file: &Path) -> bool {
        if file.extension().is_none_or(|ext| ext != "rs") {
            return false;
        }
        let parts: Vec<&str> = file.iter().filter_map(|part| part.to_str()).collect();
        let dirs = &parts[..parts.len().saturating_sub(1)];
        if dirs.contains(&"benches") {
            return false;
        }
        dirs.contains(&"tests")
            || file
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| stem.ends_with("_test"))
    }

    /// Builds the `cargo test` command, run from the project root.
    ///
    /// * Symbol: one or more referencing test names as filters (`cargo test --workspace name`,
    ///   several after `--`); more than eight fall back to their common module prefix
    ///   (`a::b::`), or to the unfiltered workspace run when they share none. No referencing
    ///   tests is [`LangError::Unsupported`].
    /// * File: `tests/<name>.rs` or `tests/<name>/…` → `cargo test --test <name>`;
    ///   `src/lib.rs` → `--lib`, `src/main.rs` → `--bins`, `src/bin/<x>.rs` → `--bin <x>`, any
    ///   other `src` file → its module path filter (`src/a/b.rs` → `a::b::`, `mod.rs` dropped),
    ///   all with `--workspace`. Other files are [`LangError::Unsupported`].
    /// * Pattern: `cargo test --workspace <pattern>`.
    ///
    /// Only the symbol form lists the tests it expects to cover.
    fn test_selection(
        &self,
        _project: &LanguageProject,
        target: &TestTarget,
    ) -> Result<TestSelection, LangError> {
        let mut command = argv_of("cargo test --workspace");
        match target {
            TestTarget::Symbol {
                path,
                referencing_tests,
            } => {
                let tests = dedup_tests(referencing_tests);
                if tests.is_empty() {
                    return Err(LangError::Unsupported(format!("no tests reference {path}")));
                }
                if tests.len() > MAX_NAMED_TESTS {
                    let prefix = common_module_prefix(tests.iter().map(|test| test.name.as_str()));
                    if !prefix.is_empty() {
                        command.push(format!("{prefix}::"));
                    }
                } else {
                    if tests.len() > 1 {
                        command.push("--".to_owned());
                    }
                    command.extend(tests.iter().map(|test| test.name.clone()));
                }
                Ok(TestSelection { tests, command })
            }
            TestTarget::File(file) => {
                let parts: Vec<&str> = file.iter().filter_map(|part| part.to_str()).collect();
                let stem = |part: &str| part.strip_suffix(".rs").unwrap_or(part).to_owned();
                if let Some(at) = parts.iter().rposition(|part| *part == "tests")
                    && at + 1 < parts.len()
                {
                    command = vec![
                        "cargo".to_owned(),
                        "test".to_owned(),
                        "--test".to_owned(),
                        stem(parts[at + 1]),
                    ];
                } else if let Some(at) = parts.iter().rposition(|part| *part == "src") {
                    match &parts[at + 1..] {
                        ["lib.rs"] => command.push("--lib".to_owned()),
                        ["main.rs"] => command.push("--bins".to_owned()),
                        ["bin", name] => command.extend(["--bin".to_owned(), stem(name)]),
                        [] => return Err(LangError::Unsupported(file.display().to_string())),
                        modules => {
                            let mut segments: Vec<String> =
                                modules.iter().map(|part| stem(part)).collect();
                            if segments.last().is_some_and(|last| last == "mod") {
                                segments.pop();
                            }
                            command.push(format!("{}::", segments.join("::")));
                        }
                    }
                } else {
                    return Err(LangError::Unsupported(format!(
                        "{} is neither under src/ nor tests/",
                        file.display()
                    )));
                }
                Ok(TestSelection {
                    tests: Vec::new(),
                    command,
                })
            }
            TestTarget::Pattern(pattern) => {
                command.push(pattern.clone());
                Ok(TestSelection {
                    tests: Vec::new(),
                    command,
                })
            }
        }
    }

    /// Parses libtest output (stdout, then stderr).
    ///
    /// Counts are the sum of every `test result:` summary; a target whose `running N tests`
    /// header has no summary (crash, signal, budget) contributes its `test … ok|FAILED|ignored`
    /// lines instead and makes the report `incomplete`, as does output with no summary at all
    /// (e.g. a compile error). Failures follow the order of the `FAILED` lines; each takes its
    /// location and message from the `---- <name> stdout ----` block: the first
    /// `panicked at <file>:<line>:<col>:` line and the line after it, or else the block's first
    /// non-empty line. Only the post-1.73 panic format carries a location.
    fn parse_test_output(&self, stdout: &str, stderr: &str) -> TestReport {
        let text = format!("{stdout}\n{stderr}");
        let lines: Vec<&str> = text.lines().collect();
        let mut report = TestReport::default();
        let mut open: Option<[u32; 3]> = None;
        let mut summaries = 0;
        let mut failed_names = Vec::new();
        let mut blocks: HashMap<&str, Located> = HashMap::new();
        let mut block_order = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if let Some(summary) = line.strip_prefix("test result: ") {
                report.passed += count_before(summary, "passed");
                report.failed += count_before(summary, "failed");
                report.ignored += count_before(summary, "ignored");
                summaries += 1;
                open = None;
            } else if line.starts_with("running ")
                && (line.ends_with(" tests") || line.ends_with(" test"))
            {
                if let Some(counts) = open.replace([0; 3]) {
                    add_counts(&mut report, counts);
                }
            } else if let Some((name, outcome)) = line
                .strip_prefix("test ")
                .and_then(|rest| rest.rsplit_once(" ... "))
            {
                let counts = open.get_or_insert([0; 3]);
                if outcome == "ok" {
                    counts[0] += 1;
                } else if outcome == "FAILED" {
                    counts[1] += 1;
                    failed_names.push(name.to_owned());
                } else if outcome.starts_with("ignored") {
                    counts[2] += 1;
                }
            } else if let Some(name) = line
                .strip_prefix("---- ")
                .and_then(|rest| rest.strip_suffix(" stdout ----"))
            {
                let block: Vec<&str> = lines[index + 1..]
                    .iter()
                    .copied()
                    .take_while(|line| !line.starts_with("---- ") && *line != "failures:")
                    .collect();
                blocks.insert(name, panic_of(&block));
                block_order.push(name);
            }
        }
        if let Some(counts) = open {
            add_counts(&mut report, counts);
            report.incomplete = true;
        }
        if summaries == 0 {
            report.incomplete = true;
        }
        for name in block_order {
            if !failed_names.iter().any(|failed| failed == name) {
                failed_names.push(name.to_owned());
            }
        }
        report.failures = failed_names
            .into_iter()
            .map(|name| {
                let (location, message) = blocks.remove(name.as_str()).unwrap_or_default();
                TestFailure {
                    name,
                    location,
                    message,
                }
            })
            .collect();
        report
    }

    /// `rustfmt --edition <edition> <file>` for a Cargo project (or one with a rustfmt config);
    /// the edition is the detected `edition` fact, 2021 when the manifest names none. rustfmt
    /// itself picks up `rustfmt.toml` from the file's ancestors.
    fn format_command(&self, project: &LanguageProject, file: &Path) -> Option<Vec<String>> {
        let edition = rustfmt_edition(project)?;
        Some(vec![
            "rustfmt".to_owned(),
            "--edition".to_owned(),
            edition,
            file.display().to_string(),
        ])
    }

    /// `rustfmt --edition <edition>` with no file argument, so rustfmt formats the stdin text and
    /// writes it to stdout; same detection and edition as [`Self::format_command`]. `None` for a
    /// non-`.rs` file or a project without rustfmt.
    fn format_stdin_command(&self, project: &LanguageProject, file: &Path) -> Option<Vec<String>> {
        if file.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            return None;
        }
        let edition = rustfmt_edition(project)?;
        Some(vec!["rustfmt".to_owned(), "--edition".to_owned(), edition])
    }
}

/// Detected rustfmt edition for a Cargo project (or one with a rustfmt config); `None` when
/// neither exists, so the project has no rustfmt command.
fn rustfmt_edition(project: &LanguageProject) -> Option<String> {
    let cargo = project.manifests.iter().any(|manifest| {
        manifest
            .file_name()
            .is_some_and(|name| name == "Cargo.toml")
    });
    let fact = |name: &str| {
        project
            .environment
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    if !cargo && fact("rustfmt").is_none() {
        return None;
    }
    Some(fact("edition").unwrap_or_else(|| "2021".to_owned()))
}

/// Converts one rust-analyzer symbol (and its children) under `owner`, whose kind is `owner_kind`
/// (`None` at file level). Line numbers outside `lines` read as empty lines, never panic.
fn convert(
    lines: &[&str],
    symbol: lsp::DocumentSymbol,
    owner: &SymbolPath,
    owner_kind: Option<SymbolKind>,
) -> Symbol {
    let reported = lines_of(&symbol.range);
    let name_line = (symbol.selection_range.start.line + 1).max(reported.start);
    let decl = declaration_line(lines, reported.start, name_line, true);
    let start = header_start(lines, decl).min(reported.start);
    let header: Vec<&str> = (start..decl)
        .map(|line| line_at(lines, line).trim())
        .collect();

    let server_kind = if symbol.kind == lsp::SymbolKind::INTERFACE {
        SymbolKind::Trait
    } else {
        kind_of(symbol.kind)
    };
    let kind = match server_kind {
        SymbolKind::Function | SymbolKind::Method if header.iter().any(|l| is_test_attr(l)) => {
            SymbolKind::Test
        }
        SymbolKind::Function | SymbolKind::Method
            if matches!(owner_kind, Some(SymbolKind::Impl | SymbolKind::Trait)) =>
        {
            SymbolKind::Method
        }
        SymbolKind::Function | SymbolKind::Method => SymbolKind::Function,
        SymbolKind::Module
            if symbol.name == "tests" || header.iter().any(|line| is_cfg_test(line)) =>
        {
            SymbolKind::Test
        }
        other => other,
    };
    let name = if server_kind == SymbolKind::Impl {
        impl_segment(&symbol.name)
    } else {
        symbol.name
    };
    let path = owner.child(&name);
    let body = LineRange::new(decl, reported.end);
    let children = symbol
        .children
        .unwrap_or_default()
        .into_iter()
        .map(|child| convert(lines, child, &path, Some(server_kind)))
        .collect();
    Symbol {
        signature: signature(lines, body, true),
        doc: first_paragraph(header.iter().filter_map(|line| {
            line.strip_prefix("///")
                .filter(|_| !line.starts_with("////"))
        })),
        path,
        kind,
        name,
        range: LineRange::new(start, reported.end),
        body,
        children,
    }
}

/// Path segment of an impl block named `impl …` by the server: the bare type name for an inherent
/// impl (generic parameters, generic arguments and the module path dropped), the whitespace-
/// normalized name unchanged for a trait impl (`… for …`) or a name that is not an impl.
fn impl_segment(name: &str) -> String {
    let name = collapse_whitespace(name);
    let Some(rest) = name.strip_prefix("impl") else {
        return name;
    };
    if name.contains(" for ") {
        return name;
    }
    let mut rest = rest.trim_start();
    if rest.starts_with('<') {
        rest = skip_balanced(rest, '<', '>').trim_start();
    }
    let ty = rest.split('<').next().unwrap_or(rest).trim();
    let ty = ty.rsplit("::").next().unwrap_or(ty).trim();
    if ty.is_empty() { name } else { ty.to_owned() }
}

/// `text` after its leading balanced `open … close` group (the whole text when unbalanced).
fn skip_balanced(text: &str, open: char, close: char) -> &str {
    let mut depth = 0usize;
    for (index, ch) in text.char_indices() {
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return &text[index + ch.len_utf8()..];
            }
        }
    }
    ""
}

/// Whether a trimmed header line is an attribute whose path ends in `test` (`#[test]`,
/// `#[tokio::test(flavor = "multi_thread")]`).
fn is_test_attr(line: &str) -> bool {
    attr_path(line).is_some_and(|path| path == "test" || path.ends_with("::test"))
}

/// Whether a trimmed header line is a `#[cfg(…)]` naming `test` (not under `not(…)`).
fn is_cfg_test(line: &str) -> bool {
    attr_path(line) == Some("cfg")
        && !line.contains("not(")
        && line
            .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
            .skip(2)
            .any(|token| token == "test")
}

/// The path of an outer attribute line (`#[tokio::test(…)]` → `tokio::test`), `None` otherwise.
fn attr_path(line: &str) -> Option<&str> {
    let inner = line.strip_prefix("#[")?;
    let end = inner.find(['(', ']', ' ', '=']).unwrap_or(inner.len());
    Some(inner[..end].trim())
}

/// Whether a normalized signature declares a module (`mod x`, `pub(crate) mod x`).
fn is_mod_signature(signature: &str) -> bool {
    signature.split_whitespace().any(|word| word == "mod")
}

/// First line of the Rust header above the declaration on 1-based line `decl` (`decl` itself when
/// there is none): `///` lines and outer attributes, see the module docs.
fn header_start(lines: &[&str], decl: u32) -> u32 {
    let mut start = decl;
    let mut line = decl.saturating_sub(1);
    while line >= 1 {
        let text = line_at(lines, line).trim();
        if text.starts_with("///") {
            start = line;
            line -= 1;
        } else if text.ends_with(']')
            && let Some(open) = attribute_start(lines, line)
        {
            start = open;
            line = open - 1;
        } else {
            break;
        }
    }
    start
}

/// First line of the outer attribute ending on 1-based line `end`, scanning upward while the
/// square brackets stay unbalanced; `None` when the lines are not one attribute. Brackets inside
/// string literals are counted too (ponytail: rare in attributes; tokenize if it ever matters).
fn attribute_start(lines: &[&str], end: u32) -> Option<u32> {
    let mut depth = 0i32;
    let mut line = end;
    while line >= 1 && end - line < 64 {
        let text = line_at(lines, line).trim();
        if text.is_empty() {
            return None;
        }
        depth += text.matches(']').count() as i32 - text.matches('[').count() as i32;
        if depth == 0 {
            return text.starts_with("#[").then_some(line);
        }
        if depth < 0 {
            return None;
        }
        line -= 1;
    }
    None
}

/// First declaration line in `from..limit` (1-based): skips blank lines, `//` comments and (when
/// `attributes`) possibly multi-line `#[...]` attributes that a server range may start with.
/// Returns `limit` (the name line) when every line before it is header.
pub(super) fn declaration_line(lines: &[&str], from: u32, limit: u32, attributes: bool) -> u32 {
    let mut line = from;
    let mut depth = 0i32;
    while line < limit {
        let text = line_at(lines, line).trim();
        let brackets = text.matches('[').count() as i32 - text.matches(']').count() as i32;
        if depth > 0 {
            depth += brackets;
        } else if text.is_empty() || text.starts_with("//") {
        } else if attributes && text.starts_with("#[") {
            depth = brackets;
        } else {
            break;
        }
        line += 1;
    }
    line
}

/// The declaration in `body` collapsed to one line.
///
/// Rust (`item_ends`): stops before the `{` that opens the body or the `;`/`=` that ends the item,
/// at bracket depth 0 (`()`, `[]`, `<>`; `->` is not a bracket). Go: stops before the first
/// depth-0 `{` that is not an empty `{}` type literal, and keeps only the first line when there
/// is none. Line comments are dropped, whitespace collapsed, padding inside brackets and trailing
/// commas removed, and the result cut to `SIGNATURE_LIMIT` characters with `…`.
pub(super) fn signature(lines: &[&str], body: LineRange, item_ends: bool) -> String {
    let joined: Vec<char> = (body.start..=body.end)
        .map(|line| strip_line_comment(line_at(lines, line)))
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .collect();
    let mut depth = 0i32;
    let mut end = None;
    let mut index = 0;
    while index < joined.len() {
        let ch = joined[index];
        let prev = index.checked_sub(1).map(|at| joined[at]);
        let next = joined.get(index + 1).copied();
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            '<' if item_ends => depth += 1,
            '>' if item_ends && prev != Some('-') && prev != Some('=') => depth -= 1,
            '{' if depth <= 0 => {
                let rest = joined[index + 1..].iter().find(|ch| !ch.is_whitespace());
                if item_ends || rest != Some(&'}') {
                    end = Some(index);
                    break;
                }
            }
            ';' if item_ends && depth <= 0 => {
                end = Some(index);
                break;
            }
            '=' if item_ends
                && depth <= 0
                && !matches!(next, Some('=' | '>'))
                && !matches!(prev, Some('=' | '<' | '>' | '!')) =>
            {
                end = Some(index);
                break;
            }
            _ => {}
        }
        index += 1;
    }
    let text: String = match end {
        Some(end) => joined[..end].iter().collect(),
        None if item_ends => joined.iter().collect(),
        None => strip_line_comment(line_at(lines, body.start)).to_owned(),
    };
    finish_signature(&text)
}

/// Whitespace-normalizes a collected declaration and bounds it to `SIGNATURE_LIMIT` characters.
fn finish_signature(text: &str) -> String {
    let mut text = collapse_whitespace(text);
    for (from, to) in [
        ("( ", "("),
        (" )", ")"),
        (",)", ")"),
        ("< ", "<"),
        (" >", ">"),
        (",>", ">"),
    ] {
        text = text.replace(from, to);
    }
    let text = text.trim_end_matches(',').trim_end();
    if text.chars().count() <= SIGNATURE_LIMIT {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(SIGNATURE_LIMIT - 1).collect();
    cut.push('…');
    cut
}

/// Collapses every whitespace run to one space and trims the ends.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `line` without a trailing `//` comment; `//` inside a `"…"` or `` `…` `` literal is kept.
pub(super) fn strip_line_comment(line: &str) -> &str {
    let mut quote = None;
    let mut previous = '\0';
    for (index, ch) in line.char_indices() {
        match quote {
            Some(open) if ch == open && previous != '\\' => quote = None,
            Some(_) => {}
            None if ch == '"' || ch == '`' => quote = Some(ch),
            None if ch == '/' && previous == '/' => return &line[..index - 1],
            None => {}
        }
        previous = if previous == '\\' && ch == '\\' {
            '\0'
        } else {
            ch
        };
    }
    line
}

/// First paragraph of doc lines whose comment markers are already removed: one optional leading
/// space is dropped per line, leading empty lines skipped, and the paragraph ends at the next empty
/// line; lines are joined by single spaces. `None` when there is no text.
pub(super) fn first_paragraph<'a>(docs: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut paragraph: Vec<&str> = Vec::new();
    for line in docs {
        let line = line.strip_prefix(' ').unwrap_or(line).trim_end();
        if line.trim().is_empty() {
            if paragraph.is_empty() {
                continue;
            }
            break;
        }
        paragraph.push(line.trim());
    }
    (!paragraph.is_empty()).then(|| paragraph.join(" "))
}

/// Lines of `source` without terminators (`\r\n` handled).
pub(super) fn source_lines(source: &str) -> Vec<&str> {
    source.lines().collect()
}

/// The 1-based line `line`, or `""` outside the source.
pub(super) fn line_at<'a>(lines: &[&'a str], line: u32) -> &'a str {
    line.checked_sub(1)
        .and_then(|index| lines.get(index as usize))
        .copied()
        .unwrap_or("")
}

/// Leading whitespace of the 1-based line `line`.
fn indent_at(lines: &[&str], line: u32) -> String {
    let text = line_at(lines, line);
    text[..text.len() - text.trim_start().len()].to_owned()
}

/// Whether the 1-based line `line` is blank or past the end of the source.
fn blank_or_eof(lines: &[&str], line: u32) -> bool {
    line_at(lines, line).trim().is_empty()
}

/// Shared insertion rules for brace languages.
///
/// * `Before`: at the anchor's header start, anchor indentation, one blank line before unless the
///   line above is blank (or the anchor starts the file), one after.
/// * `After`: the line after the anchor's end, anchor indentation, one blank before, one after
///   unless the next line is blank or the end of file.
/// * `First`/`Last` on a container (`is_container`): after the line holding its opening `{`
///   (skipping `//!` and `#![...]` inner lines) resp. after its last member (or before the
///   closing line when empty); indentation of the first member, else the container's own plus
///   `unit`. A container without a `{` but with members (Go's synthetic method sets) places
///   `First` at its first member and `Last` like `After` its last member.
///
/// Errors: [`LangError::UnknownSymbol`] when `anchor` is not in `outline`,
/// [`LangError::NotAContainer`] for `First`/`Last` on a non-container or a body-less container
/// (`mod x;`), [`LangError::Unparseable`] when the container's braces share one line.
pub(super) fn place(
    source: &str,
    outline: &Outline,
    anchor: &SymbolPath,
    where_: InsertWhere,
    unit: &str,
    is_container: impl Fn(&Symbol) -> bool,
) -> Result<InsertSite, LangError> {
    let symbol = outline
        .find(anchor)
        .ok_or_else(|| LangError::UnknownSymbol(anchor.clone()))?;
    let lines = source_lines(source);
    let after = |end: u32, indent: String| InsertSite {
        line: end + 1,
        indent,
        blank_before: 1,
        blank_after: u8::from(!blank_or_eof(&lines, end + 1)),
    };
    match where_ {
        InsertWhere::Before => {
            let line = symbol.range.start;
            Ok(InsertSite {
                line,
                indent: indent_at(&lines, line),
                blank_before: u8::from(line > 1 && !blank_or_eof(&lines, line - 1)),
                blank_after: 1,
            })
        }
        InsertWhere::After => Ok(after(
            symbol.range.end,
            indent_at(&lines, symbol.range.start),
        )),
        InsertWhere::First | InsertWhere::Last => {
            if !is_container(symbol) {
                return Err(LangError::NotAContainer(anchor.clone()));
            }
            let first = symbol.children.first();
            let last = symbol.children.last();
            let search_end = first.map_or(symbol.body.end, |child| child.range.start - 1);
            let brace = (symbol.body.start..=search_end)
                .find(|line| strip_line_comment(line_at(&lines, *line)).contains('{'));
            let member_indent = match first {
                Some(child) => indent_at(&lines, child.range.start),
                None => format!("{}{unit}", indent_at(&lines, symbol.body.start)),
            };
            let Some(brace) = brace else {
                return match (where_, first, last) {
                    (InsertWhere::First, Some(child), _) => Ok(InsertSite {
                        line: child.range.start,
                        indent: member_indent,
                        blank_before: 0,
                        blank_after: 1,
                    }),
                    (_, _, Some(child)) => Ok(after(child.range.end, member_indent)),
                    _ => Err(LangError::NotAContainer(anchor.clone())),
                };
            };
            if brace >= symbol.body.end {
                return Err(LangError::Unparseable(format!(
                    "{anchor} opens and closes its body on one line"
                )));
            }
            if let (InsertWhere::Last, Some(child)) = (where_, last) {
                return Ok(InsertSite {
                    line: child.range.end + 1,
                    indent: member_indent,
                    blank_before: 1,
                    blank_after: 0,
                });
            }
            if where_ == InsertWhere::Last {
                return Ok(InsertSite {
                    line: symbol.body.end,
                    indent: member_indent,
                    blank_before: 0,
                    blank_after: 0,
                });
            }
            let mut line = brace + 1;
            while line < symbol.body.end && {
                let text = line_at(&lines, line).trim();
                text.starts_with("//!") || text.starts_with("#![")
            } {
                line += 1;
            }
            Ok(InsertSite {
                line,
                indent: member_indent,
                blank_before: 0,
                blank_after: u8::from(first.is_some()),
            })
        }
    }
}

/// `tests` with duplicate entries removed, first occurrence kept, order preserved.
pub(super) fn dedup_tests(tests: &[TestId]) -> Vec<TestId> {
    let mut unique: Vec<TestId> = Vec::new();
    for test in tests {
        if !unique.contains(test) {
            unique.push(test.clone());
        }
    }
    unique
}

/// Longest `::`-separated module prefix shared by every test name (each name's last segment, the
/// test function, is never part of it); empty when they share none.
fn common_module_prefix<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let mut prefix: Option<Vec<&str>> = None;
    for name in names {
        let segments: Vec<&str> = name.split("::").collect();
        let modules = &segments[..segments.len() - 1];
        prefix = Some(match prefix {
            None => modules.to_vec(),
            Some(current) => current
                .iter()
                .zip(modules)
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| *a)
                .collect(),
        });
    }
    prefix.unwrap_or_default().join("::")
}

/// Adds one unfinished target's `[passed, failed, ignored]` line counts to `report`.
fn add_counts(report: &mut TestReport, [passed, failed, ignored]: [u32; 3]) {
    report.passed += passed;
    report.failed += failed;
    report.ignored += ignored;
}

/// The number printed right before `label` in a libtest summary (`1 passed; 0 failed`), 0 if none.
fn count_before(summary: &str, label: &str) -> u32 {
    let words: Vec<&str> = summary
        .split(|ch: char| ch.is_whitespace() || ch == ';' || ch == '.')
        .filter(|word| !word.is_empty())
        .collect();
    words
        .windows(2)
        .find(|pair| pair[1] == label)
        .and_then(|pair| pair[0].parse().ok())
        .unwrap_or(0)
}

/// Location and message of one `---- name stdout ----` block's lines (see
/// [`RustSupport::parse_test_output`]).
fn panic_of(block: &[&str]) -> Located {
    for (index, line) in block.iter().enumerate() {
        let Some((_, at)) = line.split_once("panicked at ") else {
            continue;
        };
        let mut parts = at.trim_end().trim_end_matches(':').rsplitn(3, ':');
        let (_column, line_no, file) = (parts.next(), parts.next(), parts.next());
        let location = file
            .zip(line_no.and_then(|number| number.parse().ok()))
            .map(|(file, number)| (PathBuf::from(file), number));
        let message = block.get(index + 1).map_or("", |line| line.trim());
        return (location, message.to_owned());
    }
    let message = block
        .iter()
        .map(|line| line.trim())
        .find(|line| !line.is_empty())
        .unwrap_or("");
    (None, message.to_owned())
}

/// Stores `argv` in the `slot` command (`build`, `check`, `test`, `lint`, `format`) unless an
/// earlier, more trusted source already filled it.
fn fill(commands: &mut ProjectCommands, slot: &str, argv: Vec<String>, source: CommandSource) {
    let target = match slot {
        "build" => &mut commands.build,
        "check" => &mut commands.check,
        "test" => &mut commands.test,
        "lint" => &mut commands.lint,
        "format" => &mut commands.format,
        _ => return,
    };
    target.get_or_insert(ProjectCommand { argv, source });
}

/// Splits a command line on whitespace (no quoting: callers only pass quote-free commands).
pub(super) fn argv_of(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}

/// Toolchain named by `rust-toolchain.toml` (`channel = "…"`) or a legacy `rust-toolchain`
/// file (TOML or a bare channel line); `None` when neither exists.
fn toolchain(root: &Path) -> Option<String> {
    ["rust-toolchain.toml", "rust-toolchain"]
        .into_iter()
        .find_map(|name| fs::read_to_string(root.join(name)).ok())
        .and_then(|text| {
            toml_string(&text, "channel").or_else(|| {
                text.lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty() && !line.starts_with('#'))
                    .filter(|line| !line.contains('='))
                    .map(str::to_owned)
            })
        })
}

/// Value of the first `key = "value"` line in a TOML text (any table), without parsing TOML.
pub(super) fn toml_string(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        if name.trim() != key {
            return None;
        }
        let value = value.trim();
        let value = value.strip_prefix('"')?;
        Some(value[..value.find('"')?].to_owned())
    })
}

/// Cargo commands (argv) from `run:` steps of `.github/workflows/*.yml|yaml`, files in name order.
///
/// A line scanner, not a YAML parser: single-line `run: …` values and the lines of `run: |`/`>`
/// blocks are candidates; only commands starting with `cargo` and free of quotes, variables,
/// pipes, chains and redirections are kept, so each yields a plain argv.
fn ci_cargo_commands(root: &Path) -> Vec<Vec<String>> {
    let Ok(entries) = fs::read_dir(root.join(".github/workflows")) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext == "yml" || ext == "yaml")
        })
        .collect();
    files.sort();
    let mut commands = Vec::new();
    let mut keep = |command: &str| {
        let command = command.trim();
        if command.starts_with("cargo ")
            && !command.contains(['"', '\'', '$', '|', '&', ';', '>', '<', '`', '\\'])
        {
            commands.push(argv_of(command));
        }
    };
    for file in files {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        let mut block: Option<usize> = None;
        for line in text.lines() {
            let indent = line.len() - line.trim_start().len();
            let trimmed = line.trim();
            if let Some(block_indent) = block {
                if trimmed.is_empty() {
                    continue;
                }
                if indent > block_indent {
                    keep(trimmed);
                    continue;
                }
                block = None;
            }
            let step = trimmed.strip_prefix("- ").unwrap_or(trimmed).trim_start();
            if let Some(value) = step.strip_prefix("run:") {
                let value = value.trim();
                if value.starts_with('|') || value.starts_with('>') {
                    block = Some(indent);
                } else {
                    keep(value);
                }
            }
        }
    }
    commands
}

/// `(target, program)` pairs for the `build`/`check`/`test`/`lint`/`fmt` targets defined in
/// `Makefile` (`make`) and then `justfile`/`Justfile` (`just`): a line starting at column 0 with
/// the target name followed by `:` (and not `:=`), or for just a recipe with parameters.
fn task_targets(root: &Path) -> Vec<(&'static str, &'static str)> {
    let mut found = Vec::new();
    for (file, program) in [
        ("Makefile", "make"),
        ("justfile", "just"),
        ("Justfile", "just"),
    ] {
        let Ok(text) = fs::read_to_string(root.join(file)) else {
            continue;
        };
        for target in ["build", "check", "test", "lint", "fmt"] {
            let defined = text.lines().any(|line| {
                line.strip_prefix(target).is_some_and(|rest| {
                    let head = rest.split(':').next().unwrap_or(rest);
                    rest.contains(':')
                        && !rest.contains(":=")
                        && (head.is_empty() || (program == "just" && head.starts_with(' ')))
                })
            });
            if defined {
                found.push((target, program));
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a document symbol spanning 0-based lines `start..=end` with its name on `name_line`.
    #[allow(deprecated)]
    fn ds(
        name: &str,
        kind: lsp::SymbolKind,
        (start, end): (u32, u32),
        name_line: u32,
        children: Vec<lsp::DocumentSymbol>,
    ) -> lsp::DocumentSymbol {
        lsp::DocumentSymbol {
            name: name.to_owned(),
            detail: None,
            kind,
            tags: None,
            deprecated: None,
            range: lsp::Range::new(lsp::Position::new(start, 0), lsp::Position::new(end, 1)),
            selection_range: lsp::Range::new(
                lsp::Position::new(name_line, 4),
                lsp::Position::new(name_line, 8),
            ),
            children: (!children.is_empty()).then_some(children),
        }
    }

    /// Fixture source; rust-analyzer 1.98 reports the ranges used in [`symbols`].
    const SOURCE: &str = r#"//! Crate docs.

use std::fmt;

/// A guard.
///
/// More detail.
#[derive(Debug)]
pub struct Guard {
    pub id: u32,
}

impl Guard {
    /// Builds a guard.
    pub fn new(id: u32) -> Self {
        Self { id }
    }

    pub fn id(&self) -> u32 {
        self.id
    }
}

impl fmt::Display for Guard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.id)
    }
}

/// Maps values.
#[inline]
#[cfg_attr(
    feature = "trace",
    tracing::instrument
)]
pub fn map_all<T, U>(
    items: Vec<T>,
    f: impl Fn(T) -> U,
) -> Vec<U>
where
    T: Clone,
{
    items.into_iter().map(f).collect()
}

fn plain() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds() {
        assert_eq!(Guard::new(1).id(), 1);
    }
}
"#;

    /// Document symbols as rust-analyzer emits them for [`SOURCE`] (headers included), except
    /// `map_all`, whose start is given either with (`29`) or without (`35`) its header.
    fn symbols(map_all_start: u32) -> Vec<lsp::DocumentSymbol> {
        use lsp::SymbolKind as K;
        vec![
            ds(
                "Guard",
                K::STRUCT,
                (4, 10),
                8,
                vec![ds("id", K::FIELD, (9, 9), 9, vec![])],
            ),
            ds(
                "impl Guard",
                K::OBJECT,
                (12, 21),
                12,
                vec![
                    ds("new", K::FUNCTION, (13, 16), 14, vec![]),
                    ds("id", K::FUNCTION, (18, 20), 18, vec![]),
                ],
            ),
            ds(
                "impl fmt::Display for Guard",
                K::OBJECT,
                (23, 27),
                23,
                vec![ds("fmt", K::FUNCTION, (24, 26), 24, vec![])],
            ),
            ds("map_all", K::FUNCTION, (map_all_start, 43), 35, vec![]),
            ds("plain", K::FUNCTION, (45, 45), 45, vec![]),
            ds(
                "tests",
                K::MODULE,
                (47, 55),
                48,
                vec![ds("builds", K::FUNCTION, (51, 54), 52, vec![])],
            ),
        ]
    }

    fn outline(map_all_start: u32) -> Outline {
        RustSupport.normalize(Path::new("src/guard.rs"), SOURCE, symbols(map_all_start))
    }

    fn find<'a>(outline: &'a Outline, path: &str) -> &'a Symbol {
        outline
            .find(&SymbolPath::parse(path).unwrap())
            .unwrap_or_else(|| panic!("{path} not found"))
    }

    #[test]
    fn header_is_the_same_whether_or_not_the_server_included_it() {
        let with = outline(29);
        let without = outline(35);
        let map_all = find(&without, "map_all");
        assert_eq!(map_all.range, LineRange::new(30, 44));
        assert_eq!(map_all.body, LineRange::new(36, 44));
        assert_eq!(find(&with, "map_all"), map_all);
        assert_eq!(map_all.doc.as_deref(), Some("Maps values."));
        let plain = find(&without, "plain");
        assert_eq!(plain.range, LineRange::new(46, 46));
        assert_eq!(plain.doc, None);
        assert_eq!(plain.signature, "fn plain()");
    }

    #[test]
    fn struct_header_doc_and_fields() {
        let outline = outline(35);
        let guard = &outline.symbols[0];
        assert_eq!(guard.kind, SymbolKind::Struct);
        assert_eq!(guard.range, LineRange::new(5, 11));
        assert_eq!(guard.body, LineRange::new(9, 11));
        assert_eq!(guard.signature, "pub struct Guard");
        assert_eq!(guard.doc.as_deref(), Some("A guard."));
        assert_eq!(guard.children[0].signature, "pub id: u32");
        assert_eq!(guard.children[0].path.to_string(), "src/guard.rs#Guard/id");
    }

    #[test]
    fn multi_line_generic_signature_collapses() {
        assert_eq!(
            find(&outline(35), "map_all").signature,
            "pub fn map_all<T, U>(items: Vec<T>, f: impl Fn(T) -> U) -> Vec<U> where T: Clone"
        );
    }

    #[test]
    fn inherent_impl_is_named_by_type_and_trait_impl_keeps_its_name() {
        let outline = outline(35);
        let inherent = &outline.symbols[1];
        assert_eq!(inherent.kind, SymbolKind::Impl);
        assert_eq!(inherent.name, "Guard");
        assert_eq!(inherent.signature, "impl Guard");
        // `Guard` alone is the struct; `Guard/new` backtracks into the impl.
        assert_eq!(find(&outline, "Guard").kind, SymbolKind::Struct);
        let new = find(&outline, "Guard/new");
        assert_eq!(new.kind, SymbolKind::Method);
        assert_eq!(new.path.to_string(), "src/guard.rs#Guard/new");
        assert_eq!(new.range, LineRange::new(14, 17));
        assert_eq!(new.signature, "pub fn new(id: u32) -> Self");
        assert_eq!(new.doc.as_deref(), Some("Builds a guard."));
        let fmt = find(&outline, "impl fmt::Display for Guard/fmt");
        assert_eq!(fmt.kind, SymbolKind::Method);
        assert_eq!(impl_segment("impl<T: Clone> crate::a::Wrap<T>"), "Wrap");
        assert_eq!(
            impl_segment("impl<T> From<T> for Wrap<T>"),
            "impl<T> From<T> for Wrap<T>"
        );
    }

    #[test]
    fn test_module_and_test_functions_are_tests() {
        let outline = outline(35);
        let tests = find(&outline, "tests");
        assert_eq!(tests.kind, SymbolKind::Test);
        assert_eq!(tests.range, LineRange::new(48, 56));
        assert_eq!(find(&outline, "tests/builds").kind, SymbolKind::Test);
        assert_eq!(find(&outline, "plain").kind, SymbolKind::Function);
        assert!(is_test_attr("#[tokio::test(flavor = \"multi_thread\")]"));
        assert!(!is_cfg_test("#[cfg(not(test))]"));
    }

    #[test]
    fn insert_sites_for_all_positions() {
        let outline = outline(35);
        let site = |path: &str, where_| {
            RustSupport.insert_site(SOURCE, &outline, &SymbolPath::parse(path).unwrap(), where_)
        };
        let at = |line, indent: &str, blank_before, blank_after| InsertSite {
            line,
            indent: indent.to_owned(),
            blank_before,
            blank_after,
        };
        assert_eq!(site("plain", InsertWhere::Before), Ok(at(46, "", 0, 1)));
        assert_eq!(
            site("Guard/new", InsertWhere::Before),
            Ok(at(14, "    ", 1, 1))
        );
        assert_eq!(site("Guard", InsertWhere::After), Ok(at(12, "", 1, 0)));
        assert_eq!(
            site("Guard/new", InsertWhere::After),
            Ok(at(18, "    ", 1, 0))
        );
        // `Guard/id` would be the struct field: same-named siblings resolve in source order.
        assert_eq!(
            site("impl fmt::Display for Guard/fmt", InsertWhere::After),
            Ok(at(28, "    ", 1, 1))
        );
        // `Guard` resolves to the struct: its first member goes after `{`.
        assert_eq!(site("Guard", InsertWhere::First), Ok(at(10, "    ", 0, 1)));
        assert_eq!(site("Guard", InsertWhere::Last), Ok(at(11, "    ", 1, 0)));
        assert_eq!(site("tests", InsertWhere::First), Ok(at(50, "    ", 0, 1)));
        assert_eq!(site("tests", InsertWhere::Last), Ok(at(56, "    ", 1, 0)));
        assert_eq!(
            site("impl fmt::Display for Guard", InsertWhere::Last),
            Ok(at(28, "    ", 1, 0))
        );
        let plain = SymbolPath::parse("plain").unwrap();
        assert_eq!(
            site("plain", InsertWhere::First),
            Err(LangError::NotAContainer(plain))
        );
        let missing = SymbolPath::parse("Nope/x").unwrap();
        assert_eq!(
            site("Nope/x", InsertWhere::After),
            Err(LangError::UnknownSymbol(missing))
        );
    }

    #[test]
    fn empty_container_inserts_before_its_closing_line() {
        let source = "impl Empty {\n}\n";
        let outline = RustSupport.normalize(
            Path::new("a.rs"),
            source,
            vec![ds("impl Empty", lsp::SymbolKind::OBJECT, (0, 1), 0, vec![])],
        );
        let path = SymbolPath::parse("Empty").unwrap();
        let expected = InsertSite {
            line: 2,
            indent: "    ".to_owned(),
            blank_before: 0,
            blank_after: 0,
        };
        for where_ in [InsertWhere::First, InsertWhere::Last] {
            assert_eq!(
                RustSupport.insert_site(source, &outline, &path, where_),
                Ok(expected.clone())
            );
        }
    }

    #[test]
    fn test_files_follow_cargo_layout() {
        for (path, expected) in [
            ("tests/it.rs", true),
            ("tests/common/mod.rs", true),
            ("crates/a/tests/x.rs", true),
            ("src/worker_test.rs", true),
            ("src/worker.rs", false),
            ("benches/tests/b.rs", false),
            ("tests/data.json", false),
        ] {
            assert_eq!(
                RustSupport.is_test_file(Path::new(path)),
                expected,
                "{path}"
            );
        }
    }

    fn project() -> LanguageProject {
        LanguageProject {
            language: Language::Rust,
            manifests: vec![PathBuf::from("Cargo.toml")],
            environment: vec![("edition".to_owned(), "2024".to_owned())],
            interpreter: None,
            commands: ProjectCommands::default(),
            entry_points: Vec::new(),
        }
    }

    fn command(target: TestTarget) -> String {
        RustSupport
            .test_selection(&project(), &target)
            .unwrap()
            .command
            .join(" ")
    }

    fn test_id(name: &str) -> TestId {
        TestId {
            file: PathBuf::from("src/a.rs"),
            name: name.to_owned(),
        }
    }

    #[test]
    fn test_selection_commands() {
        let symbol = |names: Vec<String>| TestTarget::Symbol {
            path: SymbolPath::parse("src/a.rs#Guard/new").unwrap(),
            referencing_tests: names.iter().map(|name| test_id(name)).collect(),
        };
        assert_eq!(
            command(symbol(vec!["a::tests::x".into(), "a::tests::x".into()])),
            "cargo test --workspace a::tests::x"
        );
        assert_eq!(
            command(symbol(vec!["a::tests::x".into(), "b::y".into()])),
            "cargo test --workspace -- a::tests::x b::y"
        );
        let many: Vec<String> = (0..9).map(|n| format!("a::worker::tests::t{n}")).collect();
        assert_eq!(
            command(symbol(many)),
            "cargo test --workspace a::worker::tests::"
        );
        let scattered: Vec<String> = (0..9).map(|n| format!("m{n}::t")).collect();
        assert_eq!(command(symbol(scattered)), "cargo test --workspace");
        assert!(matches!(
            RustSupport.test_selection(&project(), &symbol(Vec::new())),
            Err(LangError::Unsupported(_))
        ));
        let file = |path: &str| command(TestTarget::File(PathBuf::from(path)));
        assert_eq!(file("tests/lang.rs"), "cargo test --test lang");
        assert_eq!(
            file("src/assistance/worker.rs"),
            "cargo test --workspace assistance::worker::"
        );
        assert_eq!(file("src/lang/mod.rs"), "cargo test --workspace lang::");
        assert_eq!(file("src/lib.rs"), "cargo test --workspace --lib");
        assert_eq!(file("src/bin/tool.rs"), "cargo test --workspace --bin tool");
        assert_eq!(
            command(TestTarget::Pattern("lang::".into())),
            "cargo test --workspace lang::"
        );
    }

    #[test]
    fn parses_a_passing_run_across_targets() {
        let stdout = "\nrunning 2 tests\ntest a::one ... ok\ntest a::two ... ignored, slow\n\n\
            test result: ok. 1 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\n\
            running 1 test\ntest works ... ok\n\n\
            test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
        let report = RustSupport.parse_test_output(stdout, "   Compiling x v0.1.0\n");
        assert_eq!(
            report,
            TestReport {
                passed: 2,
                failed: 0,
                ignored: 1,
                failures: Vec::new(),
                incomplete: false
            }
        );
    }

    #[test]
    fn parses_a_failing_run_with_panic_location() {
        let stdout = "\nrunning 2 tests\ntest tests::good ... ok\ntest tests::bad ... FAILED\n\n\
            failures:\n\n---- tests::bad stdout ----\n\n\
            thread 'tests::bad' panicked at src/lib.rs:12:9:\n\
            assertion `left == right` failed\n  left: 1\n right: 2\n\
            note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n\n\
            failures:\n    tests::bad\n\n\
            test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
        let report =
            RustSupport.parse_test_output(stdout, "error: test failed, to rerun pass `--lib`\n");
        assert_eq!(
            (report.passed, report.failed, report.incomplete),
            (1, 1, false)
        );
        assert_eq!(
            report.failures,
            vec![TestFailure {
                name: "tests::bad".to_owned(),
                location: Some((PathBuf::from("src/lib.rs"), 12)),
                message: "assertion `left == right` failed".to_owned(),
            }]
        );
    }

    #[test]
    fn missing_summary_is_incomplete() {
        let report = RustSupport.parse_test_output("\nrunning 3 tests\ntest a ... ok\n", "");
        assert!(report.incomplete);
        assert_eq!(report.passed, 1);
        let compile_error = RustSupport.parse_test_output("", "error[E0425]: cannot find value\n");
        assert!(compile_error.incomplete);
        assert_eq!(compile_error.passed + compile_error.failed, 0);
    }

    #[test]
    fn detect_reads_manifest_toolchain_ci_and_makefile() {
        let root = std::env::temp_dir().join(format!("agent-ide-lang-rust-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".github/workflows")).unwrap();
        fs::create_dir_all(root.join("src/bin")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"x\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(
            root.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.98.1\"\n",
        )
        .unwrap();
        fs::write(
            root.join(".github/workflows/ci.yml"),
            "jobs:\n  test:\n    steps:\n      - run: cargo test --workspace --locked\n      - run: |\n          cargo clippy --all-targets -- -D warnings\n          echo \"$X\" | cargo fmt\n",
        )
        .unwrap();
        fs::write(root.join("Makefile"), "fmt:\n\tcargo fmt\nFLAGS := -x\n").unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        fs::write(root.join("src/bin/tool.rs"), "").unwrap();
        let project = RustSupport.detect(&root).unwrap();
        let _ = fs::remove_dir_all(&root);

        assert_eq!(
            project.environment,
            vec![
                ("toolchain".to_owned(), "1.98.1".to_owned()),
                ("edition".to_owned(), "2024".to_owned())
            ]
        );
        let commands = &project.commands;
        let text = |command: &Option<ProjectCommand>| {
            let command = command.as_ref().unwrap();
            (command.argv.join(" "), command.source)
        };
        assert_eq!(
            text(&commands.test),
            (
                "cargo test --workspace --locked".to_owned(),
                CommandSource::Ci
            )
        );
        assert_eq!(
            text(&commands.lint),
            (
                "cargo clippy --all-targets -- -D warnings".to_owned(),
                CommandSource::Ci
            )
        );
        assert_eq!(
            text(&commands.format),
            ("make fmt".to_owned(), CommandSource::Makefile)
        );
        assert_eq!(
            text(&commands.build),
            ("cargo build".to_owned(), CommandSource::Manifest)
        );
        assert_eq!(commands.typecheck, None);
        assert_eq!(
            project.entry_points,
            vec![
                PathBuf::from("src/lib.rs"),
                PathBuf::from("src/bin/tool.rs")
            ]
        );
        assert_eq!(
            RustSupport.format_command(&project, Path::new("src/lib.rs")),
            Some(argv_of("rustfmt --edition 2024 src/lib.rs"))
        );
        assert!(
            RustSupport
                .detect(Path::new("/nonexistent-agent-ide-root"))
                .is_none()
        );
    }

    #[test]
    fn format_stdin_command_drops_the_file_argument() {
        assert_eq!(
            RustSupport.format_stdin_command(&project(), Path::new("src/lib.rs")),
            Some(argv_of("rustfmt --edition 2024"))
        );
        assert_eq!(
            RustSupport.format_stdin_command(&project(), Path::new("src/lib.py")),
            None
        );
        let no_rustfmt = LanguageProject {
            manifests: Vec::new(),
            ..project()
        };
        assert_eq!(
            RustSupport.format_stdin_command(&no_rustfmt, Path::new("src/lib.rs")),
            None
        );
    }
}
