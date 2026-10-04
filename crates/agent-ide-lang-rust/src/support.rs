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

use crate::LANGUAGE;

use agent_ide_core::lang::brace::{
    Located, argv_of, collapse_whitespace, declaration_line, dedup_tests, first_paragraph, line_at,
    place, signature, source_lines,
};
use agent_ide_core::lang::{
    CommandSource, InsertSite, InsertWhere, LangError, Language, LanguageProject, LanguageSupport,
    LineRange, Outline, ProjectCommand, ProjectCommands, Symbol, SymbolKind, SymbolPath,
    SyntaxVerdict, TestFailure, TestReport, TestSelection, TestTarget, kind_of, line_count,
    lines_of,
};

/// More referencing tests than this are selected by their common module prefix instead of by name.
const MAX_NAMED_TESTS: usize = 32;

/// Stateless Rust implementation of [`LanguageSupport`].
#[derive(Clone, Copy, Debug, Default)]
pub struct RustSupport;

impl LanguageSupport for RustSupport {
    /// Always the Rust [`LANGUAGE`].
    fn language(&self) -> Language {
        LANGUAGE
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
        let root_package = toml_string(&manifest, "name");
        for argv in ci_cargo_commands(root) {
            // A CI line that builds only a member other than the root package (a helper such
            // as `xtask`) is not the project's build; the slot falls through to the defaults.
            if selects_foreign_package(&argv, root_package.as_deref()) {
                continue;
            }
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
            language: LANGUAGE,
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
            language: LANGUAGE,
            line_count: line_count(source),
            symbols: symbols
                .into_iter()
                .map(|symbol| convert(&lines, symbol, &root, None))
                .collect(),
        }
    }

    /// Outline computed from the text alone (the `lexical` module parses it with `syn` and
    /// rebuilds rust-analyzer's document symbols, which this type's `normalize` then converts),
    /// so a cold rust-analyzer can still answer `ide.outline`, `ide.read` and symbol-addressed
    /// `ide.edit`. It is either the outline the server path would normalize for the same text or
    /// `None`: whatever the parse cannot reproduce exactly — a syntax error rust-analyzer would
    /// recover from, comments it may attach to an item's range, an extern block, a `// region:`
    /// comment, cfg-duplicated items with the same name, more tokens or deeper brackets than the
    /// parse's bounded stack is sized for — keeps the file server-backed instead of a guessed
    /// range. The recorded corpus in `tests/fixtures/lexical` checks the equality against
    /// rust-analyzer's own answers (the build in its `VERSION`).
    fn outline_from_source(&self, file: &Path, source: &str) -> Option<Outline> {
        crate::lexical::lexical_outline(file, source)
    }

    /// `syn` parses the text in this process (on the lexical outline's bounded parse thread); a
    /// refusal that keeps the outline server-backed is `Unchecked` so the edit still proceeds and
    /// the project check reports.
    fn syntax_verdict(&self, _file: &Path, source: &str) -> SyntaxVerdict {
        crate::lexical::syntax_verdict(source)
    }

    /// Always `true`: the lexical outline above meets the equality obligation, so outline, read
    /// and symbol edits answer while rust-analyzer loads.
    fn outline_while_loading(&self) -> bool {
        true
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
    /// * Symbol: up to 32 referencing test names as exact libtest filters. Larger selections use
    ///   their common module prefix when it is longer than `tests`; otherwise the workspace
    ///   filter is unfiltered. Tests in one integration binary use `--test`; mixed binaries use
    ///   the workspace filter. No referencing tests is [`LangError::Unsupported`].
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
                let integration_bins: std::collections::BTreeSet<String> = tests
                    .iter()
                    .filter_map(|test| integration_test_bin(&test.file))
                    .collect();
                let one_integration_bin = if integration_bins.len() == 1 {
                    integration_bins.iter().next()
                } else {
                    None
                };
                if let Some(bin) = one_integration_bin {
                    command.extend(["--test".to_owned(), bin.clone()]);
                }
                if integration_bins.len() > 1 {
                    // Cargo has no single invocation for exact filters across selected binaries.
                } else if tests.len() > MAX_NAMED_TESTS {
                    let prefix = common_module_prefix(tests.iter().map(|test| test.name.as_str()));
                    if !prefix.is_empty() && prefix != "tests" {
                        command.push("--".to_owned());
                        command.push(format!("{prefix}::"));
                    }
                } else {
                    command.extend(["--".to_owned(), "--exact".to_owned()]);
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

    /// Rust test names are module paths derived from the file (`tests/` files name their
    /// integration-test binary as the first segment).
    fn test_id(&self, file: &Path, outline_path: &str) -> String {
        test_id(file, outline_path)
    }

    /// Files under `tests/` build into their own integration-test binaries.
    fn test_binary(&self, file: &Path) -> Option<String> {
        integration_test_bin(file)
    }

    /// The first non-empty `//!` or `///` line of the leading comment block.
    fn file_doc(&self, text: &str) -> Option<String> {
        let lines: Vec<_> = text.lines().collect();

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

    /// `cargo` runs from the toolchain named by `AGENT_IDE_RUST_TOOLCHAIN_DIR` when that
    /// directory holds a `bin/cargo`: a toolchain cargo invoked directly still resolves `rustc`
    /// through `PATH`, i.e. the rustup proxy, which answers "no default is configured" outside the
    /// repository's toolchain override and ends the run in 0 s with no summary. Putting the
    /// toolchain's own `bin` first lets cargo find its matching rustc without rustup.
    fn test_toolchain(&self, program: &str) -> Option<(PathBuf, PathBuf)> {
        if program != "cargo" {
            return None;
        }
        let bin = PathBuf::from(std::env::var_os("AGENT_IDE_RUST_TOOLCHAIN_DIR")?).join("bin");
        let cargo = bin.join("cargo");
        cargo.is_file().then_some((cargo, bin))
    }
}

/// Builds a Rust test identifier from its project-relative source path and outline path.
///
/// Source modules under `src` contribute their crate-relative path; crate roots and integration
/// test roots do not. `#[path]` module inclusions may not match their physical path.
pub(crate) fn test_id(file: &Path, outline_path: &str) -> String {
    let modules = if let Some(at) = file.iter().position(|part| part == "src") {
        let parts: Vec<_> = file.iter().skip(at + 1).collect();
        match parts.as_slice() {
            [root] if *root == "lib.rs" || *root == "main.rs" => Vec::new(),
            [bin, _] if *bin == "bin" => Vec::new(),
            _ => parts
                .iter()
                .map(|part| part.to_string_lossy().trim_end_matches(".rs").to_owned())
                .filter(|part| part != "mod")
                .collect(),
        }
    } else {
        Vec::new()
    };
    modules
        .into_iter()
        .chain(outline_path.split("::").map(str::to_owned))
        .collect::<Vec<_>>()
        .join("::")
}

/// Returns the Cargo integration-test target for a file below a `tests` directory.
pub(crate) fn integration_test_bin(file: &Path) -> Option<String> {
    let parts: Vec<_> = file.iter().collect();
    let at = parts.iter().rposition(|part| *part == "tests")?;
    parts
        .get(at + 1)
        .map(|part| part.to_string_lossy().trim_end_matches(".rs").to_owned())
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
/// (`None` at file level). Field signatures come from their exact source span so multiple inline
/// fields on one line remain distinct. Line numbers outside `lines` read as empty lines, never panic.
fn convert(
    lines: &[&str],
    symbol: lsp::DocumentSymbol,
    owner: &SymbolPath,
    owner_kind: Option<SymbolKind>,
) -> Symbol {
    let reported = lines_of(&symbol.range);
    let name_line = (symbol.selection_range.start.line + 1).max(reported.start);
    let decl = declaration_line(lines, reported.start, name_line, true);
    let server_kind = if symbol.kind == lsp::SymbolKind::INTERFACE {
        SymbolKind::Trait
    } else {
        kind_of(symbol.kind)
    };
    let start = if server_kind == SymbolKind::Field {
        reported.start
    } else {
        header_start(lines, decl).min(reported.start)
    };
    let header: Vec<&str> = (start..decl)
        .map(|line| line_at(lines, line).trim())
        .collect();
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
    let body = LineRange::new(decl, reported.end);
    let signature = if server_kind == SymbolKind::Field {
        field_signature(lines, &symbol)
            .map(|source| {
                let field_lines = source_lines(&source);
                signature(
                    &field_lines,
                    LineRange::new(1, field_lines.len() as u32),
                    true,
                )
            })
            .unwrap_or_else(|| signature(lines, body, true))
    } else {
        signature(lines, body, true)
    };
    let name = if server_kind == SymbolKind::Impl {
        impl_segment(&symbol.name)
    } else {
        symbol.name
    };
    let path = owner.child(&name);
    let children = symbol
        .children
        .unwrap_or_default()
        .into_iter()
        .map(|child| convert(lines, child, &path, Some(server_kind)))
        .collect();
    Symbol {
        signature,
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

/// Extracts a field from its selected name through its type, retaining visibility before the name
/// while excluding field attributes and comments from the signature.
fn field_signature(lines: &[&str], symbol: &lsp::DocumentSymbol) -> Option<String> {
    let name_line = line_at(lines, symbol.selection_range.start.line + 1);
    let name_byte = utf16_column_byte(name_line, symbol.selection_range.start.character)?;
    let prefix = name_line.get(..name_byte)?;
    let after_attributes = prefix.rsplit_once(']').map_or(prefix, |(_, suffix)| suffix);
    let visibility = after_attributes.trim();
    let visibility = if visibility.starts_with("pub") {
        format!("{visibility} ")
    } else {
        String::new()
    };
    Some(format!(
        "{visibility}{}",
        source_span(lines, symbol.selection_range.start, symbol.range.end)?
    ))
}

/// Returns the UTF-8 source slice covered by LSP positions, converting their UTF-16 columns.
fn source_span(lines: &[&str], start: lsp::Position, end: lsp::Position) -> Option<String> {
    if start.line > end.line {
        return None;
    }
    let start_line = line_at(lines, start.line + 1);
    let start_byte = utf16_column_byte(start_line, start.character)?;
    let end_line = line_at(lines, end.line + 1);
    let end_byte = utf16_column_byte(end_line, end.character)?;
    if start.line == end.line {
        return Some(start_line.get(start_byte..end_byte)?.to_owned());
    }
    let mut text = String::from(start_line.get(start_byte..)?);
    for line in (start.line + 1)..end.line {
        text.push('\n');
        text.push_str(line_at(lines, line + 1));
    }
    text.push('\n');
    text.push_str(end_line.get(..end_byte)?);
    Some(text)
}

/// Converts an LSP UTF-16 column on one line to a UTF-8 byte offset, or `None` inside a codepoint.
fn utf16_column_byte(line: &str, column: u32) -> Option<usize> {
    let mut units = 0;
    for (byte, character) in line.char_indices() {
        if units == column {
            return Some(byte);
        }
        units += character.len_utf16() as u32;
        if units > column {
            return None;
        }
    }
    (units == column).then_some(line.len())
}

/// Caps a server-provided impl path segment while retaining a digest of its original label.
fn bounded_impl_segment(segment: String, full_label: &str) -> String {
    const MAX_BYTES: usize = 256;
    const PREFIX_BYTES: usize = 200;

    if segment.len() <= MAX_BYTES {
        return segment;
    }

    let mut end = PREFIX_BYTES.min(segment.len());
    while !segment.is_char_boundary(end) {
        end -= 1;
    }
    let digest = blake3::hash(full_label.as_bytes());
    let digest = digest.as_bytes();
    format!(
        "{}…{:02x}{:02x}{:02x}{:02x}",
        &segment[..end],
        digest[0],
        digest[1],
        digest[2],
        digest[3]
    )
}

/// Path segment of an impl block named by the server. Inherent impls use the bare type name;
/// trait impls keep their whitespace-normalized label. Labels longer than the bound are shortened
/// with a stable digest so nested paths stay bounded without losing addressability.
fn impl_segment(name: &str) -> String {
    let normalized = collapse_whitespace(name);
    let Some(rest) = normalized.strip_prefix("impl") else {
        return bounded_impl_segment(normalized, name);
    };
    let segment = if normalized.contains(" for ") {
        normalized
    } else {
        let mut rest = rest.trim_start();
        if rest.starts_with('<') {
            rest = skip_generics(rest).trim_start();
        }
        let ty = rest.split('<').next().unwrap_or(rest).trim();
        let ty = ty.rsplit("::").next().unwrap_or(ty).trim();
        if ty.is_empty() {
            normalized
        } else {
            ty.to_owned()
        }
    };
    bounded_impl_segment(segment, name)
}

/// `text` after its leading balanced `<…>` generic parameter list, or `""` when it never closes.
/// The `>` of an `->` inside a bound (`impl<F: Fn() -> u8>`) closes nothing.
fn skip_generics(text: &str) -> &str {
    let mut depth = 0usize;
    let mut previous = None;
    for (index, ch) in text.char_indices() {
        if ch == '<' {
            depth += 1;
        } else if ch == '>' && previous != Some('-') {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return &text[index + ch.len_utf8()..];
            }
        }
        previous = Some(ch);
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
pub(crate) fn attr_path(line: &str) -> Option<&str> {
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
/// square brackets stay unbalanced; `None` when the lines are not one attribute. Brackets are
/// counted on each line's code only: inside a string, char literal or comment on that line
/// (`#[error("index outside [0, {1})")]`) they are not syntax. A string literal that spans lines
/// is blanked only on its first line.
fn attribute_start(lines: &[&str], end: u32) -> Option<u32> {
    let mut depth = 0i32;
    let mut line = end;
    while line >= 1 && end - line < 64 {
        let text = line_at(lines, line).trim();
        if text.is_empty() {
            return None;
        }
        let code = crate::module_graph::blanked_code(&text.chars().collect::<Vec<_>>());
        let count = |bracket: char| code.iter().filter(|&&ch| ch == bracket).count() as i32;
        depth += count(']') - count('[');
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

/// Whether a CI cargo command restricts itself to packages other than the root package (a
/// workspace helper member such as `xtask`): such a line builds the helper, not the project.
/// A command without a package selection, one naming the root package among its selection, and
/// every command under a virtual workspace root (no root package to compare with) are kept.
fn selects_foreign_package(argv: &[String], root_package: Option<&str>) -> bool {
    let Some(root) = root_package else {
        return false;
    };
    let mut selected: Vec<&str> = Vec::new();
    let mut tokens = argv.iter().map(String::as_str);
    while let Some(token) = tokens.next() {
        let name = if token == "-p" || token == "--package" {
            tokens.next()
        } else {
            token.strip_prefix("--package=")
        };
        if let Some(name) = name {
            selected.push(name);
        }
    }
    !selected.is_empty() && !selected.contains(&root)
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
    use agent_ide_core::lang::TestId;

    /// Inline named-variant fields remain distinct lexical children with declaration-line output.
    #[test]
    fn inline_variant_fields_have_exact_names_and_lines() {
        let source = "\n\n\n\n\n\n#[derive(clap::Subcommand)]\npub enum Release {\n    /// Show version/CHANGELOG edits; --apply performs local edits only.\n    Prepare { version: String, #[arg(long)] apply: bool },\n    /// Publish an accepted CI bundle through a complete draft; never replace a release.\n    Publish { directory: PathBuf },\n    /// Observe one exact tag/commit and validate its run and bytes. No install or wake claim.\n    Wait { #[arg(long)] repo: String, #[arg(long)] tag: String, #[arg(long)] commit: String,\n        #[arg(long, default_value_t=1800)] timeout: u64, #[arg(long)] result_file: Option<PathBuf> },\n}\n";
        let lexical = crate::lexical::lexical_outline(Path::new("src/release.rs"), source)
            .expect("clap-style enum can be outlined from source");
        let release = &lexical.symbols[0];
        assert_eq!(release.name, "Release");
        assert_eq!(release.body.start, 8);
        assert_eq!(
            release
                .children
                .iter()
                .map(|variant| variant.name.as_str())
                .collect::<Vec<_>>(),
            ["Prepare", "Publish", "Wait"]
        );
        assert_eq!(
            release
                .children
                .iter()
                .map(|variant| variant.body.start)
                .collect::<Vec<_>>(),
            [10, 12, 14]
        );
        assert_eq!(
            release.children[0]
                .children
                .iter()
                .map(|symbol| symbol.name.as_str())
                .collect::<Vec<_>>(),
            ["version", "apply"]
        );
        assert_eq!(
            release.children[1]
                .children
                .iter()
                .map(|symbol| symbol.name.as_str())
                .collect::<Vec<_>>(),
            ["directory"]
        );
        assert_eq!(
            release.children[2]
                .children
                .iter()
                .map(|symbol| symbol.name.as_str())
                .collect::<Vec<_>>(),
            ["repo", "tag", "commit", "timeout", "result_file"]
        );
        assert_eq!(
            release.children[0].doc.as_deref(),
            Some("Show version/CHANGELOG edits; --apply performs local edits only.")
        );
        assert!(
            release
                .children
                .iter()
                .flat_map(|variant| &variant.children)
                .all(|field| field.doc.is_none()),
            "variant docs must not be copied to fields"
        );

        let rendered = agent_ide_core::lang::render::outline_text(&lexical);
        for expected in [
            "    8  pub enum Release\n",
            "   10    Prepare    // Show version/CHANGELOG edits; --apply performs local edits only.\n",
            "   10      version: String\n",
            "   10      apply: bool\n",
            "   12    Publish    // Publish an accepted CI bundle through a complete draft; never replace a…\n",
            "   12      directory: PathBuf\n",
            "   14    Wait    // Observe one exact tag/commit and validate its run and bytes. No install…\n",
            "   14      repo: String\n",
            "   14      tag: String\n",
            "   14      commit: String\n",
            "   15      timeout: u64\n",
            "   15      result_file: Option<PathBuf>\n",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?} in:\n{rendered}"
            );
        }
        assert_eq!(rendered.matches("    Prepare    ").count(), 1, "{rendered}");
        assert_eq!(rendered.matches("    Publish    ").count(), 1, "{rendered}");
        assert_eq!(rendered.matches("    Wait    ").count(), 1, "{rendered}");

        let documented_fn = crate::lexical::lexical_outline(
            Path::new("src/documented.rs"),
            "/// Function docs.\npub fn documented() {}\n",
        )
        .expect("documented function can be outlined from source");
        assert!(
            agent_ide_core::lang::render::outline_text(&documented_fn)
                .contains("    2  pub fn documented()    // Function docs."),
            "{}",
            agent_ide_core::lang::render::outline_text(&documented_fn)
        );
    }

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
            language: LANGUAGE,
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

    #[test]
    fn test_id_uses_crate_relative_source_modules() {
        for (file, name, expected) in [
            (
                "src/lang/path.rs",
                "tests::parses",
                "lang::path::tests::parses",
            ),
            ("src/lang/mod.rs", "tests::works", "lang::tests::works"),
            ("src/lib.rs", "tests::works", "tests::works"),
            ("src/main.rs", "nested::test", "nested::test"),
            ("src/a/b.rs", "tests::works", "a::b::tests::works"),
            ("src/a/mod.rs", "tests::works", "a::tests::works"),
            ("tests/foo.rs", "tests::works", "tests::works"),
            ("tests/foo.rs", "nested::works", "nested::works"),
        ] {
            assert_eq!(super::test_id(Path::new(file), name), expected, "{file}");
        }
    }

    #[test]
    fn test_selection_commands() {
        let symbol = |names: Vec<String>| TestTarget::Symbol {
            path: SymbolPath::parse("src/a.rs#Guard/new").unwrap(),
            referencing_tests: names
                .iter()
                .enumerate()
                .map(|(index, name)| TestId {
                    file: PathBuf::from(format!("src/m{index}.rs")),
                    name: name.clone(),
                })
                .collect(),
        };
        assert_eq!(
            command(symbol(vec!["a::tests::x".into()])),
            "cargo test --workspace -- --exact a::tests::x"
        );
        assert_eq!(
            command(symbol(vec!["a::tests::x".into(), "b::y".into()])),
            "cargo test --workspace -- --exact a::tests::x b::y"
        );
        let many: Vec<String> = (0..15).map(|n| format!("a{n}::tests::t{n}")).collect();
        assert_eq!(
            command(symbol(many.clone())),
            format!("cargo test --workspace -- --exact {}", many.join(" "))
        );
        let over_limit: Vec<String> = (0..33)
            .map(|n| format!("crate::worker::tests::case{n}"))
            .collect();
        assert_eq!(
            command(symbol(over_limit)),
            "cargo test --workspace -- crate::worker::tests::"
        );
        let scattered: Vec<String> = (0..33).map(|n| format!("m{n}::t")).collect();
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
        let integration = |file: &str, name: &str| TestId {
            file: PathBuf::from(file),
            name: name.to_owned(),
        };
        let one_binary = TestTarget::Symbol {
            path: SymbolPath::parse("src/lib.rs#Thing").unwrap(),
            referencing_tests: vec![
                integration("tests/alpha.rs", "first"),
                integration("tests/alpha.rs", "nested::second"),
            ],
        };
        assert_eq!(
            command(one_binary),
            "cargo test --workspace --test alpha -- --exact first nested::second"
        );
        let mixed_binaries = TestTarget::Symbol {
            path: SymbolPath::parse("src/lib.rs#Thing").unwrap(),
            referencing_tests: vec![
                integration("tests/alpha.rs", "first"),
                integration("tests/beta.rs", "second"),
            ],
        };
        assert_eq!(command(mixed_binaries), "cargo test --workspace");
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

    /// A workspace whose only CI `cargo build` line builds a helper member (`-p xtask`, the
    /// agent-worktree shape) must not present the helper as the project build: the slot falls
    /// through to the Cargo default, while a line naming the root package stays CI-sourced.
    #[test]
    fn detect_skips_ci_lines_that_build_a_foreign_member() {
        let root =
            std::env::temp_dir().join(format!("agent-ide-lang-rust-xtask-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".github/workflows")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("xtask/src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"xtask\"]\ndefault-members = [\".\"]\n\n[package]\nname = \"agent-worktree\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        fs::write(
            root.join("xtask/Cargo.toml"),
            "[package]\nname = \"xtask\"\n",
        )
        .unwrap();
        fs::write(
            root.join(".github/workflows/release.yml"),
            "jobs:\n  publish:\n    steps:\n      - name: Build only tooling\n        run: |\n          cargo fetch --locked\n          cargo build --frozen -p xtask\n",
        )
        .unwrap();
        let project = RustSupport.detect(&root).unwrap();
        let build = project.commands.build.as_ref().unwrap();
        assert_eq!(
            (build.argv.join(" "), build.source),
            ("cargo build".to_owned(), CommandSource::Manifest),
            "the xtask helper line must not become the project build"
        );
        let _ = fs::remove_dir_all(&root);

        // A CI line naming the root package (alone or beside others) is the project's own.
        let root = std::env::temp_dir().join(format!(
            "agent-ide-lang-rust-root-pkg-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".github/workflows")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"app\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(
            root.join(".github/workflows/ci.yml"),
            "jobs:\n  build:\n    steps:\n      - run: cargo build --release -p app\n",
        )
        .unwrap();
        let project = RustSupport.detect(&root).unwrap();
        let build = project.commands.build.as_ref().unwrap();
        assert_eq!(
            (build.argv.join(" "), build.source),
            ("cargo build --release -p app".to_owned(), CommandSource::Ci)
        );
        let _ = fs::remove_dir_all(&root);
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

    /// Formatting edited module text through stdin leaves its unformatted child file byte-identical.
    #[test]
    fn formatting_module_stdin_does_not_rewrite_child_modules() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let root = std::env::temp_dir().join(format!("agent-ide-rustfmt-{}", std::process::id()));
        let child = root.join("child.rs");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(&child, "fn child( ){\nlet x=1;\n}\n").unwrap();
        let before = fs::read(&child).unwrap();
        let argv = RustSupport
            .format_stdin_command(&project(), Path::new("mod.rs"))
            .unwrap();
        let mut rustfmt = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        rustfmt
            .stdin
            .take()
            .unwrap()
            .write_all(b"mod child;\nfn edited( ){let x=1;}\n")
            .unwrap();
        assert!(rustfmt.wait_with_output().unwrap().status.success());
        assert_eq!(fs::read(&child).unwrap(), before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_doc_reads_the_leading_module_comment() {
        assert_eq!(
            RustSupport.file_doc("//! Rust docs\nfn a() {}"),
            Some("Rust docs".into())
        );
    }

    /// Deep server paths keep long impl labels bounded and stable without a rust-analyzer process.
    #[test]
    fn nested_long_impl_labels_normalize_to_bounded_paths() {
        use async_lsp::lsp_types::{Position, Range, SymbolKind as K};

        /// Builds a nested server-symbol chain with the same long impl label at each level.
        fn nested(label: &str, depth: usize) -> lsp::DocumentSymbol {
            let mut symbol = ds(label, K::OBJECT, (0, 0), 0, Vec::new());
            for _ in 1..depth {
                symbol = ds(label, K::OBJECT, (0, 0), 0, vec![symbol]);
            }
            symbol.range = Range::new(Position::new(0, 0), Position::new(0, 1));
            symbol
        }

        let literal = "x".repeat(1024 * 1024);
        let source = format!("const _: &str = {:?};", literal);
        let label = format!("impl Marker for {}", &literal[..4096]);
        let support = RustSupport;
        let outline =
            support.normalize(Path::new("src/lib.rs"), &source, vec![nested(&label, 128)]);

        let mut symbol = &outline.symbols[0];
        let mut path = String::new();
        let mut depth = 0;
        loop {
            let name = symbol.path.name().unwrap();
            assert!(name.len() <= 256);
            path.push_str(name);
            path.push('/');
            depth += 1;
            let Some(child) = symbol.children.first() else {
                break;
            };
            symbol = child;
        }
        assert_eq!(depth, 128);
        assert!(path.len() < 128 * 256);
        assert_eq!(
            outline.symbols[0].path.name(),
            Some(impl_segment(&label).as_str())
        );
    }
}
