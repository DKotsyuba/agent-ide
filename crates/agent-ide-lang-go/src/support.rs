//! Go language support (v0.4): `go.mod` detection, gopls symbol normalization, insertion sites,
//! `go test` selection and output parsing, `gofmt`.
//!
//! # Headers and names
//!
//! A symbol's header is the contiguous block of `//` comment lines directly above its
//! declaration (gopls ranges start at the `func`/type name and never include it). The doc is the
//! first paragraph of that block with `//` stripped and directive lines (`//go:generate`,
//! `//nolint:…`) skipped.
//!
//! gopls reports methods at file level, named `(*T).Name`, `(T).Name` or `T.Name` (a bare `Name`
//! is resolved from the receiver on the declaration line). Each receiver type `T` gets one
//! synthetic container symbol per file — kind [`SymbolKind::Impl`], name `T`, signature
//! `methods of T`, range from its first to its last method, placed where its first method is —
//! and its methods are addressed `T/Name`. When the file also declares `type T`, the type comes
//! first and `T` alone finds it; [`Outline::find`] backtracks so `T/Name` still reaches the
//! method, but `First`/`Last` on `T` then address the type's fields, not the method set.
//!
//! The line-oriented helpers are the core's shared brace-language helpers
//! ([`agent_ide_core::lang::brace`]).

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use async_lsp::lsp_types as lsp;

use agent_ide_core::lang::{
    CommandSource, InsertSite, InsertWhere, LangError, Language, LanguageProject, LanguageSupport,
    LineRange, Outline, ProjectCommand, ProjectCommands, Symbol, SymbolKind, SymbolPath,
    TestFailure, TestReport, TestSelection, TestTarget,
    brace::{
        Located, argv_of, declaration_line, dedup_tests, first_paragraph, line_at, place,
        signature, source_lines,
    },
    kind_of, line_count, lines_of,
};

use crate::LANGUAGE;

/// Stateless Go implementation of [`LanguageSupport`].
#[derive(Clone, Copy, Debug, Default)]
pub struct GoSupport;

impl LanguageSupport for GoSupport {
    /// Always the Go [`LANGUAGE`].
    fn language(&self) -> Language {
        LANGUAGE
    }

    /// Detects a Go module by `root/go.mod`.
    ///
    /// Environment facts from `go.mod`: `module`, `go` (language version) and `toolchain`.
    /// Commands (all [`CommandSource::Manifest`]): build `go build ./...`, check `go vet ./...`,
    /// test `go test ./...`, format `gofmt -l .` (lists unformatted files; per-file formatting is
    /// [`LanguageSupport::format_command`]), lint `golangci-lint run` only with a
    /// `.golangci.yml`/`.golangci.yaml`; no typecheck. Entry points follow the conventions
    /// `main.go` and `cmd/*/main.go` without reading any Go file.
    fn detect(&self, root: &Path) -> Option<LanguageProject> {
        let manifest = fs::read_to_string(root.join("go.mod")).ok()?;
        let mut environment = Vec::new();
        for line in manifest.lines().map(str::trim) {
            for key in ["module", "go", "toolchain"] {
                if let Some(value) = line
                    .strip_prefix(key)
                    .and_then(|rest| rest.strip_prefix(' '))
                {
                    environment.push((key.to_owned(), value.trim().to_owned()));
                }
            }
        }
        let command = |text: &str| {
            Some(ProjectCommand {
                argv: argv_of(text),
                source: CommandSource::Manifest,
            })
        };
        let golangci = [".golangci.yml", ".golangci.yaml"]
            .iter()
            .any(|name| root.join(name).is_file());
        let mut entry_points = Vec::new();
        if root.join("main.go").is_file() {
            entry_points.push(PathBuf::from("main.go"));
        }
        if let Ok(entries) = fs::read_dir(root.join("cmd")) {
            let mut commands: Vec<PathBuf> = entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().join("main.go").is_file())
                .map(|entry| Path::new("cmd").join(entry.file_name()).join("main.go"))
                .collect();
            commands.sort();
            entry_points.extend(commands);
        }
        Some(LanguageProject {
            language: LANGUAGE,
            manifests: vec![PathBuf::from("go.mod")],
            environment,
            interpreter: None,
            commands: ProjectCommands {
                build: command("go build ./..."),
                check: command("go vet ./..."),
                test: command("go test ./..."),
                lint: if golangci {
                    command("golangci-lint run")
                } else {
                    None
                },
                format: command("gofmt -l ."),
                typecheck: None,
            },
            entry_points,
        })
    }

    /// Normalizes gopls document symbols (see the module docs for headers and method sets).
    /// Functions named `Test…` in a `_test.go` file become [`SymbolKind::Test`]; signatures are
    /// the declaration up to its body `{` (an empty `{}` type literal such as `interface{}` does
    /// not end it), or the first declaration line when there is no body.
    fn normalize(&self, file: &Path, source: &str, symbols: Vec<lsp::DocumentSymbol>) -> Outline {
        let lines = source_lines(source);
        let root = SymbolPath::new(Some(file.to_path_buf()), Vec::new());
        let test_file = self.is_test_file(file);
        let mut output: Vec<Symbol> = Vec::new();
        let mut sets: HashMap<String, usize> = HashMap::new();
        for symbol in symbols {
            let receiver = (symbol.kind == lsp::SymbolKind::METHOD)
                .then(|| method_receiver(&lines, &symbol))
                .flatten();
            let Some(receiver) = receiver else {
                output.push(convert(&lines, symbol, &root, test_file));
                continue;
            };
            let index = *sets.entry(receiver.clone()).or_insert_with(|| {
                output.push(Symbol {
                    path: root.child(&receiver),
                    kind: SymbolKind::Impl,
                    name: receiver.clone(),
                    range: LineRange::new(1, 1),
                    body: LineRange::new(1, 1),
                    signature: format!("methods of {receiver}"),
                    doc: None,
                    children: Vec::new(),
                });
                output.len() - 1
            });
            let owner = output[index].path.clone();
            output[index]
                .children
                .push(convert(&lines, symbol, &owner, test_file));
        }
        for index in sets.into_values() {
            let set = &mut output[index];
            let start = set
                .children
                .iter()
                .map(|m| m.range.start)
                .min()
                .unwrap_or(1);
            let end = set
                .children
                .iter()
                .map(|m| m.range.end)
                .max()
                .unwrap_or(start);
            set.range = LineRange::new(start, end);
            set.body = set.range;
        }
        Outline {
            file: file.to_path_buf(),
            language: LANGUAGE,
            line_count: line_count(source),
            symbols: output,
        }
    }

    /// Computes the insertion point with the shared brace-language rules (`agent_ide_core::lang::brace::place`);
    /// containers are the kinds [`SymbolKind::is_container`] accepts (structs, interfaces and the
    /// synthetic method sets), and members of an empty container are indented one tab deeper.
    fn insert_site(
        &self,
        source: &str,
        outline: &Outline,
        anchor: &SymbolPath,
        where_: InsertWhere,
    ) -> Result<InsertSite, LangError> {
        place(source, outline, anchor, where_, "\t", |symbol| {
            symbol.kind.is_container()
        })
    }

    /// Files named `*_test.go`.
    fn is_test_file(&self, file: &Path) -> bool {
        file.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with("_test.go"))
    }

    /// Builds the `go test` command, run from the module root.
    ///
    /// * Symbol: `go test ./<dir>/... -run ^(TestA|TestB)$` over the directories of the
    ///   referencing tests' files (`./...` for the root), test names deduplicated with any
    ///   package qualifier (`pkg.`) and subtest suffix (`/sub`) dropped; no referencing tests is
    ///   [`LangError::Unsupported`].
    /// * File: `go test ./<dir>` — the whole package, since the contract forbids reading the file
    ///   to list its tests.
    /// * Pattern: `go test ./... -run <pattern>`.
    ///
    /// argv carries the regex unquoted (no shell).
    fn test_selection(
        &self,
        _project: &LanguageProject,
        target: &TestTarget,
    ) -> Result<TestSelection, LangError> {
        let mut command = argv_of("go test");
        let tests = match target {
            TestTarget::Symbol {
                path,
                referencing_tests,
            } => {
                let tests = dedup_tests(referencing_tests);
                if tests.is_empty() {
                    return Err(LangError::Unsupported(format!("no tests reference {path}")));
                }
                let mut names: Vec<&str> = Vec::new();
                for test in &tests {
                    let package = package_arg(test.file.parent(), true);
                    if !command.contains(&package) {
                        command.push(package);
                    }
                    let name = test.name.split('/').next().unwrap_or(&test.name);
                    let name = name.rsplit('.').next().unwrap_or(name);
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
                command.extend(["-run".to_owned(), format!("^({})$", names.join("|"))]);
                tests
            }
            TestTarget::File(file) => {
                command.push(package_arg(file.parent(), false));
                Vec::new()
            }
            TestTarget::Pattern(pattern) => {
                command.extend(["./...".to_owned(), "-run".to_owned(), pattern.clone()]);
                Vec::new()
            }
        };
        Ok(TestSelection { tests, command })
    }

    /// Parses `go test` output (stdout, then stderr).
    ///
    /// Counts come from top-level `--- PASS`/`--- FAIL`/`--- SKIP` lines. Limitation: without
    /// `-v` Go prints no line per passing test, so a passing run reports `passed: 0` with
    /// `incomplete: false` once a package result line (`ok`, `FAIL\t<pkg>`, `?`) is seen; only
    /// failures are itemized. The report is `incomplete` when no package result line appears or a
    /// package reports `[build failed]`/`[setup failed]`. Each `--- FAIL: <name>` (subtests
    /// included) becomes a failure whose location and message are the first `file.go:N: message`
    /// line logged for it — before the `--- FAIL` line under `-v`, after it otherwise — or a
    /// `panic:` line.
    fn parse_test_output(&self, stdout: &str, stderr: &str) -> TestReport {
        let text = format!("{stdout}\n{stderr}");
        let mut report = TestReport::default();
        let mut package_results = 0;
        let mut pending: Option<Located> = None;
        let mut open: Option<usize> = None;
        for line in text.lines() {
            let trimmed = line.trim_start();
            let top_level = trimmed.len() == line.len();
            if let Some(rest) = trimmed.strip_prefix("--- FAIL: ") {
                if top_level {
                    report.failed += 1;
                }
                let (location, message) = pending.take().unwrap_or_default();
                report.failures.push(TestFailure {
                    name: test_name(rest),
                    location,
                    message,
                });
                open = Some(report.failures.len() - 1);
            } else if trimmed.starts_with("--- PASS: ")
                || trimmed.starts_with("--- SKIP: ")
                || trimmed.starts_with("=== ")
            {
                if top_level && trimmed.starts_with("--- PASS: ") {
                    report.passed += 1;
                } else if top_level && trimmed.starts_with("--- SKIP: ") {
                    report.ignored += 1;
                }
                pending = None;
                open = None;
            } else if top_level
                && (line.starts_with("ok ")
                    || line.starts_with("ok\t")
                    || line.starts_with("FAIL\t")
                    || line.starts_with("?   "))
            {
                package_results += 1;
                if line.contains("[build failed]") || line.contains("[setup failed]") {
                    report.incomplete = true;
                }
                pending = None;
                open = None;
            } else if let Some(logged) = logged_message(trimmed) {
                match open.map(|index| &mut report.failures[index]) {
                    Some(failure) if failure.message.is_empty() => {
                        (failure.location, failure.message) = logged;
                    }
                    Some(_) => {}
                    None => {
                        pending.get_or_insert(logged);
                    }
                }
            }
        }
        if package_results == 0 {
            report.incomplete = true;
        }
        report
    }

    /// `gofmt -w <file>`; every Go project has gofmt.
    fn format_command(&self, _project: &LanguageProject, file: &Path) -> Option<Vec<String>> {
        Some(vec![
            "gofmt".to_owned(),
            "-w".to_owned(),
            file.display().to_string(),
        ])
    }

    /// `gofmt` with no file argument, so it reads the candidate text on stdin and writes the
    /// formatted text to stdout. `None` for a non-`.go` file.
    fn format_stdin_command(&self, _project: &LanguageProject, file: &Path) -> Option<Vec<String>> {
        (file.extension().and_then(|ext| ext.to_str()) == Some("go"))
            .then(|| vec!["gofmt".to_owned()])
    }
}

/// Converts one gopls symbol (and its children) under `owner`; method names lose their
/// `(*T).`/`T.` receiver prefix. Line numbers outside `lines` read as empty lines, never panic.
fn convert(
    lines: &[&str],
    symbol: lsp::DocumentSymbol,
    owner: &SymbolPath,
    test_file: bool,
) -> Symbol {
    let reported = lines_of(&symbol.range);
    let name_line = (symbol.selection_range.start.line + 1).max(reported.start);
    let decl = declaration_line(lines, reported.start, name_line, false);
    let mut start = decl;
    while start > 1 && line_at(lines, start - 1).trim().starts_with("//") {
        start -= 1;
    }
    let start = start.min(reported.start);
    let name = match symbol.name.rsplit_once('.') {
        Some((_, method)) if symbol.kind == lsp::SymbolKind::METHOD => method.to_owned(),
        _ => symbol.name,
    };
    let kind = match kind_of(symbol.kind) {
        SymbolKind::Function if test_file && name.starts_with("Test") => SymbolKind::Test,
        other => other,
    };
    let path = owner.child(&name);
    let body = LineRange::new(decl, reported.end);
    let children = symbol
        .children
        .unwrap_or_default()
        .into_iter()
        .map(|child| convert(lines, child, &path, test_file))
        .collect();
    Symbol {
        signature: signature(lines, body, false),
        doc: first_paragraph(
            (start..decl)
                .filter_map(|line| line_at(lines, line).trim().strip_prefix("//"))
                .filter(|text| !is_directive(text)),
        ),
        path,
        kind,
        name,
        range: LineRange::new(start, reported.end),
        body,
        children,
    }
}

/// Whether a comment body (text after `//`) is a tool directive such as `go:generate …` or
/// `nolint:errcheck`: it starts right after the slashes with an identifier followed by `:`.
fn is_directive(text: &str) -> bool {
    text.split_once(':').is_some_and(|(head, _)| {
        !head.is_empty()
            && head
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    })
}

/// Receiver type of a file-level gopls method: from its name (`(*T).Name`, `(T).Name`, `T.Name`)
/// or else from the declaration line `func (r *T[K]) Name(`; type arguments and `*` dropped.
/// `None` when neither yields a type.
fn method_receiver(lines: &[&str], symbol: &lsp::DocumentSymbol) -> Option<String> {
    let receiver = match symbol.name.rsplit_once('.') {
        Some((receiver, _)) => receiver
            .trim_start_matches('(')
            .trim_end_matches(')')
            .to_owned(),
        None => {
            let line = line_at(lines, symbol.selection_range.start.line + 1);
            let rest = line.trim_start().strip_prefix("func")?.trim_start();
            let inner = rest.strip_prefix('(')?;
            let inner = &inner[..inner.find(')')?];
            inner.split_whitespace().last()?.to_owned()
        }
    };
    let receiver = receiver.trim_start_matches('*');
    let receiver = receiver.split('[').next().unwrap_or(receiver).trim();
    (!receiver.is_empty()).then(|| receiver.to_owned())
}

/// `go test` package argument for a directory relative to the module root: `./dir/...` or
/// `./dir` (`./...` or `.` for the root / no directory).
fn package_arg(dir: Option<&Path>, recursive: bool) -> String {
    let dir = dir
        .map(|dir| dir.display().to_string())
        .filter(|dir| !dir.is_empty() && dir != ".");
    match (dir, recursive) {
        (Some(dir), true) => format!("./{dir}/..."),
        (Some(dir), false) => format!("./{dir}"),
        (None, true) => "./...".to_owned(),
        (None, false) => ".".to_owned(),
    }
}

/// Test name of a `--- FAIL: <name> (0.00s)` line remainder.
fn test_name(rest: &str) -> String {
    rest.split(" (").next().unwrap_or(rest).trim().to_owned()
}

/// Parses a `t.Log`-style line `file.go:12: message` (already trimmed) or a `panic: …` line.
fn logged_message(line: &str) -> Option<Located> {
    if let Some(panic) = line.strip_prefix("panic: ") {
        return Some((None, panic.trim().to_owned()));
    }
    let (location, message) = line.split_once(": ")?;
    let (file, number) = location.rsplit_once(':')?;
    if !file.ends_with(".go") || file.contains(' ') {
        return None;
    }
    let number = number.parse().ok()?;
    Some((
        Some((PathBuf::from(file), number)),
        message.trim().to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ide_core::lang::TestId;

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
                lsp::Position::new(name_line, 5),
                lsp::Position::new(name_line, 9),
            ),
            children: (!children.is_empty()).then_some(children),
        }
    }

    const SOURCE: &str = "package server

import \"context\"

// Server serves.
type Server struct {
\taddr string
}

// Start starts the server.
//
// It blocks.
//nolint:funlen
func (s *Server) Start(ctx context.Context) error {
\treturn nil
}

func (s Server) Addr() string { return s.addr }

// New builds a server.
func New(addr string, opts ...func(*Server)) *Server {
\treturn &Server{addr: addr}
}

func Any() interface{} {
\treturn nil
}
";

    /// gopls symbols for [`SOURCE`]: methods at file level with receiver-qualified names.
    fn outline() -> Outline {
        use lsp::SymbolKind as K;
        GoSupport.normalize(
            Path::new("server/server.go"),
            SOURCE,
            vec![
                ds(
                    "Server",
                    K::STRUCT,
                    (5, 7),
                    5,
                    vec![ds("addr", K::FIELD, (6, 6), 6, vec![])],
                ),
                ds("(*Server).Start", K::METHOD, (13, 15), 13, vec![]),
                ds("(Server).Addr", K::METHOD, (17, 17), 17, vec![]),
                ds("New", K::FUNCTION, (20, 22), 20, vec![]),
                ds("Any", K::FUNCTION, (24, 26), 24, vec![]),
            ],
        )
    }

    fn find<'a>(outline: &'a Outline, path: &str) -> &'a Symbol {
        outline
            .find(&SymbolPath::parse(path).unwrap())
            .unwrap_or_else(|| panic!("{path} not found"))
    }

    #[test]
    fn methods_get_a_synthetic_receiver_container() {
        let outline = outline();
        let names: Vec<(&str, SymbolKind)> = outline
            .symbols
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.kind))
            .collect();
        assert_eq!(
            names,
            [
                ("Server", SymbolKind::Struct),
                ("Server", SymbolKind::Impl),
                ("New", SymbolKind::Function),
                ("Any", SymbolKind::Function),
            ]
        );
        assert_eq!(outline.symbols[1].range, LineRange::new(10, 18));
        let start = find(&outline, "Server/Start");
        assert_eq!(start.kind, SymbolKind::Method);
        assert_eq!(start.path.to_string(), "server/server.go#Server/Start");
        assert_eq!(start.range, LineRange::new(10, 16));
        assert_eq!(start.body, LineRange::new(14, 16));
        assert_eq!(start.doc.as_deref(), Some("Start starts the server."));
        assert_eq!(
            start.signature,
            "func (s *Server) Start(ctx context.Context) error"
        );
        assert_eq!(
            find(&outline, "Server/Addr").signature,
            "func (s Server) Addr() string"
        );
    }

    #[test]
    fn headers_signatures_and_docs() {
        let outline = outline();
        let server = find(&outline, "Server");
        assert_eq!(server.kind, SymbolKind::Struct);
        assert_eq!(server.range, LineRange::new(5, 8));
        assert_eq!(server.signature, "type Server struct");
        assert_eq!(server.doc.as_deref(), Some("Server serves."));
        assert_eq!(find(&outline, "Server/addr").signature, "addr string");
        let new = find(&outline, "New");
        assert_eq!(new.range, LineRange::new(20, 23));
        assert_eq!(
            new.signature,
            "func New(addr string, opts ...func(*Server)) *Server"
        );
        let any = find(&outline, "Any");
        assert_eq!(any.range, LineRange::new(25, 27));
        assert_eq!(any.doc, None);
        assert_eq!(any.signature, "func Any() interface{}");
    }

    #[test]
    fn test_functions_in_test_files_are_tests() {
        let source = "package server\n\nimport \"testing\"\n\nfunc TestStart(t *testing.T) {\n\tt.Fatal(\"x\")\n}\n\nfunc helper() {}\n";
        let outline = GoSupport.normalize(
            Path::new("server/server_test.go"),
            source,
            vec![
                ds("TestStart", lsp::SymbolKind::FUNCTION, (4, 6), 4, vec![]),
                ds("helper", lsp::SymbolKind::FUNCTION, (8, 8), 8, vec![]),
            ],
        );
        assert_eq!(outline.symbols[0].kind, SymbolKind::Test);
        assert_eq!(outline.symbols[1].kind, SymbolKind::Function);
        assert!(GoSupport.is_test_file(Path::new("a/b_test.go")));
        assert!(!GoSupport.is_test_file(Path::new("a/b.go")));
    }

    #[test]
    fn insert_sites_for_all_positions() {
        let outline = outline();
        let site = |path: &str, where_| {
            GoSupport.insert_site(SOURCE, &outline, &SymbolPath::parse(path).unwrap(), where_)
        };
        let at = |line, indent: &str, blank_before, blank_after| InsertSite {
            line,
            indent: indent.to_owned(),
            blank_before,
            blank_after,
        };
        assert_eq!(site("New", InsertWhere::Before), Ok(at(20, "", 0, 1)));
        assert_eq!(site("New", InsertWhere::After), Ok(at(24, "", 1, 0)));
        assert_eq!(
            site("Server/Addr", InsertWhere::After),
            Ok(at(19, "", 1, 0))
        );
        assert_eq!(site("Server", InsertWhere::First), Ok(at(7, "\t", 0, 1)));
        assert_eq!(site("Server", InsertWhere::Last), Ok(at(8, "\t", 1, 0)));
        assert_eq!(
            site("New", InsertWhere::Last),
            Err(LangError::NotAContainer(SymbolPath::parse("New").unwrap()))
        );
        assert_eq!(
            site("Gone", InsertWhere::Before),
            Err(LangError::UnknownSymbol(SymbolPath::parse("Gone").unwrap()))
        );

        // A method set without its type in the file: First/Last address the methods.
        let source = "package server\n\n// Stop stops.\nfunc (s *Server) Stop() {}\n\nfunc (s *Server) Wait() {}\n";
        let methods = GoSupport.normalize(
            Path::new("server/stop.go"),
            source,
            vec![
                ds("(*Server).Stop", lsp::SymbolKind::METHOD, (3, 3), 3, vec![]),
                ds("Wait", lsp::SymbolKind::METHOD, (5, 5), 5, vec![]),
            ],
        );
        assert_eq!(methods.symbols.len(), 1);
        assert_eq!(methods.symbols[0].range, LineRange::new(3, 6));
        let set = SymbolPath::parse("Server").unwrap();
        assert_eq!(
            GoSupport.insert_site(source, &methods, &set, InsertWhere::First),
            Ok(at(3, "", 0, 1))
        );
        assert_eq!(
            GoSupport.insert_site(source, &methods, &set, InsertWhere::Last),
            Ok(at(7, "", 1, 0))
        );
    }

    fn project() -> LanguageProject {
        LanguageProject {
            language: LANGUAGE,
            manifests: vec![PathBuf::from("go.mod")],
            environment: Vec::new(),
            interpreter: None,
            commands: ProjectCommands::default(),
            entry_points: Vec::new(),
        }
    }

    #[test]
    fn test_selection_commands() {
        let id = |file: &str, name: &str| TestId {
            file: PathBuf::from(file),
            name: name.to_owned(),
        };
        let command = |target: TestTarget| {
            GoSupport
                .test_selection(&project(), &target)
                .unwrap()
                .command
                .join(" ")
        };
        let symbol = TestTarget::Symbol {
            path: SymbolPath::parse("internal/server/server.go#Server/Start").unwrap(),
            referencing_tests: vec![
                id("internal/server/server_test.go", "TestStart"),
                id("internal/server/server_test.go", "TestStop/graceful"),
                id("internal/server/server_test.go", "TestStart"),
            ],
        };
        assert_eq!(
            command(symbol),
            "go test ./internal/server/... -run ^(TestStart|TestStop)$"
        );
        assert_eq!(
            command(TestTarget::File(PathBuf::from("internal/server/server.go"))),
            "go test ./internal/server"
        );
        assert_eq!(
            command(TestTarget::Pattern("TestX".into())),
            "go test ./... -run TestX"
        );
        assert_eq!(
            GoSupport.format_command(&project(), Path::new("a/b.go")),
            Some(argv_of("gofmt -w a/b.go"))
        );
    }

    #[test]
    fn parses_verbose_and_plain_failures() {
        let expected = TestFailure {
            name: "TestB".to_owned(),
            location: Some((PathBuf::from("b_test.go"), 12)),
            message: "want 1, got 2".to_owned(),
        };
        let verbose = "=== RUN   TestA\n--- PASS: TestA (0.00s)\n=== RUN   TestB\n    b_test.go:12: want 1, got 2\n--- FAIL: TestB (0.00s)\n=== RUN   TestC\n--- SKIP: TestC (0.00s)\nFAIL\nFAIL\texample.com/x\t0.012s\nFAIL\n";
        let report = GoSupport.parse_test_output(verbose, "");
        assert_eq!(
            (
                report.passed,
                report.failed,
                report.ignored,
                report.incomplete
            ),
            (1, 1, 1, false)
        );
        assert_eq!(report.failures, vec![expected.clone()]);

        let plain = "--- FAIL: TestB (0.00s)\n    b_test.go:12: want 1, got 2\n    b_test.go:13: second\nFAIL\nFAIL\texample.com/x\t0.012s\n";
        let report = GoSupport.parse_test_output(plain, "");
        assert_eq!(
            (report.passed, report.failed, report.incomplete),
            (0, 1, false)
        );
        assert_eq!(report.failures, vec![expected]);
    }

    #[test]
    fn passing_run_without_v_and_incomplete_runs() {
        let report = GoSupport.parse_test_output("ok  \texample.com/x\t0.012s\n", "");
        assert_eq!(report, TestReport::default());
        assert!(
            GoSupport
                .parse_test_output("=== RUN   TestA\n", "")
                .incomplete
        );
        assert!(
            GoSupport
                .parse_test_output("", "FAIL\texample.com/x [build failed]\n")
                .incomplete
        );
    }

    #[test]
    fn detect_reads_go_mod_and_conventions() {
        let root = std::env::temp_dir().join(format!("agent-ide-lang-go-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("cmd/tool")).unwrap();
        fs::write(
            root.join("go.mod"),
            "module example.com/x\n\ngo 1.23\n\ntoolchain go1.23.4\n",
        )
        .unwrap();
        fs::write(root.join(".golangci.yml"), "linters: {}\n").unwrap();
        fs::write(root.join("main.go"), "package main\n").unwrap();
        fs::write(root.join("cmd/tool/main.go"), "package main\n").unwrap();
        let project = GoSupport.detect(&root).unwrap();
        let _ = fs::remove_dir_all(&root);

        let fact = |key: &str| {
            project
                .environment
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(fact("module"), Some("example.com/x"));
        assert_eq!(fact("go"), Some("1.23"));
        assert_eq!(fact("toolchain"), Some("go1.23.4"));
        let argv = |command: &Option<ProjectCommand>| command.as_ref().unwrap().argv.join(" ");
        assert_eq!(argv(&project.commands.check), "go vet ./...");
        assert_eq!(argv(&project.commands.lint), "golangci-lint run");
        assert_eq!(argv(&project.commands.format), "gofmt -l .");
        assert_eq!(
            project.entry_points,
            vec![PathBuf::from("main.go"), PathBuf::from("cmd/tool/main.go")]
        );
        assert!(
            GoSupport
                .detect(Path::new("/nonexistent-agent-ide-root"))
                .is_none()
        );
    }

    #[test]
    fn format_stdin_command_drops_the_file_argument() {
        assert_eq!(
            GoSupport.format_stdin_command(&project(), Path::new("a/b.go")),
            Some(argv_of("gofmt"))
        );
        assert_eq!(
            GoSupport.format_stdin_command(&project(), Path::new("a/b.py")),
            None
        );
    }
}
