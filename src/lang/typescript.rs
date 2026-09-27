//! TypeScript/JavaScript language support (v0.4): typescript-language-server document symbols,
//! `package.json` scripts, vitest/jest/`node --test`, prettier.
//!
//! Declarations are read with a small lexical scanner (strings, comments and bracket depth
//! only), not a parser: signatures stop at the body `{`, the arrow `=>` or a depth-0 `;`.
//! Overloads arrive from the server as several same-named siblings (one per navigation-tree
//! span); normalization keeps the first and widens its `range` over the whole group through the
//! implementation, so reading or replacing the symbol moves every overload together. Its `body`
//! stays the first declaration's reported range.

use std::path::{Path, PathBuf};

use async_lsp::lsp_types as lsp;
use serde_json::Value;

use super::{
    CommandSource, InsertSite, InsertWhere, LangError, Language, LanguageProject, LanguageSupport,
    LineRange, Outline, ProjectCommand, ProjectCommands, Symbol, SymbolKind, SymbolPath,
    TestFailure, TestReport, TestSelection, TestTarget, kind_of, line_count, lines_of,
    python::{
        MAX_ATTRIBUTE_CHARS, MAX_NAMED_TESTS, distinct, distinct_files, entry_names, env_value,
        indent_of, indent_unit, last_content_line, line_at, one_line, read_text, source_lines,
    },
    render::clip,
};

/// TypeScript and JavaScript support over typescript-language-server's document symbols;
/// stateless.
#[derive(Clone, Copy, Debug, Default)]
pub struct TypeScript;

/// Extensions of files this module treats as scripts.
const SCRIPT_EXTENSIONS: [&str; 8] = ["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];

/// Most lines scanned for one declaration's header and signature.
const SCAN_LINES: usize = 200;

impl LanguageSupport for TypeScript {
    /// Always [`Language::TypeScript`] (JavaScript files included).
    fn language(&self) -> Language {
        Language::TypeScript
    }

    /// Establishes a project from `package.json`, `tsconfig.json`, `tsconfig.*.json` or
    /// `jsconfig.json` (any one suffices); `None` when none exists. Environment facts:
    /// `package_manager` (lockfile, then `packageManager`, else npm), `node` (`.nvmrc`,
    /// `.node-version`, `engines.node`), `tsconfig` (`tsconfig.json`, else the first
    /// `tsconfig.*.json`), `test_runner` (`vitest`, `jest` or `node`) and `formatter`
    /// (`prettier`), which `test_selection` and `format_command` read back. Commands come from
    /// `scripts` run through the package manager; typecheck falls back to
    /// `npx tsc --noEmit -p <tsconfig>`.
    fn detect(&self, root: &Path) -> Option<LanguageProject> {
        let names = entry_names(root);
        let has = |name: &str| names.iter().any(|entry| entry == name);
        let starts = |prefix: &str| names.iter().any(|entry| entry.starts_with(prefix));
        let extra_tsconfigs: Vec<&String> = names
            .iter()
            .filter(|name| name.starts_with("tsconfig.") && name.ends_with(".json"))
            .filter(|name| *name != "tsconfig.json")
            .collect();
        let mut manifests: Vec<PathBuf> = ["package.json", "tsconfig.json"]
            .into_iter()
            .filter(|name| has(name))
            .map(PathBuf::from)
            .collect();
        manifests.extend(extra_tsconfigs.iter().map(PathBuf::from));
        if has("jsconfig.json") {
            manifests.push(PathBuf::from("jsconfig.json"));
        }
        if manifests.is_empty() {
            return None;
        }
        let package: Value =
            serde_json::from_str(&read_text(root, "package.json")).unwrap_or(Value::Null);
        let depends = |name: &str| {
            ["dependencies", "devDependencies"]
                .iter()
                .any(|key| package.get(key).and_then(|deps| deps.get(name)).is_some())
        };

        let manager = [
            ("pnpm-lock.yaml", "pnpm"),
            ("yarn.lock", "yarn"),
            ("bun.lockb", "bun"),
            ("bun.lock", "bun"),
            ("package-lock.json", "npm"),
        ]
        .into_iter()
        .find(|(lock, _)| has(lock))
        .map(|(_, manager)| manager.to_owned())
        .or_else(|| {
            let field = package.get("packageManager")?.as_str()?;
            Some(field.split('@').next().unwrap_or(field).to_owned())
        })
        .unwrap_or_else(|| "npm".to_owned());
        let mut environment = vec![("package_manager".to_owned(), manager.clone())];
        let node = [".nvmrc", ".node-version"]
            .into_iter()
            .find_map(|name| {
                let text = read_text(root, name);
                let version = text.trim();
                (!version.is_empty()).then(|| version.to_owned())
            })
            .or_else(|| Some(package.get("engines")?.get("node")?.as_str()?.to_owned()));
        if let Some(node) = node {
            environment.push(("node".to_owned(), node));
        }
        let tsconfig = if has("tsconfig.json") {
            Some("tsconfig.json".to_owned())
        } else {
            extra_tsconfigs.first().map(|name| (*name).clone())
        };
        if let Some(tsconfig) = &tsconfig {
            environment.push(("tsconfig".to_owned(), tsconfig.clone()));
        }
        let runner = if depends("vitest") || starts("vitest.config.") {
            "vitest"
        } else if depends("jest") || starts("jest.config.") || package.get("jest").is_some() {
            "jest"
        } else {
            "node"
        };
        environment.push(("test_runner".to_owned(), runner.to_owned()));
        if starts(".prettierrc") || starts("prettier.config.") || package.get("prettier").is_some()
        {
            environment.push(("formatter".to_owned(), "prettier".to_owned()));
        }

        let scripts = package.get("scripts").and_then(Value::as_object);
        let script = |candidates: &[&str]| {
            let name = candidates
                .iter()
                .find(|name| scripts.is_some_and(|scripts| scripts.contains_key(**name)))?;
            Some(ProjectCommand {
                argv: vec![manager.clone(), "run".to_owned(), (*name).to_owned()],
                source: CommandSource::Manifest,
            })
        };
        let commands = ProjectCommands {
            build: script(&["build"]),
            check: None,
            test: script(&["test"]),
            lint: script(&["lint"]),
            format: script(&["format", "fmt", "prettier"]),
            typecheck: script(&["typecheck", "tsc", "check-types"]).or_else(|| {
                let tsconfig = tsconfig.clone()?;
                Some(ProjectCommand {
                    argv: ["npx", "tsc", "--noEmit", "-p", &tsconfig]
                        .map(String::from)
                        .to_vec(),
                    source: CommandSource::Default,
                })
            }),
        };

        let mut entries = Vec::new();
        for key in ["main", "module", "bin", "exports"] {
            if let Some(value) = package.get(key) {
                collect_strings(value, &mut entries);
            }
        }
        let mut entry_points: Vec<PathBuf> = Vec::new();
        let candidates = entries
            .iter()
            .filter(|entry| !entry.contains('*'))
            .map(|entry| entry.trim_start_matches("./").to_owned())
            .chain(
                ["src/index.ts", "src/main.ts"]
                    .into_iter()
                    .filter(|file| root.join(file).is_file())
                    .map(String::from),
            );
        for entry in candidates {
            let path = PathBuf::from(entry);
            if !entry_points.contains(&path) {
                entry_points.push(path);
            }
        }

        Some(LanguageProject {
            language: Language::TypeScript,
            manifests,
            environment,
            interpreter: None,
            commands,
            entry_points,
        })
    }

    /// Header = the JSDoc block (`/** … */`) and decorators directly above the declaration;
    /// `doc` is the JSDoc's first paragraph (up to a blank line or the first `@tag`).
    /// Signatures keep `export`/`export default`, drop decorators, and stop at the body `{`, the
    /// `;` of a bodiless declaration, or the arrow (`const f = (a: A) => R` for arrow consts).
    /// Kinds: class members become methods (`constructor` a constructor); `type X =`,
    /// `namespace`, `enum` and `interface` declarations are recognized from the source because
    /// the server reports type aliases as variables; in test files `describe`/`it`/`test`
    /// callbacks become tests with the call (`describe("math")`) as their signature.
    /// Same-named adjacent siblings (overloads, accessor pairs) merge as the module docs describe.
    /// The outline is a skeleton: inside function, method, constructor and test-callback bodies
    /// only nested functions, classes, interfaces and enums stay — locals, object-literal
    /// properties and statement-level symbols are dropped — and class/interface fields render as
    /// `name: Type` (annotated) or the clipped `name = value`.
    fn normalize(&self, file: &Path, source: &str, symbols: Vec<lsp::DocumentSymbol>) -> Outline {
        let lines = source_lines(source);
        let root = SymbolPath::new(Some(file.to_path_buf()), Vec::new());
        Outline {
            file: file.to_path_buf(),
            language: Language::TypeScript,
            line_count: line_count(source),
            symbols: convert_all(&lines, symbols, &root, None, self.is_test_file(file)),
        }
    }

    /// `Before`/`After` put one blank line between the new code and the anchor, at the anchor's
    /// indentation. `First`/`Last` accept classes, interfaces, namespaces, enums and object
    /// literal variables: `First` lands after the opening `{` line, `Last` before the closing
    /// `}` line, indented like the first member (or the container plus the file's unit, two
    /// spaces by default). A container whose braces share a line with its body is
    /// [`LangError::Unparseable`].
    fn insert_site(
        &self,
        source: &str,
        outline: &Outline,
        anchor: &SymbolPath,
        where_: InsertWhere,
    ) -> Result<InsertSite, LangError> {
        let symbol = outline
            .find(anchor)
            .ok_or_else(|| LangError::UnknownSymbol(anchor.clone()))?;
        let lines = source_lines(source);
        let end = last_content_line(&lines, symbol.range);
        let indent = indent_of(line_at(&lines, symbol.range.start)).to_owned();
        let site = |line, indent, blank_before, blank_after| InsertSite {
            line,
            indent,
            blank_before,
            blank_after,
        };
        match where_ {
            InsertWhere::Before => return Ok(site(symbol.range.start, indent, 1, 1)),
            InsertWhere::After => return Ok(site(end + 1, indent, 1, 1)),
            InsertWhere::First | InsertWhere::Last => {}
        }
        let not_container = || LangError::NotAContainer(anchor.clone());
        let object_like = matches!(
            symbol.kind,
            SymbolKind::Variable | SymbolKind::Constant | SymbolKind::Field
        );
        if !(object_like
            || matches!(
                symbol.kind,
                SymbolKind::Class
                    | SymbolKind::Interface
                    | SymbolKind::Namespace
                    | SymbolKind::Enum
                    | SymbolKind::Module
            ))
        {
            return Err(not_container());
        }
        let first = (symbol.body.start as usize - 1).min(lines.len() - 1);
        let close = (end as usize - 1).clamp(first, lines.len() - 1);
        let block = lines[first..=close].join("\n");
        let decl = skip_decorators(&block);
        let (length, stop) = scan_signature(&block[decl..]);
        if stop != Stop::Brace
            || (object_like && !one_line(&block[decl..decl + length]).ends_with('='))
        {
            return Err(not_container());
        }
        let decl_line = first + block[..decl].matches('\n').count();
        let open = first + block[..decl + length].matches('\n').count();
        if open >= close {
            return Err(LangError::Unparseable(format!(
                "{anchor} opens and closes its body on one line"
            )));
        }
        if !lines[close].trim_start().starts_with('}') {
            return Err(LangError::Unparseable(format!(
                "the closing brace of {anchor} shares its line with code"
            )));
        }
        let has_members = lines[open + 1..close]
            .iter()
            .any(|line| !line.trim().is_empty());
        let member_indent = symbol
            .children
            .first()
            .map(|child| indent_of(line_at(&lines, child.range.start)).to_owned())
            .or_else(|| {
                lines[open + 1..close]
                    .iter()
                    .find(|line| !line.trim().is_empty())
                    .map(|line| indent_of(line).to_owned())
            })
            .unwrap_or_else(|| {
                format!(
                    "{}{}",
                    indent_of(lines[decl_line]),
                    indent_unit(&lines, '{', 2)
                )
            });
        Ok(if where_ == InsertWhere::First {
            site(open as u32 + 2, member_indent, 0, u8::from(has_members))
        } else {
            site(close as u32 + 1, member_indent, u8::from(has_members), 0)
        })
    }

    /// `*.test.*`, `*.spec.*` and anything under `__tests__/`, for every script extension.
    fn is_test_file(&self, file: &Path) -> bool {
        let script = file
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| SCRIPT_EXTENSIONS.contains(&extension));
        let name = file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        script
            && (name.contains(".test.")
                || name.contains(".spec.")
                || file
                    .components()
                    .any(|part| part.as_os_str() == "__tests__"))
    }

    /// Runner from the `test_runner` fact (`node` when absent): `npx vitest run`, `npx jest` or
    /// `node --test`. A symbol target runs the distinct files of its referencing tests filtered
    /// by `-t` (`--test-name-pattern=` for node) with their names as an escaped regex
    /// alternation (`describe > it` becomes the runner's space-joined `describe it`); beyond
    /// `MAX_NAMED_TESTS` only the files run. A symbol no test references is
    /// [`LangError::Unsupported`]. A file target runs whole only when the runner would collect
    /// it by convention (`*.test.*`, `*.spec.*`, under `__tests__/`); a directory target selects
    /// the test files inside it; a non-test script file is [`LangError::Unsupported`]. Patterns
    /// pass through unescaped.
    fn test_selection(
        &self,
        project: &LanguageProject,
        target: &TestTarget,
    ) -> Result<TestSelection, LangError> {
        let runner = env_value(project, "test_runner").unwrap_or("node");
        let (tests, files, pattern) = match target {
            TestTarget::Symbol {
                path,
                referencing_tests,
            } => {
                let tests = distinct(referencing_tests);
                if tests.is_empty() {
                    return Err(LangError::Unsupported(format!("no test references {path}")));
                }
                let pattern = (tests.len() <= MAX_NAMED_TESTS).then(|| {
                    tests
                        .iter()
                        .map(|test| regex_escape(&test.name.replace(" > ", " ")))
                        .collect::<Vec<_>>()
                        .join("|")
                });
                let files = distinct_files(&tests);
                (tests, files, pattern)
            }
            TestTarget::File(file) => {
                // A directory (`__tests__/`) selects the test files inside it and passes through
                // unchanged; a plain script file must be one the runner would collect by
                // convention, or the module would be imported top-level as a test target.
                if file
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| SCRIPT_EXTENSIONS.contains(&extension))
                    && !self.is_test_file(file)
                {
                    return Err(LangError::Unsupported(format!("no tests in {}", file.display())));
                }
                (Vec::new(), vec![file.display().to_string()], None)
            }
            TestTarget::Pattern(pattern) => (Vec::new(), Vec::new(), Some(pattern.clone())),
        };
        let mut command: Vec<String> = match runner {
            "vitest" => vec!["npx".into(), "vitest".into(), "run".into()],
            "jest" => vec!["npx".into(), "jest".into()],
            _ => vec!["node".into(), "--test".into()],
        };
        match pattern {
            Some(pattern) if matches!(runner, "vitest" | "jest") => {
                command.extend(files);
                command.extend(["-t".to_owned(), pattern]);
            }
            Some(pattern) => {
                command.push(format!("--test-name-pattern={pattern}"));
                command.extend(files);
            }
            None => command.extend(files),
        }
        Ok(TestSelection { tests, command })
    }

    /// Reads (after stripping ANSI colours) the vitest `Tests  N failed | M passed (T)` or jest
    /// `Tests: N failed, M passed, T total` summary, else node's `# pass N`/`# fail N` (TAP) or
    /// `ℹ pass N` (spec) counts; skipped and todo count as ignored, node's cancelled as failed.
    /// Failures come from vitest `FAIL file > suite > test` sections, jest `● suite › test`
    /// blocks, or node's `not ok` / `✖` entries, each with the first error line and the first
    /// `file:line[:col]` location outside `node_modules`. No summary means incomplete.
    fn parse_test_output(&self, stdout: &str, stderr: &str) -> TestReport {
        let text = strip_ansi(&format!("{stdout}\n{stderr}"));
        let lines: Vec<&str> = text.lines().map(str::trim).collect();
        let summary = lines
            .iter()
            .rev()
            .find_map(|line| runner_summary(line))
            .or_else(|| node_summary(&lines));
        let mut report = TestReport {
            incomplete: summary.is_none(),
            ..TestReport::default()
        };
        if let Some((passed, failed, ignored)) = summary {
            (report.passed, report.failed, report.ignored) = (passed, failed, ignored);
        }
        report.failures = vitest_failures(&lines);
        if report.failures.is_empty() {
            report.failures = jest_failures(&lines);
        }
        if report.failures.is_empty() {
            report.failures = node_failures(&lines);
        }
        report
    }

    /// `npx prettier --write <file>` when prettier is configured, else `None`.
    fn format_command(&self, project: &LanguageProject, file: &Path) -> Option<Vec<String>> {
        (env_value(project, "formatter")? == "prettier").then(|| {
            vec![
                "npx".to_owned(),
                "prettier".to_owned(),
                "--write".to_owned(),
                file.display().to_string(),
            ]
        })
    }

    /// `npx prettier --stdin-filepath <file>`, reading the candidate text on stdin and writing the
    /// formatted text to stdout, when prettier is configured. `None` for a file extension no
    /// script language module owns or a project without prettier.
    fn format_stdin_command(&self, project: &LanguageProject, file: &Path) -> Option<Vec<String>> {
        match file.extension().and_then(|ext| ext.to_str()) {
            Some("ts" | "tsx" | "js" | "jsx" | "mts" | "cts" | "mjs" | "cjs") => {}
            _ => return None,
        }
        (env_value(project, "formatter")? == "prettier").then(|| {
            vec![
                "npx".to_owned(),
                "prettier".to_owned(),
                "--stdin-filepath".to_owned(),
                file.display().to_string(),
            ]
        })
    }
}

/// Appends every string inside a `package.json` value (string, array or nested object values).
fn collect_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => items.iter().for_each(|item| collect_strings(item, out)),
        Value::Object(map) => map.values().for_each(|item| collect_strings(item, out)),
        _ => {}
    }
}

/// Converts siblings under `owner_path`: ordered by first line (the server sorts by name), then
/// adjacent same-named siblings merged into the first. Children of a function-like owner keep
/// only nested declarations; statement-level symbols are dropped (see [`is_body_local`]).
fn convert_all(
    lines: &[&str],
    symbols: Vec<lsp::DocumentSymbol>,
    owner_path: &SymbolPath,
    owner: Option<SymbolKind>,
    test_file: bool,
) -> Vec<Symbol> {
    let in_body = owner.is_some_and(is_body_owner);
    let mut converted: Vec<Symbol> = symbols
        .into_iter()
        .filter(|symbol| !in_body || !is_body_local(symbol.kind))
        .map(|symbol| convert(lines, symbol, owner_path, owner, test_file))
        .collect();
    converted.sort_by_key(|symbol| (symbol.range.start, symbol.body.start));
    let mut merged: Vec<Symbol> = Vec::with_capacity(converted.len());
    for symbol in converted {
        match merged.last_mut() {
            Some(previous) if previous.name == symbol.name => {
                previous.range.end = previous.range.end.max(symbol.range.end);
                previous.children.extend(symbol.children);
            }
            _ => merged.push(symbol),
        }
    }
    merged
}

/// Kinds whose bodies hold no outline entries of their own: their children keep only nested
/// functions, classes, interfaces and enums (plus the members of those).
fn is_body_owner(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Constructor | SymbolKind::Test
    )
}

/// Kinds tsserver reports for statements inside a body — locals, object-literal properties and
/// call/`throw` statements carrying the statement text as their name. None belong in an outline.
fn is_body_local(kind: lsp::SymbolKind) -> bool {
    matches!(
        kind,
        lsp::SymbolKind::VARIABLE
            | lsp::SymbolKind::CONSTANT
            | lsp::SymbolKind::PROPERTY
            | lsp::SymbolKind::FIELD
            | lsp::SymbolKind::OBJECT
            | lsp::SymbolKind::KEY
            | lsp::SymbolKind::STRING
            | lsp::SymbolKind::NUMBER
            | lsp::SymbolKind::BOOLEAN
            | lsp::SymbolKind::ARRAY
            | lsp::SymbolKind::NULL
            | lsp::SymbolKind::ENUM_MEMBER
    )
}

/// Normalizes one server symbol and its children (see [`TypeScript::normalize`]).
fn convert(
    lines: &[&str],
    symbol: lsp::DocumentSymbol,
    owner_path: &SymbolPath,
    owner: Option<SymbolKind>,
    test_file: bool,
) -> Symbol {
    let body = lines_of(&symbol.range);
    let first = (body.start as usize - 1).min(lines.len() - 1);
    let last = (body.end as usize - 1).clamp(first, (first + SCAN_LINES).min(lines.len() - 1));
    let block = lines[first..=last].join("\n");
    let decl = skip_decorators(&block);
    let (length, stop) = scan_signature(&block[decl..]);
    let head = &block[decl..decl + length];

    let mut kind = refine_kind(
        kind_of(symbol.kind),
        strip_modifiers(head),
        &symbol.name,
        owner,
    );
    let test_call = test_file
        && is_test_call(&symbol.name)
        && matches!(
            kind,
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Variable
        );
    if test_call {
        kind = SymbolKind::Test;
    }
    let member = matches!(owner, Some(SymbolKind::Class | SymbolKind::Interface))
        && matches!(
            symbol.kind,
            lsp::SymbolKind::PROPERTY
                | lsp::SymbolKind::FIELD
                | lsp::SymbolKind::VARIABLE
                | lsp::SymbolKind::CONSTANT
        );
    let signature = if test_call {
        symbol.name.trim_end_matches(" callback").to_owned()
    } else if member {
        let decl_line = first + block[..decl].matches('\n').count();
        attribute_signature(line_at(lines, decl_line as u32 + 1))
    } else {
        render_signature(head, stop, kind)
    };
    let (header, jsdoc) = header_start(lines, first);
    let doc = jsdoc.and_then(|(start, end)| jsdoc_paragraph(&lines[start..=end]));

    let path = owner_path.child(&symbol.name);
    let children = convert_all(
        lines,
        symbol.children.unwrap_or_default(),
        &path,
        Some(kind),
        test_file,
    );
    Symbol {
        path,
        kind,
        name: symbol.name,
        range: LineRange::new((header as u32 + 1).min(body.start), body.end),
        body,
        signature,
        doc,
        children,
    }
}

/// Refines the server kind from the declaration keyword and the owner: `type` → type alias,
/// `namespace`/`module` → namespace, `enum`, `interface`; functions and methods owned by a class
/// become methods (`constructor` a constructor).
fn refine_kind(base: SymbolKind, text: &str, name: &str, owner: Option<SymbolKind>) -> SymbolKind {
    let keyword = |word: &str| {
        text.strip_prefix(word)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
    };
    if keyword("type") {
        SymbolKind::TypeAlias
    } else if keyword("namespace") || keyword("module") {
        SymbolKind::Namespace
    } else if keyword("enum") {
        SymbolKind::Enum
    } else if keyword("interface") {
        SymbolKind::Interface
    } else if owner == Some(SymbolKind::Class)
        && matches!(base, SymbolKind::Function | SymbolKind::Method)
    {
        if name == "constructor" {
            SymbolKind::Constructor
        } else {
            SymbolKind::Method
        }
    } else {
        base
    }
}

/// `text` without leading declaration modifiers (`export`, `default`, `declare`, `const`, …).
fn strip_modifiers(mut text: &str) -> &str {
    /// Keywords that may precede the declaration keyword or name.
    const MODIFIERS: [&str; 15] = [
        "export",
        "default",
        "declare",
        "abstract",
        "async",
        "public",
        "private",
        "protected",
        "static",
        "readonly",
        "override",
        "accessor",
        "const",
        "let",
        "var",
    ];
    loop {
        text = text.trim_start();
        let Some(rest) = MODIFIERS.iter().find_map(|modifier| {
            text.strip_prefix(modifier)
                .filter(|rest| rest.starts_with(char::is_whitespace))
        }) else {
            return text;
        };
        text = rest;
    }
}

/// Whether a server symbol name is a test-framework callback: `describe("math") callback`,
/// `it.each(…) callback`, `test("x") callback`.
fn is_test_call(name: &str) -> bool {
    name.split_once('(').is_some_and(|(callee, _)| {
        matches!(
            callee.split('.').next(),
            Some("describe" | "it" | "test" | "suite")
        )
    })
}

/// Why [`scan_signature`] stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stop {
    /// At the `{` that opens a body (function, class, namespace, object literal).
    Brace,
    /// At the `=` of a depth-0 `=>`.
    Arrow,
    /// At a depth-0 `;`.
    Semicolon,
    /// The text ran out.
    End,
}

/// Scans a declaration for the end of its signature and returns the byte length before the
/// stop. Strings, template literals and comments are skipped; `(`/`[`, type-argument `<…>` and
/// type-literal `{…}` (a `{` after `:`, `|`, `&`, `,` or `<`) raise the depth, and only depth-0
/// stops count.
fn scan_signature(text: &str) -> (usize, Stop) {
    let bytes = text.as_bytes();
    let (mut round, mut angle, mut curly) = (0i32, 0i32, 0i32);
    let mut previous = b' ';
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        let flat = round == 0 && angle == 0 && curly == 0;
        match byte {
            b'\'' | b'"' | b'`' => {
                index = string_end(bytes, index);
                previous = byte;
                continue;
            }
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index = text[index..]
                    .find('\n')
                    .map_or(bytes.len(), |end| index + end);
                continue;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = text[index + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |end| index + end + 4);
                continue;
            }
            b'(' | b'[' => round += 1,
            b')' | b']' => round -= 1,
            b'<' if previous.is_ascii_alphanumeric() || matches!(previous, b'_' | b'$') => {
                angle += 1
            }
            b'>' if index > 0 && bytes[index - 1] == b'=' => {
                if flat {
                    return (index - 1, Stop::Arrow);
                }
            }
            b'>' if angle > 0 => angle -= 1,
            b'{' if flat && !matches!(previous, b':' | b'|' | b'&' | b',' | b'<') => {
                return (index, Stop::Brace);
            }
            b'{' => curly += 1,
            b'}' => curly -= 1,
            b';' if flat => return (index, Stop::Semicolon),
            _ => {}
        }
        if !byte.is_ascii_whitespace() {
            previous = byte;
        }
        index += 1;
    }
    (bytes.len(), Stop::End)
}

/// Index just past the string literal opening at `start` (escapes honoured, `${}` not nested).
fn string_end(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            byte if byte == quote => return index + 1,
            _ => index += 1,
        }
    }
    bytes.len()
}

/// Byte offset where the declaration starts after leading decorators (`@name`, `@a.b(…)`,
/// possibly spanning lines; a declaration on the decorator's own line is found too).
fn skip_decorators(text: &str) -> usize {
    let bytes = text.as_bytes();
    let whitespace = |from: usize| from + text[from..].len() - text[from..].trim_start().len();
    let mut index = whitespace(0);
    while bytes.get(index) == Some(&b'@') {
        index += 1;
        while index < bytes.len()
            && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'_' | b'$' | b'.'))
        {
            index += 1;
        }
        if bytes.get(index) == Some(&b'(') {
            index = matching_paren(text, index).unwrap_or(bytes.len());
        }
        index = whitespace(index);
    }
    index
}

/// Index just past the `)` matching the `(` at `open`, skipping strings; `None` if unmatched.
fn matching_paren(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0;
    let mut index = open;
    while index < bytes.len() {
        match bytes[index] {
            b'\'' | b'"' | b'`' => {
                index = string_end(bytes, index);
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// One-line signature from the scanned head: arrows render as `(args) => Return` (or a trailing
/// `=>` without a return annotation), braced type aliases as `type X = {…}`, and a trailing `;`
/// or `,` is dropped.
fn render_signature(head: &str, stop: Stop, kind: SymbolKind) -> String {
    let text = match stop {
        Stop::Arrow => arrow_signature(head),
        Stop::Brace if kind == SymbolKind::TypeAlias => format!("{} {{…}}", one_line(head)),
        _ => one_line(head),
    };
    text.trim_end_matches([';', ',']).trim_end().to_owned()
}

/// `const f = async (a: A): R` → `const f = async (a: A) => R`; without a return annotation the
/// head is kept with a trailing `=>`.
fn arrow_signature(head: &str) -> String {
    let mut depth = 0;
    let mut close = None;
    for (index, byte) in head.bytes().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(index);
                }
            }
            _ => {}
        }
    }
    if let Some(close) = close
        && let Some(result) = head[close + 1..].trim().strip_prefix(':')
    {
        return format!("{} => {}", one_line(&head[..=close]), one_line(result));
    }
    format!("{} =>", one_line(head))
}

/// Signature of a class/interface field or property: the declaration up to its first top-level
/// `=` when the name carries a type annotation (`name: Type`), otherwise the whole
/// `name = value`; `//` comments are stripped, the `=` of `=>` never cuts, and the result is
/// clipped at [`MAX_ATTRIBUTE_CHARS`] characters.
fn attribute_signature(line: &str) -> String {
    let line = line.trim_start();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut annotated = false;
    let mut cut = line.len();
    let mut prev = ' ';
    for (col, ch) in line.char_indices() {
        if let Some(open) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == open {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' | '`' => quote = Some(ch),
            '/' if line[col + 1..].starts_with('/') => {
                cut = col;
                break;
            }
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ':' if depth <= 0 && prev != '=' => annotated = true,
            '=' if depth <= 0
                && annotated
                && !matches!(prev, '=' | '!' | '<' | '>')
                && !line[col + 1..].starts_with(['=', '>']) =>
            {
                cut = col;
                break;
            }
            _ => {}
        }
        prev = ch;
    }
    clip(
        &one_line(line[..cut].trim_end().trim_end_matches(';')),
        MAX_ATTRIBUTE_CHARS,
    )
}

/// First header line index above (or at) `first`, and the nearest JSDoc block's line span.
/// Decorator lines at the declaration's indentation are claimed, with deeper or closing-bracket
/// lines between them (multi-line decorator arguments); a JSDoc block is claimed only when it
/// sits directly above what is already claimed. A blank line ends the header.
fn header_start(lines: &[&str], first: usize) -> (usize, Option<(usize, usize)>) {
    let indent = indent_of(lines[first]).len();
    let mut start = first;
    let mut jsdoc = None;
    let mut pending = false;
    let mut index = first;
    while index > 0 {
        index -= 1;
        let text = lines[index];
        let trimmed = text.trim();
        let width = indent_of(text).len();
        if trimmed.is_empty() {
            break;
        }
        if !pending && trimmed.ends_with("*/") {
            let Some(open) = (0..=index).rev().find(|&line| lines[line].contains("/*")) else {
                break;
            };
            if !lines[open].trim_start().starts_with("/**") {
                break;
            }
            jsdoc.get_or_insert((open, index));
            start = open;
            index = open;
        } else if width == indent && trimmed.starts_with('@') {
            start = index;
            pending = false;
        } else if width > indent || (width == indent && trimmed.starts_with([')', ']', '}'])) {
            pending = true;
        } else {
            break;
        }
    }
    (start, jsdoc)
}

/// First paragraph of a JSDoc block: `*` gutters stripped, up to a blank line or an `@tag`.
fn jsdoc_paragraph(block: &[&str]) -> Option<String> {
    let text = block.join("\n");
    let inner = text.trim().strip_prefix("/**")?.strip_suffix("*/")?;
    let words: Vec<&str> = inner
        .lines()
        .map(|line| {
            let line = line.trim();
            line.strip_prefix('*').unwrap_or(line).trim()
        })
        .skip_while(|line| line.is_empty())
        .take_while(|line| !line.is_empty() && !line.starts_with('@'))
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

/// `text` with regex metacharacters backslash-escaped, for `-t`/`--test-name-pattern`.
fn regex_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if "\\.^$|?*+()[]{}".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// `text` without ANSI CSI escape sequences (colours, cursor moves).
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
        } else if chars.next() == Some('[') {
            for code in chars.by_ref() {
                if code.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    out
}

/// Adds `count` to `(passed, failed, ignored)` by the runner's word for it.
fn tally(totals: &mut (u32, u32, u32), count: u32, word: &str) {
    match word {
        "passed" | "pass" => totals.0 += count,
        "failed" | "fail" | "cancelled" => totals.1 += count,
        "skipped" | "skip" | "todo" | "pending" => totals.2 += count,
        _ => {}
    }
}

/// vitest `Tests  1 failed | 3 passed (4)` or jest `Tests: 1 failed, 3 passed, 4 total`
/// (`Test Files`/`Test Suites` lines do not match).
fn runner_summary(line: &str) -> Option<(u32, u32, u32)> {
    let (items, separator) = if let Some(rest) = line.strip_prefix("Tests:") {
        (rest, ',')
    } else {
        (line.strip_prefix("Tests ")?, '|')
    };
    let mut totals = (0, 0, 0);
    let mut any = false;
    for item in items.split(separator) {
        let mut words = item.split_whitespace();
        if let (Some(count), Some(word)) = (words.next(), words.next())
            && let Ok(count) = count.parse()
        {
            tally(&mut totals, count, word);
            any = true;
        }
    }
    any.then_some(totals)
}

/// node --test counts from TAP `# pass N` or spec `ℹ pass N` lines; `None` without a pass or
/// fail line.
fn node_summary(lines: &[&str]) -> Option<(u32, u32, u32)> {
    let mut totals = (0, 0, 0);
    let mut any = false;
    for line in lines {
        let Some(rest) = line.strip_prefix("# ").or_else(|| line.strip_prefix("ℹ ")) else {
            continue;
        };
        if let Some((word, count)) = rest.split_once(' ')
            && let Ok(count) = count.trim().parse()
        {
            any |= matches!(word, "pass" | "fail");
            tally(&mut totals, count, word);
        }
    }
    any.then_some(totals)
}

/// Whether a line is an error headline: `AssertionError: …`, `TypeError: …`, `Error: …`.
fn is_error_line(line: &str) -> bool {
    line.split_once(':').is_some_and(|(head, _)| {
        head.ends_with("Error") && head.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// `path:line[:col]` (optionally wrapped in parentheses or quotes, `file://` stripped).
fn split_location(text: &str) -> Option<(PathBuf, u32)> {
    let text = text.trim().trim_matches(['(', ')', '\'', '"']);
    let text = text.strip_prefix("file://").unwrap_or(text);
    let (rest, last) = text.rsplit_once(':')?;
    let last: u32 = last.parse().ok()?;
    if let Some((file, line)) = rest.rsplit_once(':')
        && let Ok(line) = line.parse()
        && !file.is_empty()
    {
        return Some((PathBuf::from(file), line));
    }
    (!rest.is_empty()).then(|| (PathBuf::from(rest), last))
}

/// Records a failure unless one with the same name exists (runners print each twice); a
/// repeat fills a missing location.
fn push_failure(failures: &mut Vec<TestFailure>, failure: TestFailure) {
    match failures.iter_mut().find(|known| known.name == failure.name) {
        Some(known) if known.location.is_none() => known.location = failure.location,
        Some(_) => {}
        None => failures.push(failure),
    }
}

/// The error headline (else the first non-blank line) of a failure block.
fn block_message(block: &[&str]) -> String {
    block
        .iter()
        .find(|line| is_error_line(line))
        .or_else(|| block.iter().find(|line| !line.is_empty()))
        .map_or_else(String::new, |line| (*line).to_owned())
}

/// vitest `FAIL  file > suite > test` sections, each up to the next `FAIL` or `⎯` rule.
fn vitest_failures(lines: &[&str]) -> Vec<TestFailure> {
    let mut failures = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(rest) = line.strip_prefix("FAIL ") else {
            continue;
        };
        let mut parts = rest.trim().split(" > ");
        parts.next();
        let name = parts.collect::<Vec<_>>().join(" > ");
        if name.is_empty() {
            continue;
        }
        let block: Vec<&str> = lines[index + 1..]
            .iter()
            .copied()
            .take_while(|line| !line.starts_with("FAIL ") && !line.starts_with('⎯'))
            .collect();
        let location = block
            .iter()
            .filter_map(|line| line.strip_prefix("❯ "))
            .filter_map(split_location)
            .find(|(path, _)| !path.starts_with("node_modules"));
        push_failure(
            &mut failures,
            TestFailure {
                name,
                location,
                message: block_message(&block),
            },
        );
    }
    failures
}

/// jest `● suite › test` blocks, each up to the next `●` or the summary; `›` becomes `>`.
fn jest_failures(lines: &[&str]) -> Vec<TestFailure> {
    let mut failures = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(name) = line.strip_prefix("● ") else {
            continue;
        };
        let block: Vec<&str> = lines[index + 1..]
            .iter()
            .copied()
            .take_while(|line| {
                !line.starts_with("● ")
                    && !line.starts_with("Tests:")
                    && !line.starts_with("Test Suites:")
            })
            .collect();
        let location = block
            .iter()
            .filter_map(|line| line.strip_prefix("at "))
            .filter(|frame| !frame.contains("node_modules") && !frame.contains("node:"))
            .find_map(|frame| {
                let inner = frame.rsplit_once('(').map_or(frame, |(_, inner)| inner);
                split_location(inner)
            });
        push_failure(
            &mut failures,
            TestFailure {
                name: name.trim().replace(" › ", " > "),
                location,
                message: block_message(&block),
            },
        );
    }
    failures
}

/// node --test failures: TAP `not ok N - name` with its YAML `location:`/`error:` (suites
/// failing only through subtests are skipped), or spec `✖ name (…ms)` entries with the error
/// line below and a `test at file:line:col` line above.
fn node_failures(lines: &[&str]) -> Vec<TestFailure> {
    let mut failures = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if let Some(rest) = line.strip_prefix("not ok ") {
            let name = rest.split_once(" - ").map_or(rest, |(_, name)| name);
            let block: Vec<&str> = lines[index + 1..]
                .iter()
                .copied()
                .take_while(|line| {
                    *line != "..." && !line.starts_with("ok ") && !line.starts_with("not ok ")
                })
                .collect();
            if block.iter().any(|line| line.contains("subtestsFailed")) {
                continue;
            }
            let location = block
                .iter()
                .find_map(|line| line.strip_prefix("location:"))
                .and_then(split_location);
            let message = block
                .iter()
                .position(|line| line.starts_with("error:"))
                .map(|at| {
                    let value = block[at]["error:".len()..].trim();
                    if value.starts_with('|') || value.starts_with('>') {
                        block.get(at + 1).copied().unwrap_or_default().to_owned()
                    } else {
                        value.trim_matches(['\'', '"']).to_owned()
                    }
                })
                .unwrap_or_default();
            push_failure(
                &mut failures,
                TestFailure {
                    name: name.split(" # ").next().unwrap_or(name).trim().to_owned(),
                    location,
                    message,
                },
            );
        } else if let Some(rest) = line.strip_prefix("✖ ")
            && !rest.ends_with(':')
        {
            let name = rest.rsplit_once(" (").map_or(rest, |(name, _)| name);
            let location = index
                .checked_sub(1)
                .and_then(|above| lines[above].strip_prefix("test at "))
                .and_then(split_location);
            let block: Vec<&str> = lines[index + 1..]
                .iter()
                .copied()
                .take_while(|line| !line.starts_with('✖') && !line.starts_with("test at "))
                .collect();
            push_failure(
                &mut failures,
                TestFailure {
                    name: name.trim().to_owned(),
                    location,
                    message: block
                        .iter()
                        .find(|line| !line.is_empty())
                        .map_or_else(String::new, |line| (*line).to_owned()),
                },
            );
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    //! Fixtures mirror typescript-language-server 6 over tsserver's navigation tree: one symbol
    //! per declaration span (overloads repeat the name), class members nested and sorted by
    //! name, spans starting at decorators but not at JSDoc, type aliases reported as variables.

    use std::fs;

    use super::*;
    use crate::lang::TestId;

    /// A module with a decorated class, overloads, an arrow const, a type alias, an interface
    /// and an object literal.
    const SHAPES: &str = "\
import { Base } from \"./base\";

/**
 * Shapes registry.
 *
 * Details.
 */
@sealed
@Component({
  selector: \"app\",
})
export class Registry<T extends { id: string }> extends Base {
  private items: T[] = [];

  constructor(private readonly name: string) {
    super();
  }

  /** Adds an item. */
  add(item: T): void {
    this.items.push(item);
  }
}

export function area(shape: Shape): number;
export function area(shape: Shape, scale: number): number;
export function area(
  shape: Shape,
  scale = 1,
): number {
  return shape.size * scale;
}

export const double = async (n: number): Promise<number> => n * 2;

export type Point = { x: number; y: number };

export interface Shape {
  size: number;
}

export const config = {
  debug: false,
};
";

    /// A vitest file with a nested suite.
    const MATH_TEST: &str = "\
import { describe, it, expect } from \"vitest\";

describe(\"math\", () => {
  it(\"adds\", () => {
    expect(1 + 1).toBe(2);
  });
});
";

    /// A module mirroring the reported live output: a function holding a local `const`, an
    /// object literal with two properties and a call expression, and a nested function; a class
    /// with a field, a constructor and a method; an interface with two members.
    const TRANSPORT: &str = "\
export function post(path: string, body: unknown): void {
  const requestId = newRequestId();
  const init = {
    method: \"POST\",
    headers: { \"content-type\": \"application/json\" },
  };
  reportDiagnostic(\"post\", requestId);
  function retry(attempt: number): void {
    const delay = attempt * 2;
  }
  retry(1);
}

export class Client {
  private retries = 3;

  constructor(private readonly url: string) {
    const probe = open(url);
  }

  fetch(path: string): void {
    const query = \"*\";
  }
}

export interface Options {
  retries: number;
  backoff: (attempt: number) => number;
}
";

    /// Builds a document symbol over 0-based `(start line, start char, end line, end char)`.
    #[allow(deprecated)]
    fn symbol(
        name: &str,
        kind: lsp::SymbolKind,
        (start_line, start_char, end_line, end_char): (u32, u32, u32, u32),
        children: Vec<lsp::DocumentSymbol>,
    ) -> lsp::DocumentSymbol {
        let range = lsp::Range::new(
            lsp::Position::new(start_line, start_char),
            lsp::Position::new(end_line, end_char),
        );
        lsp::DocumentSymbol {
            name: name.to_owned(),
            detail: Some(String::new()),
            kind,
            tags: None,
            deprecated: None,
            range,
            selection_range: range,
            children: (!children.is_empty()).then_some(children),
        }
    }

    /// The server's symbols for [`SHAPES`] (top level and members sorted by name).
    fn shapes_symbols() -> Vec<lsp::DocumentSymbol> {
        use lsp::SymbolKind as K;
        vec![
            symbol("area", K::FUNCTION, (24, 0, 24, 43), vec![]),
            symbol("area", K::FUNCTION, (25, 0, 25, 58), vec![]),
            symbol("area", K::FUNCTION, (26, 0, 31, 1), vec![]),
            symbol(
                "config",
                K::VARIABLE,
                (41, 13, 43, 1),
                vec![symbol("debug", K::PROPERTY, (42, 2, 42, 14), vec![])],
            ),
            symbol("double", K::VARIABLE, (33, 13, 33, 65), vec![]),
            symbol("Point", K::VARIABLE, (35, 0, 35, 45), vec![]),
            symbol(
                "Registry",
                K::CLASS,
                (7, 0, 22, 1),
                vec![
                    symbol("add", K::METHOD, (19, 2, 21, 3), vec![]),
                    symbol("constructor", K::CONSTRUCTOR, (14, 2, 16, 3), vec![]),
                    symbol("items", K::PROPERTY, (12, 2, 12, 26), vec![]),
                ],
            ),
            symbol(
                "Shape",
                K::INTERFACE,
                (37, 0, 39, 1),
                vec![symbol("size", K::PROPERTY, (38, 2, 38, 15), vec![])],
            ),
        ]
    }

    /// Normalized outline of [`SHAPES`] at `src/shapes.ts`.
    fn shapes_outline() -> Outline {
        TypeScript.normalize(Path::new("src/shapes.ts"), SHAPES, shapes_symbols())
    }

    /// The server's symbols for [`TRANSPORT`]: locals, object-literal properties and a call
    /// expression as statement symbols inside bodies, members sorted by name.
    fn transport_symbols() -> Vec<lsp::DocumentSymbol> {
        use lsp::SymbolKind as K;
        vec![
            symbol(
                "Client",
                K::CLASS,
                (13, 0, 23, 1),
                vec![
                    symbol(
                        "constructor",
                        K::CONSTRUCTOR,
                        (16, 2, 18, 3),
                        vec![symbol("probe", K::VARIABLE, (17, 4, 17, 26), vec![])],
                    ),
                    symbol(
                        "fetch",
                        K::METHOD,
                        (20, 2, 22, 3),
                        vec![symbol("query", K::VARIABLE, (21, 4, 21, 22), vec![])],
                    ),
                    symbol("retries", K::PROPERTY, (14, 2, 14, 22), vec![]),
                ],
            ),
            symbol(
                "Options",
                K::INTERFACE,
                (25, 0, 28, 1),
                vec![
                    symbol("backoff", K::PROPERTY, (27, 2, 27, 39), vec![]),
                    symbol("retries", K::PROPERTY, (26, 2, 26, 17), vec![]),
                ],
            ),
            symbol(
                "post",
                K::FUNCTION,
                (0, 0, 11, 1),
                vec![
                    symbol(
                        "init",
                        K::VARIABLE,
                        (2, 2, 5, 3),
                        vec![
                            symbol(
                                "headers: { \"content-type\": \"application/json\" }",
                                K::PROPERTY,
                                (4, 4, 4, 53),
                                vec![],
                            ),
                            symbol("method: \"POST\"", K::PROPERTY, (3, 4, 3, 19), vec![]),
                        ],
                    ),
                    symbol("requestId", K::VARIABLE, (1, 2, 1, 34), vec![]),
                    symbol(
                        "reportDiagnostic(\"post\", requestId)",
                        K::CONSTANT,
                        (6, 2, 6, 37),
                        vec![],
                    ),
                    symbol(
                        "retry",
                        K::FUNCTION,
                        (7, 2, 9, 3),
                        vec![symbol("delay", K::VARIABLE, (8, 4, 8, 30), vec![])],
                    ),
                ],
            ),
        ]
    }

    /// The top-level symbol named `name`.
    fn top<'a>(outline: &'a Outline, name: &str) -> &'a Symbol {
        outline
            .symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap()
    }

    /// JSDoc above decorators extends the range; decorators leave the signature.
    #[test]
    fn normalize_extends_jsdoc_and_decorator_headers() {
        let outline = shapes_outline();
        let registry = top(&outline, "Registry");
        assert_eq!(registry.kind, SymbolKind::Class);
        assert_eq!(registry.range, LineRange::new(3, 23));
        assert_eq!(registry.body, LineRange::new(8, 23));
        assert_eq!(
            registry.signature,
            "export class Registry<T extends { id: string }> extends Base"
        );
        assert_eq!(registry.doc.as_deref(), Some("Shapes registry."));
        let members: Vec<(&str, SymbolKind)> = registry
            .children
            .iter()
            .map(|member| (member.name.as_str(), member.kind))
            .collect();
        assert_eq!(
            members,
            [
                ("items", SymbolKind::Field),
                ("constructor", SymbolKind::Constructor),
                ("add", SymbolKind::Method),
            ]
        );
        let constructor = &registry.children[1];
        assert_eq!(
            constructor.signature,
            "constructor(private readonly name: string)"
        );
        assert_eq!(registry.children[0].signature, "private items: T[]");
        let add = &registry.children[2];
        assert_eq!(add.range, LineRange::new(19, 22));
        assert_eq!(add.signature, "add(item: T): void");
        assert_eq!(add.doc.as_deref(), Some("Adds an item."));
        assert_eq!(add.path.to_string(), "src/shapes.ts#Registry/add");
    }

    /// Overloads merge into the first; arrow consts, type aliases, interfaces render as specified.
    #[test]
    fn normalize_merges_overloads_and_renders_signatures() {
        let outline = shapes_outline();
        let names: Vec<&str> = outline.symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            ["Registry", "area", "double", "Point", "Shape", "config"]
        );
        let area = top(&outline, "area");
        assert_eq!(area.kind, SymbolKind::Function);
        assert_eq!(area.range, LineRange::new(25, 32));
        assert_eq!(area.body, LineRange::new(25, 25));
        assert_eq!(area.signature, "export function area(shape: Shape): number");
        let double = top(&outline, "double");
        assert_eq!(
            double.signature,
            "export const double = async (n: number) => Promise<number>"
        );
        let point = top(&outline, "Point");
        assert_eq!(point.kind, SymbolKind::TypeAlias);
        assert_eq!(point.signature, "export type Point = {…}");
        let shape = top(&outline, "Shape");
        assert_eq!(shape.kind, SymbolKind::Interface);
        assert_eq!(shape.signature, "export interface Shape");
        assert_eq!(shape.children[0].signature, "size: number");
        assert_eq!(top(&outline, "config").signature, "export const config =");
    }

    /// Multi-line parameters collapse; a function without JSDoc has no doc.
    #[test]
    fn normalize_collapses_multiline_parameters() {
        let source = "function load(\n  path: string,\n  retries = 3,\n): Promise<void> {\n}\n";
        let symbols = vec![symbol(
            "load",
            lsp::SymbolKind::FUNCTION,
            (0, 0, 4, 1),
            vec![],
        )];
        let outline = TypeScript.normalize(Path::new("a.js"), source, symbols);
        let load = &outline.symbols[0];
        assert_eq!(
            load.signature,
            "function load(path: string, retries = 3): Promise<void>"
        );
        assert_eq!(load.doc, None);
    }

    /// describe/it callbacks in test files are tests with the call as signature.
    #[test]
    fn normalize_marks_test_callbacks() {
        let symbols = vec![symbol(
            "describe(\"math\") callback",
            lsp::SymbolKind::FUNCTION,
            (2, 17, 6, 1),
            vec![symbol(
                "it(\"adds\") callback",
                lsp::SymbolKind::FUNCTION,
                (3, 13, 5, 3),
                vec![],
            )],
        )];
        let outline =
            TypeScript.normalize(Path::new("src/math.test.ts"), MATH_TEST, symbols.clone());
        let suite = &outline.symbols[0];
        assert_eq!(suite.kind, SymbolKind::Test);
        assert_eq!(suite.signature, "describe(\"math\")");
        assert_eq!(suite.children[0].kind, SymbolKind::Test);
        let outline = TypeScript.normalize(Path::new("src/math.ts"), MATH_TEST, symbols);
        assert_eq!(outline.symbols[0].kind, SymbolKind::Function);
    }

    /// Bodies shrink to their nested declarations; class fields and interface members render
    /// annotation-or-value; no local, object-literal property or statement symbol survives.
    #[test]
    fn normalize_drops_body_statements_and_renders_members() {
        use crate::lang::render::outline_text;
        let outline = TypeScript.normalize(
            Path::new("src/transport.ts"),
            TRANSPORT,
            transport_symbols(),
        );
        assert_eq!(
            outline_text(&outline),
            "\
src/transport.ts  (29 lines, typescript)
    1  export function post(path: string, body: unknown): void
    8    function retry(attempt: number): void
   14  export class Client
   15    private retries = 3
   17    constructor(private readonly url: string)
   21    fetch(path: string): void
   26  export interface Options
   27    retries: number
   28    backoff: (attempt: number) => number
  (9 symbols)
"
        );
    }

    /// Before/After with one blank line; First/Last inside class and object literal braces.
    #[test]
    fn insert_sites_use_braces_and_member_indent() {
        let outline = shapes_outline();
        let at = |path: &str, where_| {
            TypeScript.insert_site(SHAPES, &outline, &SymbolPath::parse(path).unwrap(), where_)
        };
        let site = |line, indent: &str, blank_before, blank_after| InsertSite {
            line,
            indent: indent.to_owned(),
            blank_before,
            blank_after,
        };
        assert_eq!(at("Registry", InsertWhere::Before), Ok(site(3, "", 1, 1)));
        assert_eq!(at("area", InsertWhere::After), Ok(site(33, "", 1, 1)));
        assert_eq!(
            at("Registry/add", InsertWhere::Before),
            Ok(site(19, "  ", 1, 1))
        );
        assert_eq!(at("Registry", InsertWhere::First), Ok(site(13, "  ", 0, 1)));
        assert_eq!(at("Registry", InsertWhere::Last), Ok(site(23, "  ", 1, 0)));
        assert_eq!(at("config", InsertWhere::First), Ok(site(43, "  ", 0, 1)));
        assert_eq!(at("config", InsertWhere::Last), Ok(site(44, "  ", 1, 0)));
        assert_eq!(at("Shape", InsertWhere::Last), Ok(site(40, "  ", 1, 0)));
        assert!(matches!(
            at("area", InsertWhere::First),
            Err(LangError::NotAContainer(_))
        ));
        assert!(matches!(
            at("double", InsertWhere::Last),
            Err(LangError::NotAContainer(_))
        ));
        assert!(matches!(
            at("Nope", InsertWhere::Before),
            Err(LangError::UnknownSymbol(_))
        ));
    }

    /// An empty class takes the container indent plus the file's unit.
    #[test]
    fn insert_first_in_empty_class_uses_file_unit() {
        let source = "namespace N {\n    export const a = 1;\n}\nclass Empty {\n}\n";
        let symbols = vec![
            symbol("N", lsp::SymbolKind::MODULE, (0, 0, 2, 1), vec![]),
            symbol("Empty", lsp::SymbolKind::CLASS, (3, 0, 4, 1), vec![]),
        ];
        let outline = TypeScript.normalize(Path::new("a.ts"), source, symbols);
        assert_eq!(outline.symbols[0].kind, SymbolKind::Namespace);
        let site = TypeScript
            .insert_site(
                source,
                &outline,
                &SymbolPath::parse("Empty").unwrap(),
                InsertWhere::First,
            )
            .unwrap();
        assert_eq!(
            site,
            InsertSite {
                line: 5,
                indent: "    ".to_owned(),
                blank_before: 0,
                blank_after: 0
            }
        );
    }

    /// Test file conventions, JavaScript variants included.
    #[test]
    fn test_files_follow_runner_conventions() {
        for file in [
            "a.test.ts",
            "b.spec.tsx",
            "src/__tests__/c.js",
            "d.test.mjs",
        ] {
            assert!(TypeScript.is_test_file(Path::new(file)), "{file}");
        }
        for file in ["src/a.ts", "a.test.json", "tests/a.ts"] {
            assert!(!TypeScript.is_test_file(Path::new(file)), "{file}");
        }
    }

    /// A project with only the given environment facts.
    fn project(environment: &[(&str, &str)]) -> LanguageProject {
        LanguageProject {
            language: Language::TypeScript,
            manifests: vec![PathBuf::from("package.json")],
            environment: environment
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            interpreter: None,
            commands: ProjectCommands::default(),
            entry_points: Vec::new(),
        }
    }

    /// `argv` from string literals.
    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    /// Runner choice and argument shapes for symbol, file and pattern targets.
    #[test]
    fn test_selection_uses_detected_runner() {
        let target = TestTarget::Symbol {
            path: SymbolPath::parse("src/math.ts#add").unwrap(),
            referencing_tests: vec![
                TestId {
                    file: PathBuf::from("src/math.test.ts"),
                    name: "math > adds (1.5)".to_owned(),
                },
                TestId {
                    file: PathBuf::from("src/math.test.ts"),
                    name: "math > subtracts".to_owned(),
                },
            ],
        };
        let vitest = TypeScript
            .test_selection(&project(&[("test_runner", "vitest")]), &target)
            .unwrap();
        assert_eq!(vitest.tests.len(), 2);
        assert_eq!(
            vitest.command,
            argv(&[
                "npx",
                "vitest",
                "run",
                "src/math.test.ts",
                "-t",
                "math adds \\(1\\.5\\)|math subtracts",
            ])
        );
        let file = TestTarget::File(PathBuf::from("src/a.test.ts"));
        let jest = TypeScript
            .test_selection(&project(&[("test_runner", "jest")]), &file)
            .unwrap();
        assert_eq!(jest.command, argv(&["npx", "jest", "src/a.test.ts"]));
        let pattern = TestTarget::Pattern("adds".to_owned());
        let jest = TypeScript
            .test_selection(&project(&[("test_runner", "jest")]), &pattern)
            .unwrap();
        assert_eq!(jest.command, argv(&["npx", "jest", "-t", "adds"]));
        let node = TypeScript.test_selection(&project(&[]), &pattern).unwrap();
        assert_eq!(
            node.command,
            argv(&["node", "--test", "--test-name-pattern=adds"])
        );
        let node = TypeScript.test_selection(&project(&[]), &file).unwrap();
        assert_eq!(node.command, argv(&["node", "--test", "src/a.test.ts"]));
    }

    /// Non-test script files never become runner targets; directories pass through and let the
    /// runner select the test files inside them.
    #[test]
    fn test_selection_refuses_non_test_file_paths() {
        let plain = TestTarget::File(PathBuf::from("src/details.tsx"));
        assert!(matches!(
            TypeScript.test_selection(&project(&[]), &plain),
            Err(LangError::Unsupported(message)) if message == "no tests in src/details.tsx"
        ));
        let directory = TestTarget::File(PathBuf::from("src/__tests__"));
        assert_eq!(
            TypeScript
                .test_selection(&project(&[]), &directory)
                .unwrap()
                .command,
            argv(&["node", "--test", "src/__tests__"])
        );
        let jsx = TestTarget::File(PathBuf::from("src/app.jsx"));
        assert!(matches!(
            TypeScript.test_selection(&project(&[]), &jsx),
            Err(LangError::Unsupported(_))
        ));
        let tested = TestTarget::File(PathBuf::from("src/details.test.tsx"));
        assert_eq!(
            TypeScript
                .test_selection(&project(&[]), &tested)
                .unwrap()
                .command,
            argv(&["node", "--test", "src/details.test.tsx"])
        );
        assert!(TypeScript.is_test_file(&PathBuf::from("__tests__/helpers.js")));
        assert!(!TypeScript.is_test_file(&PathBuf::from("src/app.mjs")));
    }

    /// A passing vitest run with colours.
    #[test]
    fn parse_vitest_pass() {
        let output = " \u{1b}[32m✓\u{1b}[39m src/math.test.ts (2 tests) 3ms\n\n Test Files  1 passed (1)\n      Tests  2 passed (2)\n";
        let report = TypeScript.parse_test_output(output, "");
        assert_eq!((report.passed, report.failed, report.ignored), (2, 0, 0));
        assert!(!report.incomplete);
        assert!(report.failures.is_empty());
    }

    /// A failing vitest run: suite path, assertion message, `❯` location.
    #[test]
    fn parse_vitest_failure() {
        let output = "\
 ❯ src/math.test.ts (2 tests | 1 failed) 5ms
   × math > adds 3ms

⎯⎯⎯⎯⎯⎯⎯ Failed Tests 1 ⎯⎯⎯⎯⎯⎯⎯

 FAIL  src/math.test.ts > math > adds
AssertionError: expected 3 to be 4 // Object.is equality

- Expected
+ Received

 ❯ src/math.test.ts:6:17
      5|   it(\"adds\", () => {

⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯[1/1]⎯

 Test Files  1 failed (1)
      Tests  1 failed | 1 passed | 1 skipped (3)
";
        let report = TypeScript.parse_test_output(output, "");
        assert_eq!((report.passed, report.failed, report.ignored), (1, 1, 1));
        assert_eq!(
            report.failures,
            [TestFailure {
                name: "math > adds".to_owned(),
                location: Some((PathBuf::from("src/math.test.ts"), 6)),
                message: "AssertionError: expected 3 to be 4 // Object.is equality".to_owned(),
            }]
        );
    }

    /// A failing jest run: `●` block, first message line, first project frame.
    #[test]
    fn parse_jest_failure() {
        let output = "\
FAIL src/math.test.ts
  ● math › adds

    expect(received).toBe(expected) // Object.is equality

    Expected: 4
    Received: 3

      at Object.toBe (node_modules/expect/build/index.js:1:1)
      at Object.<anonymous> (src/math.test.ts:5:23)

Test Suites: 1 failed, 1 total
Tests:       1 failed, 3 passed, 4 total
";
        let report = TypeScript.parse_test_output("", output);
        assert_eq!((report.passed, report.failed, report.ignored), (3, 1, 0));
        assert_eq!(
            report.failures,
            [TestFailure {
                name: "math > adds".to_owned(),
                location: Some((PathBuf::from("src/math.test.ts"), 5)),
                message: "expect(received).toBe(expected) // Object.is equality".to_owned(),
            }]
        );
    }

    /// node --test TAP output: counts and the failing test with location and error.
    #[test]
    fn parse_node_tap_failure() {
        let output = "\
TAP version 13
# Subtest: adds
not ok 1 - adds
  ---
  duration_ms: 0.7
  location: '/work/test/math.test.js:4:1'
  failureType: 'testCodeFailure'
  error: 'Expected values to be strictly equal'
  code: 'ERR_ASSERTION'
  ...
ok 2 - subtracts
1..2
# tests 2
# pass 1
# fail 1
# skipped 0
# todo 1
";
        let report = TypeScript.parse_test_output(output, "");
        assert_eq!((report.passed, report.failed, report.ignored), (1, 1, 1));
        assert_eq!(
            report.failures,
            [TestFailure {
                name: "adds".to_owned(),
                location: Some((PathBuf::from("/work/test/math.test.js"), 4)),
                message: "Expected values to be strictly equal".to_owned(),
            }]
        );
    }

    /// Output without any summary is incomplete.
    #[test]
    fn parse_truncated_run_is_incomplete() {
        let report = TypeScript.parse_test_output(" RUN  v3.2.0 /work\n", "Killed");
        assert!(report.incomplete);
        assert_eq!((report.passed, report.failed), (0, 0));
    }

    /// Fresh scratch directory for a detection fixture.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agent-ide-lang-typescript-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes `text` to `root/relative`, creating parents.
    fn put(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// Scripts through pnpm, vitest and prettier facts, build-only tsconfig, entry points.
    #[test]
    fn detect_reads_package_scripts_and_environment() {
        let root = scratch("full");
        put(
            &root,
            "package.json",
            r#"{
  "main": "./dist/index.js",
  "bin": { "shapes": "./bin/shapes.js" },
  "exports": { ".": { "import": "./dist/index.mjs" }, "./*": "./dist/*.js" },
  "engines": { "node": ">=20" },
  "scripts": { "build": "tsc -b", "test": "vitest", "lint": "eslint .", "fmt": "prettier -w .", "check-types": "tsc" },
  "devDependencies": { "vitest": "^3.0.0" },
  "prettier": {}
}"#,
        );
        put(&root, "pnpm-lock.yaml", "");
        put(&root, "tsconfig.build.json", "{}");
        put(&root, "src/index.ts", "");
        let project = TypeScript.detect(&root).unwrap();
        fs::remove_dir_all(&root).unwrap();

        assert_eq!(
            project.manifests,
            [
                PathBuf::from("package.json"),
                PathBuf::from("tsconfig.build.json")
            ]
        );
        let facts: Vec<(&str, &str)> = project
            .environment
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        assert_eq!(
            facts,
            [
                ("package_manager", "pnpm"),
                ("node", ">=20"),
                ("tsconfig", "tsconfig.build.json"),
                ("test_runner", "vitest"),
                ("formatter", "prettier"),
            ]
        );
        let commands = &project.commands;
        let run = |command: &Option<ProjectCommand>| command.as_ref().map(|c| c.argv.join(" "));
        assert_eq!(run(&commands.build).as_deref(), Some("pnpm run build"));
        assert_eq!(run(&commands.test).as_deref(), Some("pnpm run test"));
        assert_eq!(run(&commands.lint).as_deref(), Some("pnpm run lint"));
        assert_eq!(run(&commands.format).as_deref(), Some("pnpm run fmt"));
        assert_eq!(
            run(&commands.typecheck).as_deref(),
            Some("pnpm run check-types")
        );
        assert_eq!(
            project.entry_points,
            [
                PathBuf::from("dist/index.js"),
                PathBuf::from("bin/shapes.js"),
                PathBuf::from("dist/index.mjs"),
                PathBuf::from("src/index.ts"),
            ]
        );
        assert_eq!(
            TypeScript.format_command(&project, Path::new("src/a.ts")),
            Some(argv(&["npx", "prettier", "--write", "src/a.ts"]))
        );
    }

    /// Without scripts, typecheck defaults to `tsc --noEmit` and npm is assumed; no manifest → none.
    #[test]
    fn detect_defaults_typecheck_to_tsc() {
        let root = scratch("bare");
        put(&root, "package.json", "{}");
        put(&root, "tsconfig.json", "{}");
        let project = TypeScript.detect(&root).unwrap();
        let typecheck = project.commands.typecheck.as_ref().unwrap();
        assert_eq!(
            typecheck.argv,
            argv(&["npx", "tsc", "--noEmit", "-p", "tsconfig.json"])
        );
        assert_eq!(typecheck.source, CommandSource::Default);
        assert_eq!(env_value(&project, "package_manager"), Some("npm"));
        assert_eq!(env_value(&project, "test_runner"), Some("node"));
        assert_eq!(TypeScript.format_command(&project, Path::new("a.ts")), None);
        fs::remove_dir_all(&root).unwrap();
        let empty = scratch("empty");
        assert_eq!(TypeScript.detect(&empty), None);
        fs::remove_dir_all(&empty).unwrap();
    }

    #[test]
    fn format_stdin_command_covers_the_script_extensions() {
        let prettier = project(&[("formatter", "prettier")]);
        for file in [
            "src/a.ts",
            "src/a.tsx",
            "src/a.js",
            "src/a.jsx",
            "src/a.mts",
            "src/a.cts",
            "src/a.mjs",
            "src/a.cjs",
        ] {
            assert_eq!(
                TypeScript.format_stdin_command(&prettier, Path::new(file)),
                Some(argv(&["npx", "prettier", "--stdin-filepath", file])),
                "{file}"
            );
        }
        assert_eq!(
            TypeScript.format_stdin_command(&prettier, Path::new("src/a.py")),
            None
        );

        let none = project(&[]);
        assert_eq!(
            TypeScript.format_stdin_command(&none, Path::new("src/a.ts")),
            None
        );
    }
}
