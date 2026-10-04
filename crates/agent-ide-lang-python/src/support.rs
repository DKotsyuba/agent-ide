//! Python language support (v0.4): pyright document symbols, pytest, ruff/black and pyright/mypy.
//!
//! Manifests (`pyproject.toml`, `setup.cfg`, CI workflows, `Makefile`) are read by plain line
//! scanning: TOML tables and `key = "value"` entries, YAML `run:` lines and blocks, make target
//! lines. Inline tables, multi-line TOML strings and YAML anchors are ignored rather than parsed,
//! so detection degrades to fewer facts instead of failing.
//!
//! A few text helpers are `pub(super)` because the TypeScript module shares them.

use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
};

use async_lsp::lsp_types as lsp;

use agent_ide_core::lang::render::clip;
use agent_ide_core::lang::text::{
    MAX_ATTRIBUTE_CHARS, MAX_NAMED_TESTS, distinct, distinct_files, entry_names, env_value,
    has_files_with, indent_of, indent_unit, last_content_line, line_at, one_line, read_text,
    source_lines,
};
use agent_ide_core::lang::{
    CommandSource, InsertSite, InsertWhere, LangError, Language, LanguageProject, LanguageSupport,
    LineRange, Outline, ProbePrograms, ProjectCommand, ProjectCommands, Symbol, SymbolKind,
    SymbolPath, TestFailure, TestId, TestReport, TestSelection, TestTarget, kind_of, line_count,
    lines_of,
};

use crate::LANGUAGE;

/// Python support over pyright's hierarchical document symbols; stateless.
#[derive(Clone, Copy, Debug, Default)]
pub struct Python;

/// Flags appended to every pytest run: no header, no `.pytest_cache` writes.
///
/// No own verbosity flag: the project's `addopts` may already carry `-q`, and a doubled `-qq`
/// makes pytest print no summary line at all, which the parser then cannot read.
const PYTEST_FLAGS: [&str; 3] = ["--no-header", "-p", "no:cacheprovider"];

/// Root marker files (besides any root `requirements*.txt` and a `.venv`/`venv` directory, both
/// matched separately) whose presence identifies a worktree as a Python project.
///
/// This is the ONE marker list shared by the project card ([`Python::detect`]) and project
/// checks ([`crate::checks::PythonChecks::is_present`]); the two must never diverge again — a
/// root `requirements-dev.txt` used to register the card while checks stayed silent.
const ROOT_MARKER_FILES: [&str; 5] = [
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "Pipfile",
    "pyrightconfig.json",
];

/// Vendor and build-output directories the nested-root probe never enters: listing one is pure
/// cost (a JS monorepo's `node_modules` holds thousands of entries), and none of them is a
/// Python subproject of this worktree.
const SKIPPED_PROBE_DIRECTORIES: [&str; 5] = ["node_modules", "target", "dist", "build", "vendor"];

/// Directory levels below the worktree root the nested-root probe descends to: a manifest two
/// levels down (`packages/alpha/pyproject.toml`) is the deepest monorepo shape discovered.
const PROBE_MAX_DEPTH: usize = 2;

/// Upper bound on directories the nested-root probe enters, so a pathological tree cannot turn
/// the cheap presence rule into a walk.
const PROBE_MAX_DIRECTORIES: usize = 32;

/// Cap on discovered Python roots; a monorepo beyond this many Python packages keeps the first
/// `PROBE_MAX_ROOTS` in directory order and the card names what it found.
pub(crate) const PROBE_MAX_ROOTS: usize = 8;

/// Reports whether `name` is a `requirements*.txt` marker file.
fn is_requirements_txt(name: &str) -> bool {
    name.starts_with("requirements") && name.ends_with(".txt")
}

/// Names of the entries directly under `dir` in directory order — the probe decides by
/// membership alone, so it never pays a sort; empty when it cannot be listed.
fn dir_names(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect()
}

/// Reports whether `dir` holds a Python manifest file: any [`ROOT_MARKER_FILES`] entry or a
/// `requirements*.txt`.
fn has_manifest(names: &[String]) -> bool {
    names
        .iter()
        .any(|name| ROOT_MARKER_FILES.contains(&name.as_str()))
        || names.iter().any(|name| is_requirements_txt(name))
}

/// The Python roots of `worktree`: the worktree itself when it declares a root marker, plus every
/// subdirectory — down to [`PROBE_MAX_DEPTH`] levels, skipping dot, vendor and
/// build-output directories, at most [`PROBE_MAX_DIRECTORIES`] of them — that holds its own
/// manifest and at least one `.py` file within a bounded walk beneath it, so a docs-only
/// `docs/requirements.txt` (Sphinx in a Rust or Go repository) is not a root. Sorted, and capped
/// at [`PROBE_MAX_ROOTS`]. Empty when no manifest exists anywhere.
///
/// Shared by the project card, the presence rule and the per-root check runs, so the three can
/// never disagree about which roots a worktree has.
pub(crate) fn python_roots(worktree: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let root_names = dir_names(worktree);
    if has_manifest(&root_names) {
        roots.push(worktree.to_path_buf());
    }
    // Breadth-first over plain subdirectories: `packages/alpha` is found before `a/b/c` ever
    // matters, and the caps bound both the width and the depth of the walk.
    let mut queue: VecDeque<(PathBuf, usize)> = root_names
        .iter()
        .filter(|name| {
            !name.starts_with('.') && !SKIPPED_PROBE_DIRECTORIES.contains(&name.as_str())
        })
        .filter_map(|name| {
            let dir = worktree.join(name);
            dir.is_dir().then_some((dir, 1usize))
        })
        .collect();
    let mut visited = 0usize;
    while let Some((dir, depth)) = queue.pop_front() {
        visited += 1;
        let names = dir_names(&dir);
        if has_manifest(&names) && has_files_with(&dir, &["py", "pyi"]) {
            roots.push(dir.clone());
        }
        if depth < PROBE_MAX_DEPTH && visited < PROBE_MAX_DIRECTORIES {
            queue.extend(
                names
                    .iter()
                    .filter(|name| {
                        !name.starts_with('.')
                            && !SKIPPED_PROBE_DIRECTORIES.contains(&name.as_str())
                    })
                    .filter_map(|name| {
                        let child = dir.join(name);
                        child.is_dir().then_some((child, depth + 1))
                    }),
            );
        }
        if visited >= PROBE_MAX_DIRECTORIES {
            break;
        }
    }
    roots.sort();
    roots.truncate(PROBE_MAX_ROOTS);
    roots
}

/// Reports whether `root` is a Python project, per the shared marker rule.
///
/// Root markers: any of [`ROOT_MARKER_FILES`], any root `requirements*.txt`, or a `.venv`/`venv`
/// directory. Plus every root `python_roots` discovers from nested manifests — so a monorepo
/// whose Python packages live two levels down, with no manifest at the worktree root, is a
/// Python project whose every root the card lists.
pub(crate) fn is_python_project(root: &Path) -> bool {
    if ROOT_MARKER_FILES
        .iter()
        .any(|name| root.join(name).is_file())
        || dir_names(root).iter().any(|name| is_requirements_txt(name))
        || [".venv", "venv"].iter().any(|dir| root.join(dir).is_dir())
    {
        return true;
    }
    !python_roots(root).is_empty()
}

/// The virtual-environment directories beside `root`, most specific first: the conventional
/// `.venv` and `venv`, then any sibling whose name extends them (`.venv-py314`, `venv310`), in
/// directory order. Each must hold `bin/python` to count.
pub(crate) fn venv_directories(root: &Path) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = [".venv", "venv"]
        .iter()
        .map(|name| root.join(name))
        .collect();
    candidates.extend(
        dir_names(root)
            .into_iter()
            .filter(|name| {
                (name.starts_with(".venv") && name != ".venv")
                    || (name.starts_with("venv") && name != "venv")
            })
            .filter(|name| root.join(name).join("bin").join("python").is_file())
            .map(|name| root.join(name)),
    );
    candidates
        .into_iter()
        .filter(|dir| dir.join("bin").join("python").is_file())
        .collect()
}

/// `-c` program of the syntax probe: parses stdin with `ast` and prints `<lineno>: <msg>` on a
/// syntax error, the exact line `SyntaxVerdict::from_probe` maps (any other nonzero output
/// means no checker was proven).
const PY_AST_PROBE: &str = "import ast,sys\ntry:\n    ast.parse(sys.stdin.read())\nexcept SyntaxError as e:\n    print(f\"{e.lineno}: {e.msg}\")\n    sys.exit(1)";

impl LanguageSupport for Python {
    /// Always the Python [`LANGUAGE`].
    fn language(&self) -> Language {
        LANGUAGE
    }

    /// Establishes a Python project per the shared marker rule (`is_python_project`); `None`
    /// when neither a marker exists nor any `.py`/`.pyi` file lies under `root` (a worktree with
    /// Python files but no manifest still gets a project, with no manifests, no commands and a
    /// `project` environment fact saying so, so the card can name it). Nested roots
    /// (`python_roots`) are all listed: each nested root contributes its manifest paths
    /// (relative to `root`), a `root` environment fact, and — when it holds a
    /// virtual-environment directory — a `venv` fact whose `bin/python` becomes the absolute
    /// `interpreter` of the first root that has one. Other environment facts recorded (in this
    /// order, each only when present at the worktree root): `tool` (`uv`, `poetry`), `python`
    /// (`.python-version`), `configured` (`pyright`, `mypy`, `ruff`) and `formatter` (`black` or
    /// `ruff`, which `format_command` reads back). Commands start from the manifests and are
    /// overridden by Makefile targets and then by CI workflow `run:` lines, so CI wins.
    fn detect(&self, root: &Path) -> Option<LanguageProject> {
        let roots = python_roots(root);
        if roots.is_empty() && !is_python_project(root) {
            // Files without any manifest: the card still lists the language and says no project
            // was found, instead of reporting nothing at all.
            return has_files_with(root, &["py", "pyi"]).then(|| LanguageProject {
                language: LANGUAGE,
                manifests: Vec::new(),
                environment: vec![(
                    "project".to_owned(),
                    "files present, no manifest found — checks unavailable".to_owned(),
                )],
                interpreter: None,
                commands: ProjectCommands::default(),
                entry_points: Vec::new(),
            });
        }
        let names = entry_names(root);
        let has = |name: &str| root.join(name).is_file();
        let mut manifests: Vec<PathBuf> = ["pyproject.toml", "setup.py", "setup.cfg"]
            .into_iter()
            .filter(|name| has(name))
            .map(PathBuf::from)
            .collect();
        manifests.extend(
            names
                .iter()
                .filter(|name| name.starts_with("requirements") && name.ends_with(".txt"))
                .map(PathBuf::from),
        );
        // Every nested root contributes its own manifests, addressed relative to `root`, and a
        // `root` fact so the card lists it.
        let relative_manifests: Vec<PathBuf> = roots
            .iter()
            .filter(|project_root| project_root != &root)
            .flat_map(|project_root| {
                let project_names = entry_names(project_root);
                let mut nested: Vec<PathBuf> = ["pyproject.toml", "setup.py", "setup.cfg"]
                    .into_iter()
                    .filter(|name| project_names.iter().any(|entry| entry == name))
                    .map(|name| project_root.join(name))
                    .collect();
                nested.extend(
                    project_names
                        .iter()
                        .filter(|name| is_requirements_txt(name))
                        .map(|name| project_root.join(name)),
                );
                nested
            })
            .map(|path| {
                path.strip_prefix(root)
                    .map(Path::to_path_buf)
                    .unwrap_or(path)
            })
            .collect();
        manifests.extend(relative_manifests);
        let pyproject = read_text(root, "pyproject.toml");
        let setup_cfg = read_text(root, "setup.cfg");

        let mut environment = Vec::new();
        let mut fact =
            |name: &str, value: &str| environment.push((name.to_owned(), value.to_owned()));
        let mut interpreter = None;
        for project_root in &roots {
            let relative = project_root
                .strip_prefix(root)
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default();
            if project_root != root {
                fact("root", &relative);
            }
            if interpreter.is_none()
                && let Some(venv) = venv_directories(project_root).into_iter().next()
            {
                let python = venv.join("bin").join("python");
                // Not canonicalized: the venv's `python` is a symlink whose target loses the venv.
                interpreter = Some(std::path::absolute(&python).unwrap_or(python));
                let venv_name = venv
                    .strip_prefix(project_root)
                    .ok()
                    .and_then(|path| path.to_str())
                    .unwrap_or(".venv");
                if relative.is_empty() {
                    fact("venv", venv_name);
                } else {
                    fact("venv", &format!("{relative}/{venv_name}"));
                }
            }
        }
        if interpreter.is_none()
            && let Some(venv) = venv_directories(root).into_iter().next()
        {
            // A worktree environment also covers nested roots without their own environment.
            let python = venv.join("bin").join("python");
            interpreter = Some(std::path::absolute(&python).unwrap_or(python));
            fact(
                "venv",
                venv.strip_prefix(root)
                    .ok()
                    .and_then(|path| path.to_str())
                    .unwrap_or(".venv"),
            );
        }
        let uv = has("uv.lock");
        if uv {
            fact("tool", "uv");
        }
        if has("poetry.lock") {
            fact("tool", "poetry");
        }
        if let Some(version) = read_text(root, ".python-version")
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
        {
            fact("python", version);
        }
        let pyright = toml_has_table(&pyproject, "tool.pyright") || has("pyrightconfig.json");
        let mypy = toml_has_table(&pyproject, "tool.mypy")
            || has("mypy.ini")
            || has(".mypy.ini")
            || toml_has_table(&setup_cfg, "mypy");
        let ruff = toml_has_table(&pyproject, "tool.ruff") || has("ruff.toml") || has(".ruff.toml");
        for (configured, name) in [(pyright, "pyright"), (mypy, "mypy"), (ruff, "ruff")] {
            if configured {
                fact("configured", name);
            }
        }
        let formatter = if toml_has_table(&pyproject, "tool.black") {
            Some("black")
        } else if ruff {
            Some("ruff")
        } else {
            None
        };
        if let Some(formatter) = formatter {
            fact("formatter", formatter);
        }

        let manifest = |args: &[&str]| ProjectCommand {
            argv: with_uv(uv, args),
            source: CommandSource::Manifest,
        };
        let mut commands = ProjectCommands {
            test: Some(manifest(&["pytest"])),
            lint: ruff.then(|| manifest(&["ruff", "check", "."])),
            format: formatter.map(|formatter| match formatter {
                "black" => manifest(&["black", "."]),
                _ => manifest(&["ruff", "format", "."]),
            }),
            typecheck: if pyright {
                Some(manifest(&["pyright"]))
            } else if mypy {
                Some(manifest(&["mypy", "."]))
            } else {
                None
            },
            ..ProjectCommands::default()
        };
        apply_makefile(root, &mut commands);
        apply_ci(root, &mut commands);

        Some(LanguageProject {
            language: LANGUAGE,
            manifests,
            environment,
            interpreter,
            commands,
            entry_points: entry_points(root, &names, &pyproject),
        })
    }

    /// Header = decorators directly above `def`/`class` (multi-line decorator arguments
    /// included); the docstring stays inside the body and its first paragraph becomes `doc`.
    /// Names a function binds (parameters, locals, `except as` bindings, comprehension
    /// variables) are dropped; class attributes and enum members keep only their target and
    /// annotation (`name: Type`) or their clipped `NAME = value`. Functions in a class become
    /// methods (`__init__` a constructor), functions nested in functions stay functions, and in
    /// test files module- or class-level `test_*` functions and `Test*` classes become tests.
    /// Same-named siblings are kept as reported (pyright reports redefinitions separately);
    /// [`Outline::find`] then resolves the first.
    fn normalize(&self, file: &Path, source: &str, symbols: Vec<lsp::DocumentSymbol>) -> Outline {
        let lines = source_lines(source);
        let root = SymbolPath::new(Some(file.to_path_buf()), Vec::new());
        Outline {
            file: file.to_path_buf(),
            language: LANGUAGE,
            line_count: line_count(source),
            symbols: convert_all(
                &lines,
                symbols,
                &root,
                Owner::Module,
                self.is_test_file(file),
            ),
        }
    }

    /// `Before`/`After` place code next to the anchor with PEP 8 spacing: two blank lines at
    /// module level, one inside classes and functions. `First`/`Last` require a class anchor:
    /// `First` lands after the class line (and its docstring), `Last` after the class's last
    /// non-blank line, both with the members' indentation (the file's unit when the class has no
    /// members). A class whose body shares the `class` line is [`LangError::Unparseable`].
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
        let blanks = if anchor.segments().len() <= 1 { 2 } else { 1 };
        let indent = indent_of(line_at(&lines, symbol.range.start)).to_owned();
        let end = last_content_line(&lines, symbol.range);
        match where_ {
            InsertWhere::Before => Ok(InsertSite {
                line: symbol.range.start,
                indent,
                blank_before: blanks,
                blank_after: blanks,
            }),
            InsertWhere::After => Ok(InsertSite {
                line: end + 1,
                indent,
                blank_before: blanks,
                blank_after: blanks,
            }),
            InsertWhere::First | InsertWhere::Last => {
                let decl = find_decl(&lines, symbol.range, &symbol.name)
                    .filter(|&index| lines[index].trim_start().starts_with("class "))
                    .ok_or_else(|| LangError::NotAContainer(anchor.clone()))?;
                let (_, header) = signature_at(&lines, decl);
                if !lines[header.line]
                    .get(header.col + 1..)
                    .unwrap_or("")
                    .trim()
                    .is_empty()
                {
                    return Err(LangError::Unparseable(format!(
                        "class body of {anchor} shares the class line"
                    )));
                }
                let docstring = docstring(&lines, &header);
                let preamble_end = docstring.as_ref().map_or(header.line, |doc| doc.end);
                let has_members =
                    (preamble_end + 1..end as usize).any(|index| !lines[index].trim().is_empty());
                let member_indent = symbol
                    .children
                    .first()
                    .map(|child| indent_of(line_at(&lines, child.range.start)).to_owned())
                    .or_else(|| {
                        lines
                            .get(header.line + 1..end as usize)
                            .unwrap_or_default()
                            .iter()
                            .find(|line| !line.trim().is_empty())
                            .map(|line| indent_of(line).to_owned())
                    })
                    .unwrap_or_else(|| {
                        format!("{}{}", indent_of(lines[decl]), indent_unit(&lines, ':', 4))
                    });
                Ok(if where_ == InsertWhere::First {
                    InsertSite {
                        line: preamble_end as u32 + 2,
                        indent: member_indent,
                        blank_before: u8::from(docstring.is_some()),
                        blank_after: u8::from(has_members),
                    }
                } else {
                    InsertSite {
                        line: end + 1,
                        indent: member_indent,
                        blank_before: 1,
                        blank_after: 0,
                    }
                })
            }
        }
    }
    /// `test_*.py`, `*_test.py`, `conftest.py` and any `.py` file under a `tests` or `test` directory.
    fn is_test_file(&self, file: &Path) -> bool {
        let Some(name) = file.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        name.ends_with(".py")
            && (name.starts_with("test_")
                || name.ends_with("_test.py")
                || name == "conftest.py"
                || file.components().any(|part| {
                    let part = part.as_os_str();
                    part == "tests" || part == "test"
                }))
    }
    /// pytest with `--no-header -p no:cacheprovider`, prefixed by `uv run` in uv projects.
    /// A symbol target names each distinct referencing test as a node id (`file::Class::test`;
    /// `/` in a name is read as nesting) or, beyond `MAX_NAMED_TESTS`, their distinct files; a
    /// symbol no test references is [`LangError::Unsupported`]. A file target runs whole only
    /// when the file is one pytest would collect by convention (`test_*.py`, `*_test.py`,
    /// `conftest.py`, under `tests/` or `test/`); a directory target selects the test files
    /// inside it; a non-test `.py` file is [`LangError::Unsupported`]. Patterns go to `-k`.
    fn test_selection(
        &self,
        project: &LanguageProject,
        target: &TestTarget,
    ) -> Result<TestSelection, LangError> {
        let uv = project
            .environment
            .iter()
            .any(|(name, value)| name == "tool" && value == "uv");
        let (tests, args) = match target {
            TestTarget::Symbol {
                path,
                referencing_tests,
            } => {
                let tests = distinct(referencing_tests);
                if tests.is_empty() {
                    return Err(LangError::Unsupported(format!("no test references {path}")));
                }
                let args = if tests.len() <= MAX_NAMED_TESTS {
                    tests.iter().map(node_id).collect()
                } else {
                    distinct_files(&tests)
                };
                (tests, args)
            }
            TestTarget::File(file) => {
                // A directory (`tests/`) selects the test files inside it and passes through
                // unchanged; a plain `.py` file must be one pytest would collect by convention,
                // or the module would be imported top-level as a test target.
                if file.extension().is_some_and(|extension| extension == "py")
                    && !self.is_test_file(file)
                {
                    return Err(LangError::Unsupported(format!(
                        "no tests in {}",
                        file.display()
                    )));
                }
                (Vec::new(), vec![file.display().to_string()])
            }
            TestTarget::Pattern(pattern) => (Vec::new(), vec!["-k".to_owned(), pattern.clone()]),
        };
        let mut command = with_uv(uv, &["pytest"]);
        command.extend(args);
        command.extend(PYTEST_FLAGS.map(String::from));
        Ok(TestSelection { tests, command })
    }

    /// Reads the last `N passed, M failed, K skipped in 0.1s` summary (errors count as failed,
    /// xfailed as ignored, xpassed as passed), `FAILED`/`ERROR` node-id lines, and the
    /// `____ name ____` failure blocks for the `file.py:line:` location and the first `E` line.
    /// Without a summary line the report is incomplete.
    fn parse_test_output(&self, stdout: &str, stderr: &str) -> TestReport {
        let text = format!("{stdout}\n{stderr}");
        let lines: Vec<&str> = text.lines().collect();
        let mut report = TestReport {
            incomplete: true,
            ..TestReport::default()
        };
        if let Some((passed, failed, ignored)) =
            lines.iter().rev().find_map(|line| pytest_summary(line))
        {
            (report.passed, report.failed, report.ignored) = (passed, failed, ignored);
            report.incomplete = false;
        }
        let blocks = failure_blocks(&lines);
        for line in &lines {
            let Some(rest) = line
                .strip_prefix("FAILED ")
                .or_else(|| line.strip_prefix("ERROR "))
            else {
                continue;
            };
            let (id, message) = rest.split_once(" - ").unwrap_or((rest, ""));
            let id = id.trim();
            let block_name = id
                .split_once("::")
                .map_or(id, |(_, name)| name)
                .replace("::", ".");
            let block = blocks.iter().find(|block| block.name == block_name);
            report.failures.push(TestFailure {
                name: id.to_owned(),
                location: block.and_then(|block| block.location.clone()),
                message: if message.is_empty() {
                    block
                        .and_then(|block| block.message.clone())
                        .unwrap_or_default()
                } else {
                    message.trim().to_owned()
                },
            });
        }
        if report.failures.is_empty() {
            report.failures = blocks
                .into_iter()
                .map(|block| TestFailure {
                    name: block.name,
                    location: block.location,
                    message: block.message.unwrap_or_default(),
                })
                .collect();
        }
        report
    }

    /// `black <file>` or `ruff format <file>` as detected (`uv run` prefixed in uv projects);
    /// `None` when the project configures neither.
    fn format_command(&self, project: &LanguageProject, file: &Path) -> Option<Vec<String>> {
        let uv = project
            .environment
            .iter()
            .any(|(name, value)| name == "tool" && value == "uv");
        let file = file.display().to_string();
        match env_value(project, "formatter")? {
            "black" => Some(with_uv(uv, &["black", &file])),
            "ruff" => Some(with_uv(uv, &["ruff", "format", &file])),
            _ => None,
        }
    }

    /// `ruff format --stdin-filename <file> -` or `black -q -` as detected (`uv run` prefixed in
    /// uv projects), reading the candidate text on stdin and writing the formatted text to
    /// stdout. `None` for a non-`.py`/`.pyi` file or a project without either formatter.
    fn format_stdin_command(&self, project: &LanguageProject, file: &Path) -> Option<Vec<String>> {
        match file.extension().and_then(|ext| ext.to_str()) {
            Some("py" | "pyi") => {}
            _ => return None,
        }
        let uv = project
            .environment
            .iter()
            .any(|(name, value)| name == "tool" && value == "uv");
        let file = file.display().to_string();
        match env_value(project, "formatter")? {
            "black" => Some(with_uv(uv, &["black", "-q", "-"])),
            "ruff" => Some(with_uv(
                uv,
                &["ruff", "format", "--stdin-filename", &file, "-"],
            )),
            _ => None,
        }
    }

    /// Tests live only in test files by this runner's naming convention.
    fn tests_only_in_test_files(&self) -> bool {
        true
    }

    /// The project's own interpreter (the detected venv, else `python3` on PATH like the
    /// formatter's tools) runs `ast.parse` over the candidate on stdin and prints
    /// `<lineno>: <msg>` on a syntax error — exactly the probe line
    /// `SyntaxVerdict::from_probe` maps. A missing interpreter fails the spawn, which the
    /// caller maps to `Unchecked`, never a refusal. Python has no launcher-configured probe
    /// programs, so `configured` is ignored.
    fn syntax_probe_command(
        &self,
        project: &LanguageProject,
        _root: &Path,
        file: &Path,
        _configured: Option<&ProbePrograms>,
    ) -> Option<Vec<String>> {
        match file.extension().and_then(|ext| ext.to_str()) {
            Some("py" | "pyi") => {}
            _ => return None,
        }
        let interpreter = project
            .interpreter
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("python3"))
            .display()
            .to_string();
        Some(vec![interpreter, "-c".to_owned(), PY_AST_PROBE.to_owned()])
    }

    /// The module docstring's first line (or the line after an empty opening quote).
    fn file_doc(&self, text: &str) -> Option<String> {
        let lines: Vec<_> = text.lines().collect();

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
}

/// What encloses a symbol being normalized; decides the method/constructor/test refinement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Owner {
    /// Top level of the file.
    Module,
    /// A class body: functions become methods.
    Class,
    /// A function body (or any non-class symbol): nested functions stay functions.
    Function,
}

/// Kinds pyright reports for names a function binds: parameters, locals, `except as` bindings,
/// comprehension variables and type parameters. None of them belong in an outline.
fn is_local(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Variable | SymbolKind::Constant | SymbolKind::Field | SymbolKind::TypeAlias
    )
}

/// Kinds pyright reports for class-level attributes and enum members.
fn is_attribute(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Variable | SymbolKind::Constant | SymbolKind::Field | SymbolKind::Variant
    )
}

/// Converts sibling document symbols under `owner_path`, ordered by their first line. Children
/// of functions only keep structure: every bound name is dropped (see [`is_local`]).
fn convert_all(
    lines: &[&str],
    symbols: Vec<lsp::DocumentSymbol>,
    owner_path: &SymbolPath,
    owner: Owner,
    test_file: bool,
) -> Vec<Symbol> {
    let mut converted: Vec<Symbol> = symbols
        .into_iter()
        .filter(|symbol| owner != Owner::Function || !is_local(kind_of(symbol.kind)))
        .map(|symbol| convert(lines, symbol, owner_path, owner, test_file))
        .collect();
    converted.sort_by_key(|symbol| (symbol.range.start, symbol.body.start));
    converted
}

/// Normalizes one pyright symbol and its children (see [`Python::normalize`] for the rules).
fn convert(
    lines: &[&str],
    symbol: lsp::DocumentSymbol,
    owner_path: &SymbolPath,
    owner: Owner,
    test_file: bool,
) -> Symbol {
    let body = lines_of(&symbol.range);
    let decl = find_decl(lines, body, &symbol.name);
    let is_class = decl.is_some_and(|index| lines[index].trim_start().starts_with("class "));
    let mut kind = match kind_of(symbol.kind) {
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Constructor => match owner {
            Owner::Class if symbol.name == "__init__" => SymbolKind::Constructor,
            Owner::Class => SymbolKind::Method,
            Owner::Module | Owner::Function => SymbolKind::Function,
        },
        other => other,
    };
    let collected = owner != Owner::Function
        && ((matches!(kind, SymbolKind::Function | SymbolKind::Method)
            && symbol.name.starts_with("test_"))
            || (kind == SymbolKind::Class && symbol.name.starts_with("Test")));
    if test_file && collected {
        kind = SymbolKind::Test;
    }
    let attribute = owner == Owner::Class && is_attribute(kind);
    let (signature, doc, header) = match decl {
        Some(index) => {
            let (signature, end) = signature_at(lines, index);
            let doc = docstring(lines, &end).and_then(|doc| doc.text);
            (signature, doc, decorator_start(lines, index))
        }
        None => {
            let line = line_at(lines, body.start);
            let signature = if attribute {
                attribute_signature(line)
            } else {
                one_line(line)
            };
            (signature, None, body.start as usize - 1)
        }
    };
    let child_owner = if is_class || kind == SymbolKind::Class {
        Owner::Class
    } else {
        Owner::Function
    };
    let path = owner_path.child(&symbol.name);
    let children = convert_all(
        lines,
        symbol.children.unwrap_or_default(),
        &path,
        child_owner,
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

/// Index of the `def`/`async def`/`class` line declaring `name` inside `range`, if any.
fn find_decl(lines: &[&str], range: LineRange, name: &str) -> Option<usize> {
    let last = (range.end as usize).min(lines.len());
    (range.start as usize - 1..last).find(|&index| {
        let text = lines[index].trim_start();
        let text = text.strip_prefix("async ").map_or(text, str::trim_start);
        text.strip_prefix("def ")
            .or_else(|| text.strip_prefix("class "))
            .is_some_and(|rest| {
                rest.trim_start().strip_prefix(name).is_some_and(|after| {
                    !after.starts_with(|c: char| c.is_alphanumeric() || c == '_')
                })
            })
    })
}

/// Index of the first decorator line directly above the declaration at `decl` (`decl` itself
/// when there is none). Lines deeper than the declaration or closing brackets at its indentation
/// are claimed only when an `@` line above them completes a multi-line decorator; a blank line
/// ends the header.
fn decorator_start(lines: &[&str], decl: usize) -> usize {
    let indent = indent_of(lines[decl]).len();
    let mut start = decl;
    let mut index = decl;
    while index > 0 {
        index -= 1;
        let text = lines[index];
        let trimmed = text.trim();
        let width = indent_of(text).len();
        if trimmed.is_empty() {
            break;
        }
        if width == indent && trimmed.starts_with('@') {
            start = index;
        } else if !(width > indent || (width == indent && trimmed.starts_with([')', ']', '}']))) {
            break;
        }
    }
    start
}

/// Where a `def`/`class` header ends: the line index and byte column of its depth-0 `:`.
struct HeaderEnd {
    /// 0-based line index.
    line: usize,
    /// Byte column of the `:` in that line.
    col: usize,
}

/// Collapses the header starting at line `start` into one line up to (excluding) its depth-0
/// `:`, skipping string contents and comments. An unterminated header ends at the last line
/// scanned (at most 64 lines).
fn signature_at(lines: &[&str], start: usize) -> (String, HeaderEnd) {
    let mut depth = 0i32;
    let mut text = String::new();
    let last = (start + 64).min(lines.len());
    for (index, line) in lines.iter().enumerate().take(last).skip(start) {
        let mut quote: Option<char> = None;
        let mut escaped = false;
        let mut cut = line.len();
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
                '\'' | '"' => quote = Some(ch),
                '#' => {
                    cut = col;
                    break;
                }
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                ':' if depth <= 0 => {
                    text.push_str(&line[..col]);
                    return (one_line(&text), HeaderEnd { line: index, col });
                }
                _ => {}
            }
        }
        text.push_str(&line[..cut]);
        text.push(' ');
    }
    let line = last.saturating_sub(1);
    (
        one_line(&text),
        HeaderEnd {
            line,
            col: lines[line].len().saturating_sub(1),
        },
    )
}

/// Signature of a class attribute or enum member: `name: annotation` for an annotated
/// assignment, otherwise the whole `name = value`, comment stripped and clipped at
/// [`MAX_ATTRIBUTE_CHARS`] characters. Brackets, strings and `==`-style operators are skipped
/// while looking for the value's `=`.
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
            '\'' | '"' => quote = Some(ch),
            '#' => {
                cut = col;
                break;
            }
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ':' if depth <= 0 && prev != '=' => annotated = true,
            '=' if depth <= 0
                && !matches!(prev, '=' | '!' | '<' | '>' | ':')
                && !line[col + 1..].starts_with('=')
                && annotated =>
            {
                cut = col;
                break;
            }
            _ => {}
        }
        prev = ch;
    }
    clip(&one_line(line[..cut].trim_end()), MAX_ATTRIBUTE_CHARS)
}

/// A docstring's line span and its first paragraph (`None` when the docstring is blank).
struct Docstring {
    /// 0-based index of the line holding the closing quotes.
    end: usize,
    /// First paragraph, `None` for a blank docstring.
    text: Option<String>,
}

/// The triple-quoted string that is the first statement after `header` (on the header line
/// itself or the next non-blank, non-comment line); `None` when the body starts otherwise or the
/// string never closes.
fn docstring(lines: &[&str], header: &HeaderEnd) -> Option<Docstring> {
    let rest = lines[header.line]
        .get(header.col + 1..)
        .unwrap_or("")
        .trim();
    let (first, mut index) = if rest.is_empty() || rest.starts_with('#') {
        let index = (header.line + 1..lines.len()).find(|&index| {
            let text = lines[index].trim();
            !text.is_empty() && !text.starts_with('#')
        })?;
        (lines[index].trim(), index)
    } else {
        (rest, header.line)
    };
    let opened = first.trim_start_matches(['r', 'R', 'u', 'U']);
    let delimiter = ["\"\"\"", "'''"]
        .into_iter()
        .find(|delimiter| opened.starts_with(delimiter))?;
    let mut content = String::new();
    let mut remainder = &opened[3..];
    loop {
        if let Some(close) = remainder.find(delimiter) {
            content.push_str(&remainder[..close]);
            break;
        }
        content.push_str(remainder);
        content.push('\n');
        index += 1;
        remainder = lines.get(index)?;
    }
    Some(Docstring {
        end: index,
        text: first_paragraph(&content),
    })
}

/// First paragraph of free text: leading blank lines skipped, lines trimmed and joined with one
/// space up to the first blank line; `None` when there is no text.
fn first_paragraph(content: &str) -> Option<String> {
    let words: Vec<&str> = content
        .lines()
        .map(str::trim)
        .skip_while(|line| line.is_empty())
        .take_while(|line| !line.is_empty())
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

/// Whether `text` opens the TOML table `name` or one of its subtables (`[tool.ruff]`,
/// `[tool.ruff.lint]`); also matches INI sections such as `[mypy]`.
fn toml_has_table(text: &str, name: &str) -> bool {
    text.lines().any(|line| {
        line.trim().strip_prefix('[').is_some_and(|rest| {
            rest.strip_prefix(name)
                .is_some_and(|after| after.starts_with(']') || after.starts_with('.'))
        })
    })
}

/// `key = "value"` entries of exactly the TOML table `name`, quotes stripped from both sides.
fn toml_entries(text: &str, name: &str) -> Vec<(String, String)> {
    let header = format!("[{name}]");
    let mut inside = false;
    let mut entries = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            inside = line == header;
        } else if inside && let Some((key, value)) = line.split_once('=') {
            let unquote = |text: &str| text.trim().trim_matches(['"', '\'']).to_owned();
            entries.push((unquote(key), unquote(value)));
        }
    }
    entries
}

/// Entry points relative to `root`: `<pkg>/__main__.py` directly under the root or `src/`,
/// `main.py`, `app.py`, and the modules `[project.scripts]`/`[tool.poetry.scripts]` targets
/// name when their file exists (`pkg.cli:main` → `pkg/cli.py`, `pkg/cli/__init__.py`, or the
/// same under `src/`).
fn entry_points(root: &Path, names: &[String], pyproject: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for (base, names) in [
        (PathBuf::new(), names.to_vec()),
        (PathBuf::from("src"), entry_names(&root.join("src"))),
    ] {
        for name in names {
            let main = base.join(name).join("__main__.py");
            if root.join(&main).is_file() {
                found.push(main);
            }
        }
    }
    for name in ["main.py", "app.py"] {
        if root.join(name).is_file() {
            found.push(PathBuf::from(name));
        }
    }
    for table in ["project.scripts", "tool.poetry.scripts"] {
        for (_, target) in toml_entries(pyproject, table) {
            let module = target
                .split(':')
                .next()
                .unwrap_or_default()
                .replace('.', "/");
            let candidates = [
                format!("{module}.py"),
                format!("{module}/__init__.py"),
                format!("src/{module}.py"),
                format!("src/{module}/__init__.py"),
            ];
            if let Some(file) = candidates.iter().find(|file| root.join(file).is_file()) {
                found.push(PathBuf::from(file));
            }
        }
    }
    let mut unique = Vec::new();
    for path in found {
        if !unique.contains(&path) {
            unique.push(path);
        }
    }
    unique
}

/// Replaces commands with `make <target>` for the first matching Makefile target per slot:
/// `test`/`tests`, `lint`, `format`/`fmt`, `typecheck`/`type-check`/`types`/`mypy`/`pyright`.
fn apply_makefile(root: &Path, commands: &mut ProjectCommands) {
    for line in read_text(root, "Makefile").lines() {
        if !line.starts_with(|c: char| c.is_ascii_alphanumeric()) || line.contains(":=") {
            continue;
        }
        let Some((targets, _)) = line.split_once(':') else {
            continue;
        };
        for target in targets.split_whitespace() {
            let slot = match target {
                "test" | "tests" => &mut commands.test,
                "lint" => &mut commands.lint,
                "format" | "fmt" => &mut commands.format,
                "typecheck" | "type-check" | "types" | "mypy" | "pyright" => {
                    &mut commands.typecheck
                }
                _ => continue,
            };
            if slot
                .as_ref()
                .is_none_or(|command| command.source != CommandSource::Makefile)
            {
                *slot = Some(ProjectCommand {
                    argv: vec!["make".to_owned(), target.to_owned()],
                    source: CommandSource::Makefile,
                });
            }
        }
    }
}

/// Replaces commands with the first CI `run:` segment (split at `&&` and `;`) mentioning
/// `pytest` (test), `ruff check` (lint) or `pyright`/`mypy` (typecheck); install lines are
/// skipped. The segment is split on whitespace, so shell quoting is not honoured.
fn apply_ci(root: &Path, commands: &mut ProjectCommands) {
    for command in ci_run_lines(root) {
        for segment in command.split("&&").flat_map(|part| part.split(';')) {
            let segment = segment.trim();
            if segment.contains("install") {
                continue;
            }
            let slot = if segment.contains("pytest") {
                &mut commands.test
            } else if segment.contains("ruff check") {
                &mut commands.lint
            } else if segment.contains("pyright") || segment.contains("mypy") {
                &mut commands.typecheck
            } else {
                continue;
            };
            if slot
                .as_ref()
                .is_none_or(|command| command.source != CommandSource::Ci)
            {
                *slot = Some(ProjectCommand {
                    argv: segment.split_whitespace().map(String::from).collect(),
                    source: CommandSource::Ci,
                });
            }
        }
    }
}

/// Every shell line of `run:` steps in `.github/workflows/*.yml|*.yaml`, files in name order:
/// the inline value, or each non-blank, non-comment line of a `|`/`>` block.
pub(super) fn ci_run_lines(root: &Path) -> Vec<String> {
    let dir = root.join(".github").join("workflows");
    let mut runs = Vec::new();
    for name in entry_names(&dir) {
        if !(name.ends_with(".yml") || name.ends_with(".yaml")) {
            continue;
        }
        let text = fs::read_to_string(dir.join(&name)).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        let mut index = 0;
        while index < lines.len() {
            let line = lines[index];
            index += 1;
            let trimmed = line.trim_start();
            let (key_indent, item) = match trimmed.strip_prefix("- ") {
                Some(item) => (indent_of(line).len() + 2, item.trim_start()),
                None => (indent_of(line).len(), trimmed),
            };
            let Some(value) = item.strip_prefix("run:") else {
                continue;
            };
            let value = value.trim();
            if value.starts_with('|') || value.starts_with('>') {
                while index < lines.len()
                    && (lines[index].trim().is_empty()
                        || indent_of(lines[index]).len() > key_indent)
                {
                    let text = lines[index].trim();
                    if !text.is_empty() && !text.starts_with('#') {
                        runs.push(text.to_owned());
                    }
                    index += 1;
                }
            } else if !value.is_empty() {
                runs.push(value.trim_matches(['"', '\'']).to_owned());
            }
        }
    }
    runs
}

/// `args` as argv, behind `uv run` when the project is managed by uv.
fn with_uv(uv: bool, args: &[&str]) -> Vec<String> {
    let prefix: &[&str] = if uv { &["uv", "run"] } else { &[] };
    prefix
        .iter()
        .chain(args)
        .map(|arg| (*arg).to_owned())
        .collect()
}

/// pytest node id for a test: the name as is when already qualified with its file, otherwise
/// `file::name` with `/` nesting turned into `::`.
fn node_id(test: &TestId) -> String {
    let file = test.file.display().to_string();
    if test.name.starts_with(&format!("{file}::")) {
        test.name.clone()
    } else {
        format!("{file}::{}", test.name.replace('/', "::"))
    }
}

/// Parses a pytest summary line (`==== 1 failed, 2 passed in 0.12s ====` or the `-q` form)
/// into `(passed, failed, ignored)`; `no tests ran in …` is all zeros. Unknown words such as
/// `warnings` or `deselected` are ignored; any unparsable item rejects the line.
fn pytest_summary(line: &str) -> Option<(u32, u32, u32)> {
    let text = line.trim().trim_matches('=').trim();
    let (counts, time) = text.rsplit_once(" in ")?;
    if !time.trim_start().starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let mut totals = (0, 0, 0);
    if counts.trim() == "no tests ran" {
        return Some(totals);
    }
    for item in counts.split(", ") {
        let (count, word) = item.trim().split_once(' ')?;
        let count: u32 = count.parse().ok()?;
        match word {
            "passed" | "xpassed" => totals.0 += count,
            "failed" | "error" | "errors" => totals.1 += count,
            "skipped" | "xfailed" => totals.2 += count,
            _ => {}
        }
    }
    Some(totals)
}

/// One `____ name ____` section of pytest's failure report.
struct FailureBlock {
    /// Header text: `test_y`, `TestX.test_y`, `test_y[param]`.
    name: String,
    /// Last `file.py:line:` line in the block (the innermost frame).
    location: Option<(PathBuf, u32)>,
    /// First `E` line with the marker stripped.
    message: Option<String>,
}

/// Splits pytest's failure report into blocks; a `=`-rule line ends the current block.
fn failure_blocks(lines: &[&str]) -> Vec<FailureBlock> {
    let mut blocks: Vec<FailureBlock> = Vec::new();
    let mut open = false;
    for line in lines {
        let trimmed = line.trim();
        if trimmed.len() > 6 && trimmed.starts_with("___") && trimmed.ends_with("___") {
            blocks.push(FailureBlock {
                name: trimmed.trim_matches('_').trim().to_owned(),
                location: None,
                message: None,
            });
            open = true;
            continue;
        }
        if trimmed.starts_with("===") {
            open = false;
        }
        let Some(block) = blocks.last_mut().filter(|_| open) else {
            continue;
        };
        if let Some(message) = line.strip_prefix("E ") {
            block
                .message
                .get_or_insert_with(|| message.trim().to_owned());
        } else if !line.starts_with(char::is_whitespace)
            && let Some((file, rest)) = line.split_once(".py:")
            && !file.contains(' ')
            && let Some((number, _)) = rest.split_once(':')
            && let Ok(number) = number.parse()
        {
            block.location = Some((PathBuf::from(format!("{file}.py")), number));
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    //! Fixtures mirror pyright 1.1.4xx: decorated functions start at their first decorator,
    //! methods are children of their class, variables cover only their name.

    use super::*;

    /// A module with a constant, a decorated function and a class with methods.
    const SERVICE: &str = "\
\"\"\"Service module.\"\"\"

import functools

MAX_RETRIES = 3


@functools.lru_cache(
    maxsize=None,
)
@traced
def load(
    path: str,
    retries: int = MAX_RETRIES,
) -> dict[str, int]:
    \"\"\"Load the table.

    More details.
    \"\"\"
    return {}


class Worker(Base):
    \"\"\"A worker.

    Runs jobs.
    \"\"\"

    def __init__(self, name: str) -> None:
        self.name = name

    @cached
    def label(self) -> str:
        def inner():
            return 1
        return self.name
";

    /// A pytest module with a function test, a test class and a helper method.
    const TESTS: &str = "\
import pytest


def test_load():
    assert load(\"x\") == {}


class TestWorker:
    def test_label(self):
        assert True

    def helper(self):
        pass
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
            detail: None,
            kind,
            tags: None,
            deprecated: None,
            range,
            selection_range: range,
            children: (!children.is_empty()).then_some(children),
        }
    }

    /// pyright's symbols for [`SERVICE`]; `load` starts at its `def` line to exercise the
    /// upward decorator scan, `label` at its decorator as pyright reports it.
    fn service_symbols() -> Vec<lsp::DocumentSymbol> {
        vec![
            symbol(
                "Worker",
                lsp::SymbolKind::CLASS,
                (22, 0, 35, 24),
                vec![
                    symbol(
                        "label",
                        lsp::SymbolKind::METHOD,
                        (31, 4, 35, 24),
                        vec![symbol(
                            "inner",
                            lsp::SymbolKind::FUNCTION,
                            (33, 8, 34, 20),
                            vec![],
                        )],
                    ),
                    symbol("__init__", lsp::SymbolKind::METHOD, (28, 4, 29, 24), vec![]),
                ],
            ),
            symbol(
                "MAX_RETRIES",
                lsp::SymbolKind::CONSTANT,
                (4, 0, 4, 11),
                vec![],
            ),
            symbol("load", lsp::SymbolKind::FUNCTION, (11, 0, 19, 13), vec![]),
        ]
    }

    /// Normalized outline of [`SERVICE`] at `src/pkg/service.py`.
    fn service_outline() -> Outline {
        Python.normalize(Path::new("src/pkg/service.py"), SERVICE, service_symbols())
    }

    #[test]
    /// Decorators (multi-line included) extend the range; docstring first paragraph is `doc`;
    /// multi-line parameter lists collapse into one signature line.
    fn normalize_extends_headers_and_collapses_signatures() {
        let outline = service_outline();
        assert_eq!(outline.line_count, 36);
        let names: Vec<&str> = outline.symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["MAX_RETRIES", "load", "Worker"]);

        let load = &outline.symbols[1];
        assert_eq!(load.kind, SymbolKind::Function);
        assert_eq!(load.range, LineRange::new(8, 20));
        assert_eq!(load.body, LineRange::new(12, 20));
        assert_eq!(
            load.signature,
            "def load(path: str, retries: int = MAX_RETRIES) -> dict[str, int]"
        );
        assert_eq!(load.doc.as_deref(), Some("Load the table."));

        let constant = &outline.symbols[0];
        assert_eq!(constant.kind, SymbolKind::Constant);
        assert_eq!(constant.signature, "MAX_RETRIES = 3");
        assert_eq!(constant.doc, None);
    }

    #[test]
    /// Class members become constructor/method, nested functions stay functions, paths nest.
    fn normalize_refines_kinds_and_paths() {
        let outline = service_outline();
        let worker = &outline.symbols[2];
        assert_eq!(worker.kind, SymbolKind::Class);
        assert_eq!(worker.signature, "class Worker(Base)");
        assert_eq!(worker.doc.as_deref(), Some("A worker."));
        let init = &worker.children[0];
        assert_eq!(init.kind, SymbolKind::Constructor);
        assert_eq!(init.signature, "def __init__(self, name: str) -> None");
        let label = &worker.children[1];
        assert_eq!(label.kind, SymbolKind::Method);
        assert_eq!(label.range, LineRange::new(32, 36));
        let inner = &label.children[0];
        assert_eq!(inner.kind, SymbolKind::Function);
        assert_eq!(
            inner.path.to_string(),
            "src/pkg/service.py#Worker/label/inner"
        );
        let path = SymbolPath::parse("src/pkg/service.py#Worker/label/inner").unwrap();
        assert_eq!(outline.find(&path).map(|s| s.name.as_str()), Some("inner"));
    }

    #[test]
    /// In test files `test_*` functions and `Test*` classes are tests; helpers are not.
    fn normalize_marks_tests_only_in_test_files() {
        let symbols = vec![
            symbol(
                "test_load",
                lsp::SymbolKind::FUNCTION,
                (3, 0, 4, 28),
                vec![],
            ),
            symbol(
                "TestWorker",
                lsp::SymbolKind::CLASS,
                (7, 0, 12, 12),
                vec![
                    symbol("test_label", lsp::SymbolKind::METHOD, (8, 4, 9, 19), vec![]),
                    symbol("helper", lsp::SymbolKind::METHOD, (11, 4, 12, 12), vec![]),
                ],
            ),
        ];
        let outline = Python.normalize(Path::new("tests/test_service.py"), TESTS, symbols.clone());
        let kinds: Vec<SymbolKind> = outline.symbols.iter().map(|s| s.kind).collect();
        assert_eq!(kinds, [SymbolKind::Test, SymbolKind::Test]);
        let members: Vec<SymbolKind> = outline.symbols[1].children.iter().map(|s| s.kind).collect();
        assert_eq!(members, [SymbolKind::Test, SymbolKind::Method]);

        let outline = Python.normalize(Path::new("src/service.py"), TESTS, symbols);
        assert_eq!(outline.symbols[0].kind, SymbolKind::Function);
        assert_eq!(outline.symbols[1].kind, SymbolKind::Class);
    }

    /// A module mirroring the reported pyright output: parameter, local and `except as`
    /// Variable children on function bodies, a nested function, an annotated attribute and
    /// enum members.
    const CONTRACT: &str = "\
\"\"\"Contract module.\"\"\"

from uuid import UUID


def parse(value: str, limit: int = 8) -> UUID:
    parsed = UUID(value)
    try:
        return UUID(value)
    except ValueError as error:
        raise ValueError(str(error)) from error


def outer(count: int):
    def inner(offset: int) -> int:
        return offset + count
    return inner


class ContractModel(BaseModel):
    model_config = ConfigDict(extra=\"forbid\", strict=True, validate_assignment=True)
    work_id: UUID = Field(default_factory=uuid4)


class WorkKind(StrEnum):
    PROJECT = \"project\"
    EPIC = \"epic\"
";

    /// pyright's symbols for [`CONTRACT`]; variables cover only their name.
    fn contract_symbols() -> Vec<lsp::DocumentSymbol> {
        vec![
            symbol(
                "parse",
                lsp::SymbolKind::FUNCTION,
                (5, 0, 10, 45),
                vec![
                    symbol("value", lsp::SymbolKind::VARIABLE, (5, 10, 5, 15), vec![]),
                    symbol("limit", lsp::SymbolKind::VARIABLE, (5, 23, 5, 28), vec![]),
                    symbol("parsed", lsp::SymbolKind::VARIABLE, (6, 4, 6, 10), vec![]),
                    symbol("error", lsp::SymbolKind::VARIABLE, (9, 24, 9, 29), vec![]),
                ],
            ),
            symbol(
                "outer",
                lsp::SymbolKind::FUNCTION,
                (13, 0, 16, 16),
                vec![
                    symbol("count", lsp::SymbolKind::VARIABLE, (13, 10, 13, 15), vec![]),
                    symbol(
                        "inner",
                        lsp::SymbolKind::FUNCTION,
                        (14, 4, 16, 16),
                        vec![symbol(
                            "offset",
                            lsp::SymbolKind::VARIABLE,
                            (14, 14, 14, 20),
                            vec![],
                        )],
                    ),
                ],
            ),
            symbol(
                "ContractModel",
                lsp::SymbolKind::CLASS,
                (19, 0, 21, 44),
                vec![
                    symbol(
                        "model_config",
                        lsp::SymbolKind::VARIABLE,
                        (20, 4, 20, 16),
                        vec![],
                    ),
                    symbol(
                        "work_id",
                        lsp::SymbolKind::VARIABLE,
                        (21, 4, 21, 11),
                        vec![],
                    ),
                ],
            ),
            symbol(
                "WorkKind",
                lsp::SymbolKind::CLASS,
                (24, 0, 26, 18),
                vec![
                    symbol(
                        "PROJECT",
                        lsp::SymbolKind::ENUM_MEMBER,
                        (25, 4, 25, 11),
                        vec![],
                    ),
                    symbol("EPIC", lsp::SymbolKind::ENUM_MEMBER, (26, 4, 26, 8), vec![]),
                ],
            ),
        ]
    }

    #[test]
    /// Function-local names (parameters, locals, `except as` bindings) disappear, nested
    /// functions stay, class attributes and enum members render target/annotation or a clipped
    /// assignment.
    fn normalize_drops_function_locals_and_clips_class_attributes() {
        use agent_ide_core::lang::render::outline_text;
        let outline = Python.normalize(
            Path::new("src/pkg/contract.py"),
            CONTRACT,
            contract_symbols(),
        );
        assert_eq!(
            outline_text(&outline),
            "\
src/pkg/contract.py  (27 lines, python)
    6  def parse(value: str, limit: int = 8) -> UUID
   14  def outer(count: int)
   15    def inner(offset: int) -> int
   20  class ContractModel(BaseModel)
   21    model_config = ConfigDict(extra=\"forbid\", strict=True, vali…
   22    work_id: UUID
   25  class WorkKind(StrEnum)
   26    PROJECT = \"project\"
   27    EPIC = \"epic\"
  (9 symbols)
"
        );
    }

    #[test]
    /// PEP 8 spacing for Before/After; First/Last inside a class honour the docstring.
    fn insert_sites_follow_pep8() {
        let outline = service_outline();
        let at = |path: &str, where_| {
            Python.insert_site(SERVICE, &outline, &SymbolPath::parse(path).unwrap(), where_)
        };
        let site = |line, indent: &str, blank_before, blank_after| InsertSite {
            line,
            indent: indent.to_owned(),
            blank_before,
            blank_after,
        };
        assert_eq!(at("load", InsertWhere::Before), Ok(site(8, "", 2, 2)));
        assert_eq!(at("load", InsertWhere::After), Ok(site(21, "", 2, 2)));
        assert_eq!(
            at("Worker/__init__", InsertWhere::Before),
            Ok(site(29, "    ", 1, 1))
        );
        assert_eq!(
            at("Worker/label", InsertWhere::After),
            Ok(site(37, "    ", 1, 1))
        );
        assert_eq!(at("Worker", InsertWhere::First), Ok(site(28, "    ", 1, 1)));
        assert_eq!(at("Worker", InsertWhere::Last), Ok(site(37, "    ", 1, 0)));
        assert!(matches!(
            at("load", InsertWhere::First),
            Err(LangError::NotAContainer(_))
        ));
        assert!(matches!(
            at("Nope", InsertWhere::After),
            Err(LangError::UnknownSymbol(_))
        ));
    }

    #[test]
    /// A class without docstring takes its body's indentation; First lands after the header.
    fn insert_first_in_bare_class_uses_body_indent() {
        let source = "class Empty:\n  pass\n";
        let symbols = vec![symbol(
            "Empty",
            lsp::SymbolKind::CLASS,
            (0, 0, 1, 6),
            vec![],
        )];
        let outline = Python.normalize(Path::new("a.py"), source, symbols);
        let site = Python
            .insert_site(
                source,
                &outline,
                &SymbolPath::parse("Empty").unwrap(),
                InsertWhere::First,
            )
            .unwrap();
        assert_eq!((site.line, site.indent.as_str()), (2, "  "));
        assert_eq!((site.blank_before, site.blank_after), (0, 1));
    }

    #[test]
    /// Test file conventions.
    fn test_files_follow_pytest_conventions() {
        for file in [
            "test_a.py",
            "pkg/a_test.py",
            "tests/helpers.py",
            "conftest.py",
        ] {
            assert!(Python.is_test_file(Path::new(file)), "{file}");
        }
        for file in ["src/a.py", "tests/data.json", "testing.py"] {
            assert!(!Python.is_test_file(Path::new(file)), "{file}");
        }
    }

    /// A project with only the given environment facts.
    fn project(environment: &[(&str, &str)]) -> LanguageProject {
        LanguageProject {
            language: LANGUAGE,
            manifests: vec![PathBuf::from("pyproject.toml")],
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

    #[test]
    /// Node ids (deduplicated, uv-prefixed), the file fallback past twelve tests, files, `-k`.
    fn test_selection_builds_pytest_commands() {
        let file = PathBuf::from("tests/test_service.py");
        let id = |name: &str| TestId {
            file: file.clone(),
            name: name.to_owned(),
        };
        let target = TestTarget::Symbol {
            path: SymbolPath::parse("src/pkg/service.py#load").unwrap(),
            referencing_tests: vec![
                id("tests/test_service.py::test_load"),
                id("TestWorker/test_label"),
                id("tests/test_service.py::test_load"),
            ],
        };
        let selection = Python
            .test_selection(&project(&[("tool", "uv")]), &target)
            .unwrap();
        assert_eq!(selection.tests.len(), 2);
        assert_eq!(
            selection.command,
            argv(&[
                "uv",
                "run",
                "pytest",
                "tests/test_service.py::test_load",
                "tests/test_service.py::TestWorker::test_label",
                "--no-header",
                "-p",
                "no:cacheprovider",
            ])
        );

        let many: Vec<TestId> = (0..13)
            .map(|n| TestId {
                file: PathBuf::from(if n % 2 == 0 {
                    "tests/test_a.py"
                } else {
                    "tests/test_b.py"
                }),
                name: format!("test_{n}"),
            })
            .collect();
        let target = TestTarget::Symbol {
            path: SymbolPath::parse("load").unwrap(),
            referencing_tests: many,
        };
        let selection = Python.test_selection(&project(&[]), &target).unwrap();
        assert_eq!(selection.tests.len(), 13);
        assert_eq!(
            &selection.command[..3],
            argv(&["pytest", "tests/test_a.py", "tests/test_b.py"])
        );

        let file_target = TestTarget::File(file.clone());
        let command = Python
            .test_selection(&project(&[]), &file_target)
            .unwrap()
            .command;
        assert_eq!(&command[..2], argv(&["pytest", "tests/test_service.py"]));

        // Non-test `.py` files never become pytest targets; directories pass through and let
        // pytest select the test files inside them.
        let plain = TestTarget::File(PathBuf::from("src/hypfactory/yaml_subset.py"));
        assert!(matches!(
            Python.test_selection(&project(&[]), &plain),
            Err(LangError::Unsupported(message)) if message == "no tests in src/hypfactory/yaml_subset.py"
        ));
        let conftest = TestTarget::File(PathBuf::from("tests/conftest.py"));
        assert_eq!(
            &Python
                .test_selection(&project(&[]), &conftest)
                .unwrap()
                .command[..2],
            argv(&["pytest", "tests/conftest.py"])
        );
        let directory = TestTarget::File(PathBuf::from("tests"));
        assert_eq!(
            &Python
                .test_selection(&project(&[]), &directory)
                .unwrap()
                .command[..2],
            argv(&["pytest", "tests"])
        );
        assert!(Python.is_test_file(&PathBuf::from("test/helper.py")));
        assert!(!Python.is_test_file(&PathBuf::from("src/value.py")));

        let pattern = TestTarget::Pattern("load and not slow".to_owned());
        let command = Python
            .test_selection(&project(&[]), &pattern)
            .unwrap()
            .command;
        assert_eq!(&command[..3], argv(&["pytest", "-k", "load and not slow"]));

        let unreferenced = TestTarget::Symbol {
            path: SymbolPath::parse("load").unwrap(),
            referencing_tests: Vec::new(),
        };
        assert!(matches!(
            Python.test_selection(&project(&[]), &unreferenced),
            Err(LangError::Unsupported(_))
        ));
    }

    #[test]
    /// A passing run: counts from the `-q` summary, nothing incomplete.
    fn parse_passing_run() {
        let report = Python.parse_test_output("....\n4 passed in 0.03s\n", "");
        assert_eq!((report.passed, report.failed, report.ignored), (4, 0, 0));
        assert!(!report.incomplete);
        assert!(report.failures.is_empty());
    }

    #[test]
    /// The banner form pytest prints without own `-q` (a project `addopts -q` must not
    /// double into `-qq` and suppress this line): counts parse, nothing incomplete.
    fn parse_banner_summary_without_own_quiet_flag() {
        let report = Python.parse_test_output(
            "tests/unit/test_statistics_bets.py .....\n\n\
             ============================ 21 passed in 0.42s ============================\n",
            "",
        );
        assert_eq!((report.passed, report.failed, report.ignored), (21, 0, 0));
        assert!(!report.incomplete);
        assert!(report.failures.is_empty());
    }

    #[test]
    /// A failing run: summary counts, FAILED node ids, block location and message.
    fn parse_failing_run_with_location() {
        let output = "\
F.s                                                                     [100%]
=================================== FAILURES ===================================
__________________________________ test_load ___________________________________

    def test_load():
>       assert load(\"x\") == {\"a\": 1}
E       AssertionError: assert {} == {'a': 1}

tests/test_service.py:5: AssertionError
_____________________________ TestWorker.test_label ____________________________

    def test_label(self):
>       helper()

tests/test_service.py:10: in test_label
    helper()
src/pkg/service.py:3: in helper
    raise ValueError(\"bad\")
E   ValueError: bad

src/pkg/service.py:3: ValueError
=========================== short test summary info ============================
FAILED tests/test_service.py::test_load - AssertionError: assert {} == {'a': 1}
FAILED tests/test_service.py::TestWorker::test_label
2 failed, 1 passed, 1 skipped in 0.05s
";
        let report = Python.parse_test_output(output, "");
        assert_eq!((report.passed, report.failed, report.ignored), (1, 2, 1));
        assert!(!report.incomplete);
        assert_eq!(
            report.failures,
            [
                TestFailure {
                    name: "tests/test_service.py::test_load".to_owned(),
                    location: Some((PathBuf::from("tests/test_service.py"), 5)),
                    message: "AssertionError: assert {} == {'a': 1}".to_owned(),
                },
                TestFailure {
                    name: "tests/test_service.py::TestWorker::test_label".to_owned(),
                    location: Some((PathBuf::from("src/pkg/service.py"), 3)),
                    message: "ValueError: bad".to_owned(),
                },
            ]
        );
    }

    #[test]
    /// No summary line means the run stopped early; nothing is inferred from silence.
    fn parse_truncated_run_is_incomplete() {
        let report = Python.parse_test_output("..F", "Killed");
        assert!(report.incomplete);
        assert_eq!((report.passed, report.failed), (0, 0));
    }

    /// Fresh scratch directory for a detection fixture.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agent-ide-lang-python-{name}-{}",
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

    #[test]
    /// Manifest facts, venv interpreter, uv prefixing, Makefile and CI overrides, entry points.
    fn detect_reads_manifests_environment_and_overrides() {
        let root = scratch("full");
        put(
            &root,
            "pyproject.toml",
            "[project]\nname = \"svc\"\n\n[project.scripts]\nsvc = \"svc.cli:main\"\n\n\
             [tool.ruff]\nline-length = 100\n\n[tool.pyright]\nstrict = [\"src\"]\n",
        );
        put(&root, "requirements-dev.txt", "pytest\n");
        put(&root, "uv.lock", "");
        put(&root, ".python-version", "3.12\n");
        put(&root, ".venv/bin/python", "");
        put(&root, "src/svc/cli.py", "");
        put(&root, "svc/__main__.py", "");
        put(
            &root,
            "Makefile",
            "lint:\n\truff check src\n\ntypecheck: deps\n\tpyright\n",
        );
        put(
            &root,
            ".github/workflows/ci.yml",
            "jobs:\n  test:\n    steps:\n      - run: pip install mypy\n      - run: |\n          \
             uv sync\n          uv run pytest -x\n",
        );
        let project = Python.detect(&root).unwrap();
        fs::remove_dir_all(&root).unwrap();

        assert_eq!(
            project.manifests,
            [
                PathBuf::from("pyproject.toml"),
                PathBuf::from("requirements-dev.txt")
            ]
        );
        assert_eq!(project.interpreter, Some(root.join(".venv/bin/python")));
        let facts: Vec<(&str, &str)> = project
            .environment
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        assert_eq!(
            facts,
            [
                ("venv", ".venv"),
                ("tool", "uv"),
                ("python", "3.12"),
                ("configured", "pyright"),
                ("configured", "ruff"),
                ("formatter", "ruff"),
            ]
        );
        let commands = &project.commands;
        let test = commands.test.as_ref().unwrap();
        assert_eq!(
            (test.argv.clone(), test.source),
            (argv(&["uv", "run", "pytest", "-x"]), CommandSource::Ci)
        );
        let lint = commands.lint.as_ref().unwrap();
        assert_eq!(
            (lint.argv.clone(), lint.source),
            (argv(&["make", "lint"]), CommandSource::Makefile)
        );
        let typecheck = commands.typecheck.as_ref().unwrap();
        assert_eq!(typecheck.argv, argv(&["make", "typecheck"]));
        let format = commands.format.as_ref().unwrap();
        assert_eq!(
            (format.argv.clone(), format.source),
            (
                argv(&["uv", "run", "ruff", "format", "."]),
                CommandSource::Manifest
            )
        );
        assert_eq!(commands.build, None);
        assert_eq!(
            project.entry_points,
            [
                PathBuf::from("svc/__main__.py"),
                PathBuf::from("src/svc/cli.py")
            ]
        );
        assert_eq!(
            Python.format_command(&project, Path::new("src/svc/cli.py")),
            Some(argv(&["uv", "run", "ruff", "format", "src/svc/cli.py"]))
        );
    }

    #[test]
    /// Black wins over ruff for formatting, mypy is the fallback checker, no manifest → none.
    fn detect_prefers_black_and_mypy_fallback() {
        let root = scratch("black");
        put(&root, "setup.cfg", "[mypy]\nstrict = True\n");
        put(&root, "pyproject.toml", "[tool.black]\nline-length = 88\n");
        let project = Python.detect(&root).unwrap();
        assert_eq!(
            project.commands.typecheck.as_ref().unwrap().argv,
            argv(&["mypy", "."])
        );
        assert_eq!(project.commands.lint, None);
        assert_eq!(
            Python.format_command(&project, Path::new("a.py")),
            Some(argv(&["black", "a.py"]))
        );
        fs::remove_dir_all(&root).unwrap();
        let empty = scratch("empty");
        assert_eq!(Python.detect(&empty), None);
        fs::remove_dir_all(&empty).unwrap();
    }

    /// The syntax probe is the project's own interpreter running `ast.parse` on stdin; no
    /// subprocess runs here, only the argv shape is checked.
    #[test]
    fn syntax_probe_command_uses_the_project_interpreter() {
        let root = Path::new("/repo");
        let mut venv = project(&[]);
        venv.interpreter = Some(PathBuf::from("/repo/.venv/bin/python"));
        assert_eq!(
            Python
                .syntax_probe_command(&venv, root, Path::new("src/svc/cli.py"), None)
                .unwrap()[..2],
            ["/repo/.venv/bin/python".to_owned(), "-c".to_owned()]
        );
        assert!(
            Python
                .syntax_probe_command(&venv, root, Path::new("src/svc/cli.py"), None)
                .unwrap()[2]
                .contains("ast.parse")
        );
        // Without a venv the probe falls back to python3 on PATH, like the formatter's tools.
        let bare = project(&[]);
        assert_eq!(
            Python
                .syntax_probe_command(&bare, root, Path::new("a.py"), None)
                .unwrap()[0],
            "python3"
        );
        assert_eq!(
            Python.syntax_probe_command(&bare, root, Path::new("a.js"), None),
            None
        );
    }

    /// Checks stdin formatter argv for each supported formatter configuration.
    #[test]
    /// The card's marker rule is the same shared list checks use: a root
    /// `requirements-dev.txt` registers (the old exact-`requirements.txt` check list missed it),
    /// a nested `tools/requirements-ml.txt` registers through the nested-root probe only beside a
    /// `.py` file in the same directory, a vendor directory is never probed, a marker two levels
    /// down registers as a nested root, and a docs-only `docs/requirements.txt` does not.
    fn detect_uses_the_shared_marker_list_including_nested_requirements() {
        let root = scratch("requirements-dev-only");
        put(&root, "requirements-dev.txt", "pytest\n");
        let project = Python
            .detect(&root)
            .expect("root requirements-dev.txt registers (shared marker list)");
        assert_eq!(
            project.commands.test.as_ref().unwrap().argv,
            argv(&["pytest"])
        );
        fs::remove_dir_all(&root).unwrap();

        let root = scratch("tools-requirements");
        put(&root, "tools/requirements-ml.txt", "pandas\n");
        assert!(
            Python.detect(&root).is_none(),
            "a nested marker with no .py file beside it does not register"
        );
        put(&root, "tools/analyze.py", "import pandas\n");
        let project = Python
            .detect(&root)
            .expect("tools/requirements-ml.txt beside tools/analyze.py registers");
        assert_eq!(
            project.manifests,
            [PathBuf::from("tools/requirements-ml.txt")]
        );
        assert_eq!(
            project.commands.test.as_ref().unwrap().argv,
            argv(&["pytest"])
        );
        fs::remove_dir_all(&root).unwrap();

        let root = scratch("docs-requirements");
        put(&root, "docs/requirements.txt", "sphinx\n");
        assert!(
            Python.detect(&root).is_none(),
            "a docs-only requirements.txt does not register"
        );
        fs::remove_dir_all(&root).unwrap();

        let root = scratch("vendor-requirements");
        put(&root, "node_modules/pkg/requirements.txt", "");
        put(&root, "node_modules/pkg/lib.py", "");
        assert!(
            Python.detect(&root).is_none(),
            "vendor directories are never probed"
        );
        fs::remove_dir_all(&root).unwrap();

        let root = scratch("deep-requirements");
        put(&root, "nested/deep/requirements.txt", "");
        put(&root, "nested/deep/lib.py", "");
        let project = Python
            .detect(&root)
            .expect("a manifest two levels down registers as a nested root");
        assert_eq!(
            project.manifests,
            [PathBuf::from("nested/deep/requirements.txt")]
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// A monorepo with no manifest at the worktree root: the nested Python packages two levels
    /// down are both discovered as roots (`python_roots`), every root is listed on the project
    /// (one manifest per root, a `root` environment fact per package), and a sibling TypeScript
    /// app neither adds nor hides a root. Files of the language with no manifest anywhere still
    /// produce a project whose `project` fact says checks are unavailable.
    #[test]
    fn nested_packages_are_discovered_and_files_without_a_manifest_are_named() {
        let root = scratch("nested-packages");
        put(
            &root,
            "packages/alpha/pyproject.toml",
            "[project]\nname = \"alpha\"\n",
        );
        put(&root, "packages/alpha/src/alpha/__init__.py", "");
        put(
            &root,
            "packages/beta/pyproject.toml",
            "[project]\nname = \"beta\"\n",
        );
        put(
            &root,
            "packages/beta/tests/test_beta.py",
            "def test_beta():\n    pass\n",
        );
        put(&root, "apps/web/package.json", "{\"name\": \"web\"}\n");
        assert_eq!(
            python_roots(&root),
            [root.join("packages/alpha"), root.join("packages/beta"),]
        );
        assert!(is_python_project(&root));
        let project = Python.detect(&root).expect("nested roots register");
        assert_eq!(
            project.manifests,
            [
                PathBuf::from("packages/alpha/pyproject.toml"),
                PathBuf::from("packages/beta/pyproject.toml"),
            ]
        );
        let facts: Vec<&(String, String)> = project
            .environment
            .iter()
            .filter(|(key, _)| key == "root")
            .collect();
        assert_eq!(
            facts,
            [
                &("root".to_owned(), "packages/alpha".to_owned()),
                &("root".to_owned(), "packages/beta".to_owned()),
            ],
            "every root is listed as an environment fact the card renders"
        );
        fs::remove_dir_all(&root).unwrap();

        // Files of the language without any manifest: the card still lists Python and says no
        // project was found, while the presence rule (and with it project checks) stays off.
        let root = scratch("files-only");
        put(&root, "scripts/analyze.py", "print('hi')\n");
        assert!(!is_python_project(&root));
        let project = Python
            .detect(&root)
            .expect("files alone still make a project");
        assert!(project.manifests.is_empty());
        assert_eq!(
            project.environment,
            vec![(
                "project".to_owned(),
                "files present, no manifest found — checks unavailable".to_owned()
            )]
        );
        assert_eq!(project.commands, ProjectCommands::default());
        fs::remove_dir_all(&root).unwrap();
    }

    /// A worktree venv supplies the interpreter to nested roots without their own environment.
    #[test]
    fn nested_root_uses_worktree_venv_fallback() {
        let root = scratch("nested-root-worktree-venv");
        put(&root, ".venv/bin/python", "");
        put(&root, "svc/pyproject.toml", "[project]\nname = \"svc\"\n");
        put(&root, "svc/app.py", "match value:\n    case 1: pass\n");
        let project = Python.detect(&root).expect("nested project registers");
        assert_eq!(
            project.interpreter,
            Some(std::path::absolute(root.join(".venv/bin/python")).unwrap())
        );
        assert!(
            project
                .environment
                .iter()
                .any(|(key, value)| { key == "venv" && value == ".venv" })
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// Environment discovery beside a Python root: the conventional `.venv` wins, a suffixed
    /// sibling (`.venv-py314`, `venv310`) is found when it holds `bin/python`, and a directory
    /// without one is ignored. The first root with an environment names the `venv` fact and the
    /// interpreter; a suffixed environment beside a nested root is named with the root prefix.
    #[test]
    fn suffixed_venv_directories_are_discovered_beside_each_root() {
        let root = scratch("suffixed-venv");
        put(&root, "pyproject.toml", "[project]\nname = \"svc\"\n");
        put(&root, ".venv-py314/bin/python", "");
        put(&root, "venv-empty/lib", "");
        let venvs = venv_directories(&root);
        assert_eq!(
            venvs,
            [root.join(".venv-py314")],
            "only a directory holding bin/python counts"
        );
        let project = Python.detect(&root).expect("root manifest registers");
        assert_eq!(
            project.interpreter,
            Some(root.join(".venv-py314/bin/python")),
            "the suffixed environment's interpreter is used"
        );
        assert!(
            project
                .environment
                .iter()
                .any(|(key, value)| key == "venv" && value == ".venv-py314")
        );
        fs::remove_dir_all(&root).unwrap();

        let root = scratch("nested-suffixed-venv");
        put(&root, "packages/alpha/pyproject.toml", "");
        put(&root, "packages/alpha/src/a.py", "");
        put(&root, "packages/alpha/.venv-314/bin/python", "");
        put(&root, "packages/beta/pyproject.toml", "");
        put(&root, "packages/beta/tests/test_b.py", "");
        let project = Python.detect(&root).expect("nested roots register");
        assert_eq!(
            project.interpreter,
            Some(root.join("packages/alpha/.venv-314/bin/python"))
        );
        assert!(
            project
                .environment
                .iter()
                .any(|(key, value)| { key == "venv" && value == "packages/alpha/.venv-314" })
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn format_stdin_command_reads_stdin_for_each_formatter() {
        let ruff = project(&[("tool", "uv"), ("formatter", "ruff")]);
        assert_eq!(
            Python.format_stdin_command(&ruff, Path::new("src/svc/cli.py")),
            Some(argv(&[
                "uv",
                "run",
                "ruff",
                "format",
                "--stdin-filename",
                "src/svc/cli.py",
                "-"
            ]))
        );
        assert_eq!(
            Python.format_stdin_command(&ruff, Path::new("src/svc/cli.pyi")),
            Some(argv(&[
                "uv",
                "run",
                "ruff",
                "format",
                "--stdin-filename",
                "src/svc/cli.pyi",
                "-"
            ]))
        );
        assert_eq!(
            Python.format_stdin_command(&ruff, Path::new("src/svc/cli.js")),
            None
        );

        let black = project(&[("formatter", "black")]);
        assert_eq!(
            Python.format_stdin_command(&black, Path::new("a.py")),
            Some(argv(&["black", "-q", "-"]))
        );

        let none = project(&[]);
        assert_eq!(Python.format_stdin_command(&none, Path::new("a.py")), None);
    }

    #[test]
    fn file_doc_reads_the_module_docstring() {
        assert_eq!(
            Python.file_doc("\"\"\"Python docs\"\"\"\n"),
            Some("Python docs".into())
        );
    }
}
