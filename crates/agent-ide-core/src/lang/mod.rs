//! Language support contract for the symbol-addressed tools (v0.4).
//!
//! Everything that differs between languages lives behind `LanguageSupport`: how a project and
//! its environment are detected, how a language server's document symbols become header-inclusive
//! `Symbol`s with stable `SymbolPath`s, where new code is inserted, how tests are selected, run
//! and parsed, and which formatter the project uses. The transport (LSP session), the reply
//! rendering and the size ceilings stay in the shared layers and never depend on the language.
//!
//! The contract is deliberately synchronous and side-effect free apart from `LanguageSupport::detect`,
//! which reads manifests under the project root: implementations are plain data transformations
//! that unit tests exercise with fixture sources and hand-built document symbols.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use async_lsp::lsp_types as lsp;

/// Line and brace helpers shared by the support modules of brace-delimited languages.
pub mod brace;
pub mod edits;
pub mod environment;
/// Cross-language name facts: namespaces, facts, the fact sink and the `NameFacts` seam.
pub mod names;
pub mod path;
pub mod render;
/// Line, indentation and project-fact helpers shared by language support modules.
pub mod text;

pub use path::SymbolPath;

/// Static identity and behaviour of one supported language, defined once by its language module.
///
/// Core code never names a language: it keys behaviour on the [`Language`] handle built from a
/// descriptor and reaches everything language-specific through the descriptor's traits. A
/// language module defines its descriptor as a `static` and exposes a `LANGUAGE` constant made
/// with [`Language::of`]; the application registers its languages once at startup with
/// [`install`].
pub struct LanguageDescriptor {
    /// Stable lowercase identifier used in replies, journal lines, telemetry, launcher
    /// configuration sections and the `problems` language filter. Unique among registered
    /// languages.
    pub id: &'static str,
    /// Human-facing name, also the handle's `Debug` form.
    pub display_name: &'static str,
    /// File extensions (without the dot) whose files this language owns. An extension belongs to
    /// at most one registered language.
    pub extensions: &'static [&'static str],
    /// Project manifest the `ide.start` description says the project card replaces reading, or
    /// `None` when the language has no manifest worth naming there.
    pub card_manifest: Option<&'static str>,
    /// Directories below the user's home where the language's user-installed tools (formatters)
    /// live, prepended to the formatter `PATH` in registration order.
    pub home_tool_dirs: &'static [&'static str],
    /// Symbol-tool behaviour: outlines, insertion points, test selection and formatting.
    pub support: &'static dyn LanguageSupport,
    /// Confined project-check integration, when the language has a checker.
    pub checks: Option<&'static dyn crate::checks::LanguageChecks>,
    /// Language-server integration, when the language has one.
    pub server: Option<&'static dyn crate::intelligence::server::LanguageServer>,
    /// Cross-language name facts, when the language states any.
    pub names: Option<&'static dyn names::NameFacts>,
}

/// A registered language: a cheap copyable handle to its [`LanguageDescriptor`].
///
/// Equality and hashing use the descriptor's `id`. Ordering follows registration order (the
/// order `ide.start`, the `<agent-ide>` block and problem pages list languages), with
/// unregistered languages after every registered one, ordered by `id`. The handle serializes as
/// its `id` and deserializes only an `id` of a registered language.
#[derive(Clone, Copy)]
pub struct Language(&'static LanguageDescriptor);

/// Languages registered for this process, in registration order; set once by [`install`].
static REGISTRY: std::sync::OnceLock<Vec<Language>> = std::sync::OnceLock::new();

/// Registers the process's languages, in the order replies list them.
///
/// Only the first call installs; later calls change nothing. Returns whether `languages` is the
/// installed set, so a caller can detect a conflicting earlier registration. Must run before any
/// launcher configuration is parsed or any path is mapped to a language.
pub fn install(languages: &[Language]) -> bool {
    let installed = REGISTRY.get_or_init(|| languages.to_vec());
    installed.as_slice() == languages
}

/// Returns the registered languages in registration order; empty before [`install`].
pub fn registered() -> &'static [Language] {
    REGISTRY.get().map_or(&[], Vec::as_slice)
}

impl Language {
    /// Builds the handle for one language module's descriptor.
    pub const fn of(descriptor: &'static LanguageDescriptor) -> Self {
        Self(descriptor)
    }

    /// Stable lowercase identifier used in replies and journal lines.
    pub const fn name(self) -> &'static str {
        self.0.id
    }

    /// Same as [`Language::name`]: the canonical lowercase identifier the feed and the
    /// `ide.context` problems kind use.
    pub const fn as_str(self) -> &'static str {
        self.0.id
    }

    /// Human-facing name.
    pub const fn display_name(self) -> &'static str {
        self.0.display_name
    }

    /// The full static descriptor.
    pub const fn descriptor(self) -> &'static LanguageDescriptor {
        self.0
    }

    /// The language's symbol-tool behaviour.
    pub fn support(self) -> &'static dyn LanguageSupport {
        self.0.support
    }

    /// The language's project-check integration, if any.
    pub fn checks(self) -> Option<&'static dyn crate::checks::LanguageChecks> {
        self.0.checks
    }

    /// The language's server integration, if any.
    pub fn server(self) -> Option<&'static dyn crate::intelligence::server::LanguageServer> {
        self.0.server
    }

    /// The language's cross-language name facts, if any.
    pub fn names(self) -> Option<&'static dyn names::NameFacts> {
        self.0.names
    }

    /// Selects the registered language owning `path`'s extension; `None` for files no registered
    /// language owns.
    pub fn for_path(path: &Path) -> Option<Self> {
        let extension = path.extension().and_then(|value| value.to_str())?;
        registered()
            .iter()
            .copied()
            .find(|language| language.0.extensions.contains(&extension))
    }

    /// Selects the registered language with identifier `id`.
    pub fn by_id(id: &str) -> Option<Self> {
        registered()
            .iter()
            .copied()
            .find(|language| language.0.id == id)
    }

    /// Reports whether `worktree` looks like a project of this language, per the language's cheap
    /// root-level presence rule (T10B); always `false` for a language without project checks. A
    /// language absent from a worktree is never checked and never mentioned.
    pub fn is_present(self, worktree: &Path) -> bool {
        self.0
            .checks
            .is_some_and(|checks| checks.is_present(worktree))
    }

    /// Position in registration order; unregistered languages sort last.
    fn rank(self) -> usize {
        registered()
            .iter()
            .position(|language| *language == self)
            .unwrap_or(usize::MAX)
    }
}

impl PartialEq for Language {
    /// Compares identifiers.
    fn eq(&self, other: &Self) -> bool {
        self.0.id == other.0.id
    }
}

impl Eq for Language {}

impl std::hash::Hash for Language {
    /// Hashes the identifier.
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.id.hash(state);
    }
}

impl PartialOrd for Language {
    /// Total order; see [`Language`].
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Language {
    /// Registration order, then identifier.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank()
            .cmp(&other.rank())
            .then_with(|| self.0.id.cmp(other.0.id))
    }
}

impl fmt::Debug for Language {
    /// Writes the display name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.display_name)
    }
}

impl fmt::Display for Language {
    /// Writes the identifier.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl serde::Serialize for Language {
    /// Serializes the identifier.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

impl<'de> serde::Deserialize<'de> for Language {
    /// Accepts only the identifier of a registered language.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let id = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        Self::by_id(&id)
            .ok_or_else(|| serde::de::Error::custom(format_args!("unknown language `{id}`")))
    }
}

/// Closed symbol kinds shared by every language; language-specific kinds map onto the nearest one.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SymbolKind {
    Module,
    Namespace,
    Struct,
    Enum,
    Class,
    Interface,
    Trait,
    Impl,
    TypeAlias,
    Function,
    Method,
    Constructor,
    Field,
    Variant,
    Constant,
    Variable,
    Test,
    Other,
}

impl SymbolKind {
    /// Stable lowercase name used in outlines and symbol cards.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Module => "module",
            Self::Namespace => "namespace",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Trait => "trait",
            Self::Impl => "impl",
            Self::TypeAlias => "type",
            Self::Function => "fn",
            Self::Method => "method",
            Self::Constructor => "constructor",
            Self::Field => "field",
            Self::Variant => "variant",
            Self::Constant => "const",
            Self::Variable => "var",
            Self::Test => "test",
            Self::Other => "symbol",
        }
    }

    /// Parses one closed kind name (`name()`'s exact output); `None` for anything else, including
    /// case variants.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "module" => Self::Module,
            "namespace" => Self::Namespace,
            "struct" => Self::Struct,
            "enum" => Self::Enum,
            "class" => Self::Class,
            "interface" => Self::Interface,
            "trait" => Self::Trait,
            "impl" => Self::Impl,
            "type" => Self::TypeAlias,
            "fn" => Self::Function,
            "method" => Self::Method,
            "constructor" => Self::Constructor,
            "field" => Self::Field,
            "variant" => Self::Variant,
            "const" => Self::Constant,
            "var" => Self::Variable,
            "test" => Self::Test,
            "symbol" => Self::Other,
            _ => return None,
        })
    }

    /// Whether the kind owns members that get their own path segment.
    pub const fn is_container(self) -> bool {
        matches!(
            self,
            Self::Module
                | Self::Namespace
                | Self::Struct
                | Self::Enum
                | Self::Class
                | Self::Interface
                | Self::Trait
                | Self::Impl
        )
    }
}

/// Inclusive 1-based line range, the unit every reply prints and every edit replaces.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(deny_unknown_fields)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

impl LineRange {
    /// Builds a range; `end` is clamped so a range never runs backwards.
    pub fn new(start: u32, end: u32) -> Self {
        Self {
            start: start.max(1),
            end: end.max(start.max(1)),
        }
    }

    /// Number of lines covered.
    pub fn len(self) -> u32 {
        self.end - self.start + 1
    }

    /// Whether the range covers no lines (never true for a constructed range; kept for symmetry).
    pub fn is_empty(self) -> bool {
        self.end < self.start
    }
}

impl fmt::Display for LineRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.start == self.end {
            write!(f, "{}", self.start)
        } else {
            write!(f, "{}–{}", self.start, self.end)
        }
    }
}

/// One normalized symbol: header-inclusive range, one-line signature, first doc paragraph, children.
///
/// `range` starts at the symbol's header (doc comments, attributes, decorators, JSDoc) so that
/// reading, replacing and deleting the symbol always moves its documentation with it. `body`
/// is the range the language server reported for the declaration itself; `range.start <= body.start`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Symbol {
    pub path: SymbolPath,
    pub kind: SymbolKind,
    pub name: String,
    pub range: LineRange,
    pub body: LineRange,
    /// Declaration collapsed to one line: `pub fn establish_start(&mut self, …) -> BindingStatus`.
    pub signature: String,
    /// First paragraph of the documentation, if any, with comment markers stripped.
    pub doc: Option<String>,
    pub children: Vec<Symbol>,
}

impl Symbol {
    /// Depth-first walk over this symbol and its descendants.
    pub fn walk<'a>(&'a self, visit: &mut dyn FnMut(&'a Symbol)) {
        visit(self);
        for child in &self.children {
            child.walk(visit);
        }
    }
}

/// Skeleton of one file: every normalized symbol with its members, no bodies.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outline {
    pub file: PathBuf,
    pub language: Language,
    pub line_count: u32,
    pub symbols: Vec<Symbol>,
}

impl Outline {
    /// Finds the symbol addressed by `path` (file already matched by the caller); `None` for a
    /// file path or an unknown symbol.
    ///
    /// Same-named siblings are tried in source order and the walk backtracks: where a type `Foo`
    /// and its implementation blocks share the segment `Foo`, `Foo` finds the type while `Foo/new`
    /// finds the method inside whichever block declares it. A field and a method of the same name
    /// (`Foo/source` declared by the type and by one of its blocks) resolve to the method: the field
    /// is read and edited through its type.
    pub fn find(&self, path: &SymbolPath) -> Option<&Symbol> {
        fn descend<'a>(level: &'a [Symbol], segments: &[String], found: &mut Vec<&'a Symbol>) {
            let Some((first, rest)) = segments.split_first() else {
                return;
            };
            for symbol in level.iter().filter(|symbol| symbol.name == *first) {
                if rest.is_empty() {
                    found.push(symbol);
                } else {
                    descend(&symbol.children, rest, found);
                }
            }
        }
        let mut found = Vec::new();
        descend(&self.symbols, path.segments(), &mut found);
        found
            .iter()
            .find(|symbol| symbol.kind != SymbolKind::Field)
            .or(found.first())
            .copied()
    }

    /// All symbols whose name equals `name`, at any depth, for ambiguity reports.
    pub fn named<'a>(&'a self, name: &str) -> Vec<&'a Symbol> {
        let mut matches = Vec::new();
        for symbol in &self.symbols {
            symbol.walk(&mut |candidate| {
                if candidate.name == name {
                    matches.push(candidate);
                }
            });
        }
        matches
    }
}

/// Where new code goes relative to an anchor symbol.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub enum InsertWhere {
    /// Immediately before the anchor's header.
    Before,
    /// Immediately after the anchor's last line.
    After,
    /// As the first member of a container anchor.
    First,
    /// As the last member of a container anchor.
    Last,
}

/// Exact insertion point computed by the language module.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InsertSite {
    /// New code is inserted before this 1-based line (`line_count + 1` appends).
    pub line: u32,
    /// Indentation each inserted line receives (copied from the anchor's siblings).
    pub indent: String,
    /// Blank lines to emit before the inserted code.
    pub blank_before: u8,
    /// Blank lines to emit after the inserted code.
    pub blank_after: u8,
}

/// What `ide.test` was asked to run.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub enum TestTarget {
    /// Tests that reference this symbol (the caller resolves references; the language module maps
    /// them onto test identifiers and a command).
    Symbol {
        path: SymbolPath,
        referencing_tests: Vec<TestId>,
    },
    /// Every test in one file or module.
    File(PathBuf),
    /// A raw filter passed to the language's test runner.
    Pattern(String),
}

/// One test the runner can address by name.
#[derive(Clone, Debug, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestId {
    pub file: PathBuf,
    /// Runner-specific identifier: `assistance::worker::tests::stop_cancels_queue`,
    /// `tests/test_candles.py::test_empty_day`, `Class > method`.
    pub name: String,
}

/// Command that runs a selection, plus the identifiers it is expected to cover.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestSelection {
    pub tests: Vec<TestId>,
    /// argv, program first; run from the project root.
    pub command: Vec<String>,
}

/// One failed test as the runner reported it.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestFailure {
    pub name: String,
    /// `file:line` when the runner names one.
    pub location: Option<(PathBuf, u32)>,
    /// The assertion or panic message: its first line, or a few lines joined with ` | ` when the
    /// runner's parser keeps the values that explain it.
    pub message: String,
}

/// Parsed runner output; counts are what the runner printed, never inferred from silence.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestReport {
    pub passed: u32,
    pub failed: u32,
    pub ignored: u32,
    pub failures: Vec<TestFailure>,
    /// The runner stopped before printing a summary (budget, crash, signal).
    pub incomplete: bool,
}

/// Structural verdict for a candidate file text, used to refuse a broken edit before any write.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub enum SyntaxVerdict {
    /// The text parses structurally.
    Clean,
    /// The text does not parse; 1-based line (0 when unknown) in the checked text, bounded message.
    Failed { line: u32, message: String },
    /// No structural checker is available here; the edit proceeds and the project check reports.
    Unchecked,
}

/// Where a project command came from; replies print it so the agent knows how far to trust it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CommandSource {
    /// A `key: command` line inside the ` ```agent-ide ` fenced block in `AGENTS.md`.
    Agents,
    /// Same block, read from `CLAUDE.md` when `AGENTS.md` declares none.
    Claude,
    Ci,
    Makefile,
    Manifest,
    Readme,
    Default,
}

impl CommandSource {
    /// Stable lowercase name used in the project card.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Agents => "agents",
            Self::Claude => "claude",
            Self::Ci => "ci",
            Self::Makefile => "makefile",
            Self::Manifest => "manifest",
            Self::Readme => "readme",
            Self::Default => "default",
        }
    }
}

/// One project command with its provenance.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectCommand {
    pub argv: Vec<String>,
    pub source: CommandSource,
}

/// The commands a language module knows for a project; `None` means the project has none.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectCommands {
    pub build: Option<ProjectCommand>,
    pub check: Option<ProjectCommand>,
    pub test: Option<ProjectCommand>,
    pub lint: Option<ProjectCommand>,
    pub format: Option<ProjectCommand>,
    pub typecheck: Option<ProjectCommand>,
}

/// One language's view of a project root.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LanguageProject {
    pub language: Language,
    /// Manifest files that established the project, relative to the root.
    pub manifests: Vec<PathBuf>,
    /// Environment facts as `name: value` pairs: `toolchain: 1.98.1`, `venv: .venv`, `node: 24.4.0`.
    pub environment: Vec<(String, String)>,
    /// Absolute interpreter or toolchain the language server and runners must use, if one applies.
    pub interpreter: Option<PathBuf>,
    pub commands: ProjectCommands,
    /// Entry points relative to the root: `src/main.rs`, `src/index.ts`, `package/__main__.py`.
    pub entry_points: Vec<PathBuf>,
}

/// The externally configured programs one language's stdin syntax probe may run: an absolute
/// program and the absolute module it runs, resolved by the worker from the effective launcher
/// configuration for that language's server. Both are opaque paths here; the owning language
/// decides how its probe uses them (see [`LanguageSupport::syntax_probe_command`]).
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbePrograms {
    /// Absolute program that runs the module.
    pub program: PathBuf,
    /// Absolute module the program runs.
    pub module: PathBuf,
}

/// Failures a language module reports; the reply layer maps them onto closed failure codes.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum LangError {
    /// The anchor or target symbol is not in the outline.
    UnknownSymbol(SymbolPath),
    /// `First`/`Last` on a symbol that owns no members.
    NotAContainer(SymbolPath),
    /// The source does not parse well enough to place code or read a header.
    Unparseable(String),
    /// The project has no runner or command for the request.
    Unsupported(String),
}

impl fmt::Display for LangError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSymbol(path) => write!(f, "unknown symbol {path}"),
            Self::NotAContainer(path) => write!(f, "{path} has no members"),
            Self::Unparseable(reason) => write!(f, "unparseable source: {reason}"),
            Self::Unsupported(reason) => write!(f, "unsupported: {reason}"),
        }
    }
}

impl std::error::Error for LangError {}

/// The per-language contract. Implementations are stateless; every method is a pure function of
/// its inputs except `detect`, which reads manifests under `root`, and `syntax_probe_command`,
/// which also probes the files its `root` and configured programs name.
pub trait LanguageSupport: Send + Sync {
    fn language(&self) -> Language;

    /// Reads manifests and environment markers under `root`; `None` when the language is absent.
    fn detect(&self, root: &Path) -> Option<LanguageProject>;

    /// Turns the server's document symbols for `file` into header-inclusive, path-addressed
    /// symbols. `source` is the exact text the symbols were computed from. Test modules and test
    /// functions get [`SymbolKind::Test`] so outlines can collapse them.
    fn normalize(&self, file: &Path, source: &str, symbols: Vec<lsp::DocumentSymbol>) -> Outline;

    /// Computes where `where_` relative to `anchor` lands, with the indentation the neighbours use.
    fn insert_site(
        &self,
        source: &str,
        outline: &Outline,
        anchor: &SymbolPath,
        where_: InsertWhere,
    ) -> Result<InsertSite, LangError>;

    /// Whether `file` is a test file by the language's conventions (`tests/`, `test_*.py`, `*.test.ts`, `*_test.go`).
    fn is_test_file(&self, file: &Path) -> bool;

    /// Builds the runner command for a target, run from the project root.
    fn test_selection(
        &self,
        project: &LanguageProject,
        target: &TestTarget,
    ) -> Result<TestSelection, LangError>;

    /// Parses runner output into counts and the first failures; never infers success from silence.
    fn parse_test_output(&self, stdout: &str, stderr: &str) -> TestReport;

    /// Formatter command for one file, if the project has one; run from the project root.
    fn format_command(&self, project: &LanguageProject, file: &Path) -> Option<Vec<String>>;

    /// Formatter that reads the file's text on stdin and writes the formatted text to stdout, for a
    /// candidate that is not on disk yet. `file` is the relative path the text belongs to (formatters
    /// pick their config and language from it). `None` when the project has no formatter for it.
    fn format_stdin_command(&self, project: &LanguageProject, file: &Path) -> Option<Vec<String>>;

    /// In-process structural check when this module has a parser available.
    /// Bounded like the lexical outline: a text the checker refuses to attempt is [`SyntaxVerdict::Unchecked`],
    /// which lets the edit proceed and the project check report.
    fn syntax_verdict(&self, file: &Path, source: &str) -> SyntaxVerdict {
        let _ = (file, source);
        SyntaxVerdict::Unchecked
    }

    /// stdin probe command for a bounded external structural checker. `None` means no probe here,
    /// i.e. [`SyntaxVerdict::Unchecked`]. The caller runs it from the project `root` with the
    /// candidate on stdin and maps its output through [`SyntaxVerdict::from_probe`]; the probe's
    /// first output line must be `<line>: <message>` and a nonzero exit without that shape means
    /// "no checker". `configured` carries the programs the caller resolved from the effective
    /// launcher configuration for this language's server (see [`ProbePrograms`]); the probe
    /// prefers them over project-local tools and never reads the process environment.
    /// Same contract as [`Self::format_stdin_command`].
    fn syntax_probe_command(
        &self,
        project: &LanguageProject,
        root: &Path,
        file: &Path,
        configured: Option<&ProbePrograms>,
    ) -> Option<Vec<String>> {
        let _ = (project, root, file, configured);
        None
    }

    /// Runner identifier of the test whose outline path (segments joined with `::`) is
    /// `outline_path` in the project-relative `file`. Defaults to `outline_path` itself; a
    /// language whose runner addresses tests by module path derives it from `file`.
    fn test_id(&self, file: &Path, outline_path: &str) -> String {
        let _ = file;
        outline_path.to_owned()
    }

    /// Name of the separately built test binary `file` belongs to, when the language compiles
    /// some test files into their own binaries; `None` (the default) otherwise. Used to tell the
    /// agent that a selection spans several binaries.
    fn test_binary(&self, file: &Path) -> Option<String> {
        let _ = file;
        None
    }

    /// Pinned toolchain for a test command whose program is `program`: the executable to run
    /// instead of resolving `program` through `PATH`, and a directory to put first on the command's
    /// `PATH` so the tools it starts resolve from the same toolchain. `None` (the default) runs
    /// `program` as given with the inherited environment. Reads only process configuration (the
    /// daemon's environment) and the filesystem; never the agent's.
    fn test_toolchain(&self, program: &str) -> Option<(PathBuf, PathBuf)> {
        let _ = program;
        None
    }

    /// Every project root's environment in `worktree`, honouring the stored selections
    /// ([`environment::selections`]): the language's single resolver, which the card, project
    /// checks, the language server session and test and format commands all use. Reads files
    /// only and never runs an environment manager. Empty (the default) when the language has no
    /// selectable environment.
    fn environments(&self, worktree: &Path) -> Vec<environment::ResolvedEnv> {
        let _ = worktree;
        Vec::new()
    }

    /// Accepts or refuses `selector` for the project `root` (relative to `worktree`) before it
    /// is stored. `Err` carries the cause and the way out: an unknown candidate with the
    /// candidates, or a project pin that overrides any selection. Admission of absolute paths
    /// against the allowed roots is the caller's. The default refuses: nothing to choose.
    fn check_selection(&self, worktree: &Path, root: &Path, selector: &str) -> Result<(), String> {
        let _ = (worktree, root, selector);
        Err("this language has no selectable environment".to_owned())
    }

    /// How a test or format command whose program is `program`, started in `cwd` inside
    /// `worktree`, runs in the resolved environment; `None` runs it as given with the inherited
    /// environment. The default keeps the pinned toolchain of [`Self::test_toolchain`].
    fn command_env(
        &self,
        worktree: &Path,
        cwd: &Path,
        program: &str,
    ) -> Option<environment::CommandEnv> {
        let _ = (worktree, cwd);
        self.test_toolchain(program)
            .map(|(executable, directory)| environment::CommandEnv {
                argv_prefix: vec![executable.into_os_string()],
                path_prefix: Some(directory),
                vars: Vec::new(),
            })
    }

    /// First module documentation line of a file's `text` (a directory outline shows it next to
    /// the file), or `None` (the default) when the language has no module-doc convention or the
    /// file has none.
    fn file_doc(&self, text: &str) -> Option<String> {
        let _ = text;
        None
    }

    /// Outline computed from the text alone; `None` (the default) keeps outlines server-backed.
    /// When no registered server owns the file's extension, the symbol tools outline, read and
    /// describe the file from this and report usages and callers as unavailable. A language with
    /// a server may still answer: graph use-site nodes and name-card addresses use it where the
    /// server is not asked or cannot answer, and — only when
    /// [`LanguageSupport::outline_while_loading`] opts in — outline, read and symbol edits use it
    /// while that server is still loading, when it is unavailable (its workspace failed to load,
    /// or no launch configures it), and when its ready session's documentSymbols exchange
    /// failed.
    fn outline_from_source(&self, file: &Path, source: &str) -> Option<Outline> {
        let _ = (file, source);
        None
    }

    /// Whether [`LanguageSupport::outline_from_source`] answers `ide.outline`, `ide.read` and
    /// symbol-addressed `ide.edit` while the language's registered server is still loading, and
    /// when that server is unavailable (its workspace failed to load, or no launch configures
    /// it) or its ready session's documentSymbols exchange failed; `false` (the default) keeps
    /// those calls waiting for the server, or refusing `provider_unavailable` when it failed.
    ///
    /// Opting in is an obligation, because a symbol edit splices by the outline's ranges: every
    /// `Some` outline it returns for a text must equal what [`LanguageSupport::normalize`] makes
    /// of the server's document symbols for the same text — the same addresses, kinds, ranges,
    /// bodies, signatures, docs, children and order — and a text it cannot outline with that
    /// guarantee must answer `None`, which keeps the call waiting for the server, or refusing
    /// `provider_unavailable` when it failed. An address the source outline does not contain is
    /// not proven absent: while the server loads the call waits for it instead of answering
    /// `unknown_symbol`, and when it is unavailable the call refuses `provider_unavailable`.
    fn outline_while_loading(&self) -> bool {
        false
    }

    /// Whether [`LanguageSupport::outline_from_source`] answers `ide.outline`, `ide.read` and
    /// symbol-addressed `ide.edit` when the language's registered server refused to verify the
    /// file's project inputs (`resolution_unverified`, for example a project configuration the
    /// exact-resolution rules refuse). `false` (the default) keeps the refusal.
    ///
    /// Unlike [`LanguageSupport::outline_while_loading`] this state is terminal for the file —
    /// no later server answer is lost by answering now — so the obligation is only the equality
    /// one: every `Some` outline must equal what [`LanguageSupport::normalize`] makes of the
    /// server's symbols, and a text that cannot meet it must answer `None` (keeping the
    /// refusal). An address the source outline does not contain answers `unknown_symbol`, which
    /// names exactly what the source scan proved.
    fn outline_when_resolution_unverified(&self) -> bool {
        false
    }

    /// Whether tests live only in files [`LanguageSupport::is_test_file`] accepts, so a path
    /// target that is not a test file can be answered "no tests" without running anything.
    /// Defaults to `false`: tests may sit next to the code they test.
    fn tests_only_in_test_files(&self) -> bool {
        false
    }
}

/// Reads the exact lines `range` covers from `source`, keeping line terminators.
pub fn slice_lines(source: &str, range: LineRange) -> String {
    source
        .split_inclusive('\n')
        .skip((range.start - 1) as usize)
        .take(range.len() as usize)
        .collect()
}

/// Number of lines in `source`, counting an unterminated last line.
pub fn line_count(source: &str) -> u32 {
    let count = source.matches('\n').count() as u32;
    if source.is_empty() || source.ends_with('\n') {
        count
    } else {
        count + 1
    }
}

/// Converts an LSP range into a 1-based inclusive line range.
pub fn lines_of(range: &lsp::Range) -> LineRange {
    let start = range.start.line + 1;
    // An end position at character 0 means "up to the end of the previous line".
    let end = if range.end.character == 0 && range.end.line > range.start.line {
        range.end.line
    } else {
        range.end.line + 1
    };
    LineRange::new(start, end)
}

/// Maps the LSP kind onto the closed kind set; language modules refine `Function` into `Method`
/// or `Test` from context.
pub fn kind_of(kind: lsp::SymbolKind) -> SymbolKind {
    match kind {
        lsp::SymbolKind::MODULE | lsp::SymbolKind::PACKAGE | lsp::SymbolKind::FILE => {
            SymbolKind::Module
        }
        lsp::SymbolKind::NAMESPACE => SymbolKind::Namespace,
        lsp::SymbolKind::STRUCT => SymbolKind::Struct,
        lsp::SymbolKind::ENUM => SymbolKind::Enum,
        lsp::SymbolKind::CLASS => SymbolKind::Class,
        lsp::SymbolKind::INTERFACE => SymbolKind::Interface,
        lsp::SymbolKind::OBJECT => SymbolKind::Impl,
        lsp::SymbolKind::TYPE_PARAMETER => SymbolKind::TypeAlias,
        lsp::SymbolKind::FUNCTION => SymbolKind::Function,
        lsp::SymbolKind::METHOD => SymbolKind::Method,
        lsp::SymbolKind::CONSTRUCTOR => SymbolKind::Constructor,
        lsp::SymbolKind::FIELD | lsp::SymbolKind::PROPERTY => SymbolKind::Field,
        lsp::SymbolKind::ENUM_MEMBER => SymbolKind::Variant,
        lsp::SymbolKind::CONSTANT => SymbolKind::Constant,
        lsp::SymbolKind::VARIABLE => SymbolKind::Variable,
        _ => SymbolKind::Other,
    }
}

/// Neutral languages for unit tests of language-independent code.
///
/// They carry identity, ordering, extensions and a root-marker presence rule but no real
/// language behaviour, so core tests never depend on a bundled language. `ALPHA`, `BETA` and
/// `GAMMA` have project checks (present when `<id>.toml` exists at the worktree root) and tiny
/// token-based name-fact providers; `GAMMA` also outlines from source; `DELTA` has neither. None
/// of those four has a server. `EPSILON` alone has one, the recording [`FixtureServer`], for tests
/// of provider ownership. Their identifiers sort alpha, beta, delta, epsilon, gamma.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// Symbol-tool stub for the test language with the carried identifier: detects nothing,
    /// normalizes to an empty outline, supports no runner.
    struct Support(&'static str);

    impl LanguageSupport for Support {
        /// The stubbed language.
        fn language(&self) -> Language {
            Language::by_id(self.0)
                .or_else(|| {
                    [ALPHA, BETA, GAMMA, DELTA, EPSILON]
                        .into_iter()
                        .find(|l| l.name() == self.0)
                })
                .expect("a test language")
        }
        /// Detects marker-based environment fixtures and a root `fmt.toml` project whose only
        /// command is the stdin formatter below, so core tests can exercise a formatting edit
        /// without a real toolchain.
        fn detect(&self, root: &Path) -> Option<LanguageProject> {
            ((self.0 == "gamma" && root.join("fmt.toml").exists())
                || (self.0 == "alpha" && root.join("env.fixture").exists()))
            .then(|| LanguageProject {
                language: self.language(),
                manifests: vec![PathBuf::from("fmt.toml")],
                environment: Vec::new(),
                interpreter: None,
                commands: ProjectCommands::default(),
                entry_points: Vec::new(),
            })
        }
        /// Resolves the test-only `env.fixture` list; absent markers preserve existing tests.
        fn environments(&self, worktree: &Path) -> Vec<environment::ResolvedEnv> {
            if self.0 != "alpha" {
                return Vec::new();
            }
            let Ok(labels) = std::fs::read_to_string(worktree.join("env.fixture")) else {
                return Vec::new();
            };
            let candidates: Vec<_> = labels
                .lines()
                .map(|label| environment::EnvCandidate {
                    label: label.to_owned(),
                    path: worktree.join(label),
                    version: Some("1.2.3".to_owned()),
                    broken: label == "broken",
                })
                .collect();
            let selection = environment::selections(worktree, self.language())
                .into_iter()
                .find(|choice| choice.root.as_os_str().is_empty());
            let chosen = selection
                .as_ref()
                .and_then(|choice| {
                    candidates
                        .iter()
                        .find(|candidate| candidate.label == choice.selector)
                })
                .or_else(|| {
                    if selection.is_none() {
                        candidates.first()
                    } else {
                        None
                    }
                })
                .cloned();
            let source = chosen.as_ref().map(|_| {
                if selection.is_some() {
                    environment::EnvSource::Selected
                } else {
                    environment::EnvSource::Discovered
                }
            });
            let identity = format!(
                "{labels}:{}",
                chosen
                    .as_ref()
                    .map_or("", |candidate| candidate.label.as_str())
            );
            vec![environment::ResolvedEnv {
                root: PathBuf::new(),
                chosen,
                source,
                candidates,
                warnings: Vec::new(),
                missing_next_step: Some("create an environment or choose auto".into()),
                identity,
            }]
        }

        /// Accepts a listed test candidate, otherwise exposes a stable refusal reason.
        fn check_selection(
            &self,
            worktree: &Path,
            _root: &Path,
            selector: &str,
        ) -> Result<(), String> {
            if self.environments(worktree).iter().any(|env| {
                env.candidates
                    .iter()
                    .any(|candidate| candidate.label == selector)
            }) {
                Ok(())
            } else {
                Err("candidate absent; choose a listed environment".into())
            }
        }

        /// Replaces only the fake runner with a shell probe for prefix, PATH and variables.
        fn command_env(
            &self,
            worktree: &Path,
            _cwd: &Path,
            program: &str,
        ) -> Option<environment::CommandEnv> {
            (self.0 == "alpha" && program == "env-fixture-runner").then(|| {
                environment::CommandEnv {
                    argv_prefix: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "printf '%s\\n%s\\npass\\n' \"$ENV_FIXTURE\" \"$PATH\"".into(),
                    ],
                    path_prefix: Some(worktree.join("env-bin")),
                    vars: vec![("ENV_FIXTURE".into(), "resolved".into())],
                }
            })
        }

        /// An outline with no symbols, except for epsilon: it keeps each top-level server symbol
        /// the way epsilon's source outline reads a `sym <name>` … `end` block (kind `Other`,
        /// signature `sym <name>`, the lines of its range), as the opt-in contract of
        /// [`LanguageSupport::outline_while_loading`] requires.
        fn normalize(
            &self,
            file: &Path,
            source: &str,
            symbols: Vec<lsp::DocumentSymbol>,
        ) -> Outline {
            let symbols = if self.0 == "epsilon" {
                symbols
                    .into_iter()
                    .map(|symbol| {
                        let range =
                            LineRange::new(symbol.range.start.line + 1, symbol.range.end.line + 1);
                        Symbol {
                            path: SymbolPath::new(
                                Some(file.to_path_buf()),
                                vec![symbol.name.clone()],
                            ),
                            kind: SymbolKind::Other,
                            signature: format!("sym {}", symbol.name),
                            name: symbol.name,
                            range,
                            body: range,
                            doc: None,
                            children: Vec::new(),
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            };
            Outline {
                file: file.to_path_buf(),
                language: self.language(),
                line_count: line_count(source),
                symbols,
            }
        }
        /// No insertion points.
        fn insert_site(
            &self,
            _source: &str,
            _outline: &Outline,
            anchor: &SymbolPath,
            _where_: InsertWhere,
        ) -> Result<InsertSite, LangError> {
            Err(LangError::UnknownSymbol(anchor.clone()))
        }
        /// Files named `test_*` are tests.
        fn is_test_file(&self, file: &Path) -> bool {
            file.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("test_"))
        }
        /// No runner.
        fn test_selection(
            &self,
            _project: &LanguageProject,
            _target: &TestTarget,
        ) -> Result<TestSelection, LangError> {
            Err(LangError::Unsupported("test language".into()))
        }
        /// Counts `pass` and `fail` lines of `stdout`.
        fn parse_test_output(&self, stdout: &str, _stderr: &str) -> TestReport {
            TestReport {
                passed: stdout.lines().filter(|line| *line == "pass").count() as u32,
                failed: stdout.lines().filter(|line| *line == "fail").count() as u32,
                ..TestReport::default()
            }
        }
        /// A leading `#!doc ` line is the module documentation.
        fn file_doc(&self, text: &str) -> Option<String> {
            text.lines()
                .next()?
                .strip_prefix("#!doc ")
                .map(str::to_owned)
        }
        /// Epsilon, whose server can fail, falls back to its source outline like a real language.
        fn outline_while_loading(&self) -> bool {
            self.0 == "epsilon"
        }
        /// Gamma and epsilon outline from their text: `sym <name>` opens a symbol, `end` closes the
        /// innermost open one.
        fn outline_from_source(&self, file: &Path, source: &str) -> Option<Outline> {
            if self.0 != "gamma" && self.0 != "epsilon" {
                return None;
            }
            let mut open: Vec<Symbol> = Vec::new();
            let mut symbols = Vec::new();
            for (index, line) in source.lines().enumerate() {
                let number = index as u32 + 1;
                if let Some(name) = line.trim().strip_prefix("sym ") {
                    let mut segments: Vec<String> =
                        open.iter().map(|symbol| symbol.name.clone()).collect();
                    segments.push(name.to_owned());
                    open.push(Symbol {
                        path: SymbolPath::new(Some(file.to_path_buf()), segments),
                        kind: SymbolKind::Other,
                        name: name.to_owned(),
                        range: LineRange::new(number, number),
                        body: LineRange::new(number, number),
                        signature: line.trim().to_owned(),
                        doc: None,
                        children: Vec::new(),
                    });
                } else if line.trim() == "end"
                    && let Some(mut done) = open.pop()
                {
                    done.range = LineRange::new(done.range.start, number);
                    done.body = done.range;
                    match open.last_mut() {
                        Some(parent) => parent.children.push(done),
                        None => symbols.push(done),
                    }
                }
            }
            Some(Outline {
                file: file.to_path_buf(),
                language: self.language(),
                line_count: line_count(source),
                symbols,
            })
        }
        /// No formatter except gamma's stdin `tr`, which turns every comma into a line break so
        /// a test candidate's line count visibly moves.
        fn format_command(&self, _project: &LanguageProject, _file: &Path) -> Option<Vec<String>> {
            None
        }
        /// Gamma formats stdin by splitting on commas; no other test language formats.
        fn format_stdin_command(
            &self,
            _project: &LanguageProject,
            _file: &Path,
        ) -> Option<Vec<String>> {
            (self.0 == "gamma")
                .then(|| vec!["/usr/bin/tr".to_owned(), ",".to_owned(), "\n".to_owned()])
        }
    }

    /// Project-check stub: present when the root marker file exists.
    struct Checks(&'static str);

    impl crate::checks::LanguageChecks for Checks {
        /// Present iff `<worktree>/<marker>` exists.
        fn is_present(&self, worktree: &Path) -> bool {
            worktree.join(self.0).exists()
        }
        /// The marker doubles as the tool name.
        fn tool_name(&self) -> &'static str {
            self.0
        }
        /// Test languages have no launcher section.
        fn parse_config(
            &self,
            _section: serde_json::Value,
        ) -> Result<std::sync::Arc<dyn crate::checks::CheckConfig>, serde_json::Error> {
            Err(serde::de::Error::custom("test language"))
        }
    }

    use super::names::{
        Certainty, FactSink, FileVerdict, NameFact, NameFacts, NameKey, Namespace,
        NamespaceCoverage, Role, ns,
    };

    /// Name-fact stub whose whitespace-separated tokens are facts: alpha defines class names
    /// (`@x`, `@x/domain`) and element ids (`#x`); beta uses class names exactly (`use:x`,
    /// `use:x/domain`) and heuristically (`~x`); gamma uses element ids (`#x`). A file whose first
    /// line is `#!skip` is skipped as generated.
    struct Names(&'static str);

    /// Coverage of the alpha stub.
    const ALPHA_COVERAGE: &[NamespaceCoverage] = &[
        NamespaceCoverage {
            namespace: ns::CLASS,
            defines: true,
            uses: false,
        },
        NamespaceCoverage {
            namespace: ns::ELEMENT_ID,
            defines: true,
            uses: false,
        },
    ];
    /// Coverage of the beta stub.
    const BETA_COVERAGE: &[NamespaceCoverage] = &[NamespaceCoverage {
        namespace: ns::CLASS,
        defines: false,
        uses: true,
    }];
    /// Coverage of the gamma stub.
    const GAMMA_COVERAGE: &[NamespaceCoverage] = &[NamespaceCoverage {
        namespace: ns::ELEMENT_ID,
        defines: false,
        uses: true,
    }];

    impl Names {
        /// Reads one token as `(namespace, role, certainty, name/domain)` per the stub's syntax.
        fn token<'a>(&self, token: &'a str) -> Option<(Namespace, Role, Certainty, &'a str)> {
            match self.0 {
                "alpha" => token
                    .strip_prefix('@')
                    .map(|rest| (ns::CLASS, Role::Define, Certainty::Exact, rest))
                    .or_else(|| {
                        token
                            .strip_prefix('#')
                            .map(|rest| (ns::ELEMENT_ID, Role::Define, Certainty::Exact, rest))
                    }),
                "beta" => token
                    .strip_prefix("use:")
                    .map(|rest| (ns::CLASS, Role::Use, Certainty::Exact, rest))
                    .or_else(|| {
                        token
                            .strip_prefix('~')
                            .map(|rest| (ns::CLASS, Role::Use, Certainty::Heuristic("tilde"), rest))
                    }),
                _ => token
                    .strip_prefix('#')
                    .map(|rest| (ns::ELEMENT_ID, Role::Use, Certainty::Exact, rest)),
            }
        }
    }

    impl NameFacts for Names {
        /// The stub's fixed coverage.
        fn coverage(&self) -> &'static [NamespaceCoverage] {
            match self.0 {
                "alpha" => ALPHA_COVERAGE,
                "beta" => BETA_COVERAGE,
                _ => GAMMA_COVERAGE,
            }
        }
        /// One fact per recognized token, positioned at the token's first byte.
        fn extract(&self, _file: &Path, source: &str, sink: &mut FactSink) -> FileVerdict {
            if source.starts_with("#!skip") {
                return FileVerdict::Skipped("generated");
            }
            for (index, line) in source.lines().enumerate() {
                let mut at = 0;
                for token in line.split(' ') {
                    let column = at + 1;
                    at += token.len() + 1;
                    let Some((namespace, role, certainty, rest)) = self.token(token) else {
                        continue;
                    };
                    let (name, domain) = rest.split_once('/').unwrap_or((rest, ""));
                    let fact = NameFact {
                        key: NameKey {
                            namespace,
                            domain: domain.into(),
                            name: name.into(),
                        },
                        role,
                        line: index as u32 + 1,
                        column: column as u32,
                        certainty,
                    };
                    if !sink.push(fact) {
                        return FileVerdict::Indexed;
                    }
                }
            }
            FileVerdict::Indexed
        }
    }

    /// Descriptor of the first checked test language.
    static ALPHA_DESCRIPTOR: LanguageDescriptor = LanguageDescriptor {
        id: "alpha",
        display_name: "Alpha",
        extensions: &["alpha"],
        card_manifest: Some("alpha.toml"),
        home_tool_dirs: &[],
        support: &Support("alpha"),
        checks: Some(&Checks("alpha.toml")),
        server: None,
        names: Some(&Names("alpha")),
    };
    /// Descriptor of the second checked test language.
    static BETA_DESCRIPTOR: LanguageDescriptor = LanguageDescriptor {
        id: "beta",
        display_name: "Beta",
        extensions: &["beta"],
        card_manifest: None,
        home_tool_dirs: &[],
        support: &Support("beta"),
        checks: Some(&Checks("beta.toml")),
        server: None,
        names: Some(&Names("beta")),
    };
    /// Descriptor of the third checked test language.
    static GAMMA_DESCRIPTOR: LanguageDescriptor = LanguageDescriptor {
        id: "gamma",
        display_name: "Gamma",
        extensions: &["gamma"],
        card_manifest: Some("gamma.json"),
        home_tool_dirs: &[],
        support: &Support("gamma"),
        checks: Some(&Checks("gamma.toml")),
        server: None,
        names: Some(&Names("gamma")),
    };
    /// Descriptor of the unchecked test language.
    static DELTA_DESCRIPTOR: LanguageDescriptor = LanguageDescriptor {
        id: "delta",
        display_name: "Delta",
        extensions: &["delta"],
        card_manifest: None,
        home_tool_dirs: &[],
        support: &Support("delta"),
        checks: None,
        server: None,
        names: None,
    };

    /// Descriptor of the test language that alone has a language server: the fixture server below.
    static EPSILON_DESCRIPTOR: LanguageDescriptor = LanguageDescriptor {
        id: "epsilon",
        display_name: "Epsilon",
        extensions: &["epsilon"],
        card_manifest: None,
        home_tool_dirs: &[],
        support: &Support("epsilon"),
        checks: None,
        server: Some(&FixtureServer),
        names: None,
    };

    /// The events every fixture backend recorded, as `<event>:<binding tag>[:<detail>]` lines in
    /// order. Tests filter by the binding tags of their own actors, so concurrent tests that use
    /// distinct actor names never see each other's lines.
    static FIXTURE_LOG: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    /// Short stable tag of one binding in [`FIXTURE_LOG`] lines: its fingerprint's first four
    /// bytes in hex.
    pub(crate) fn fixture_tag(binding: &crate::assistance::host_binding::BindingRef) -> String {
        binding.fingerprint()[..4]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Returns the [`FIXTURE_LOG`] lines whose binding tag is one of `bindings`, in order.
    pub(crate) fn fixture_events(
        bindings: &[&crate::assistance::host_binding::BindingRef],
    ) -> Vec<String> {
        let tags: Vec<String> = bindings
            .iter()
            .map(|binding| fixture_tag(binding))
            .collect();
        FIXTURE_LOG
            .lock()
            .expect("fixture log")
            .iter()
            .filter(|line| tags.iter().any(|tag| line.split(':').nth(1) == Some(tag)))
            .cloned()
            .collect()
    }

    /// Binding tags whose next `close_binding` on the fixture server fails once, so a test can
    /// make a reader owner's cleanup fail during a writer's handover.
    static FIXTURE_CLOSE_FAILURES: std::sync::Mutex<Vec<String>> =
        std::sync::Mutex::new(Vec::new());

    /// Makes the next fixture-server `close_binding` of `binding` fail with `Internal`, once.
    pub(crate) fn fixture_fail_next_close(binding: &crate::assistance::host_binding::BindingRef) {
        FIXTURE_CLOSE_FAILURES
            .lock()
            .expect("fixture close failures")
            .push(fixture_tag(binding));
    }

    /// Armed one-shot `ensure_live` faults of the fixture server: the binding tag and whether the
    /// fault is a namespace lookup as a binding that owns none (`true`) or a bare refusal (`false`).
    static FIXTURE_ENSURE_FAULTS: std::sync::Mutex<Vec<(String, bool)>> =
        std::sync::Mutex::new(Vec::new());

    /// Makes the next fixture-server `ensure_live` of `binding` fail with a bare
    /// `ProviderUnavailable` the way a real backend does, once: directly when `unowned` is false,
    /// or by resolving the cache namespace of a binding that owns none when it is true.
    pub(crate) fn fixture_fail_next_ensure(
        binding: &crate::assistance::host_binding::BindingRef,
        unowned: bool,
    ) {
        FIXTURE_ENSURE_FAULTS
            .lock()
            .expect("fixture ensure faults")
            .push((fixture_tag(binding), unowned));
    }

    /// Tags of the bindings whose fixture `ensure_live` opens a scripted language-server session
    /// (and so answers `Ok` instead of `ProviderLoading`).
    static FIXTURE_SESSIONS: std::sync::Mutex<Vec<(String, &'static [&'static str])>> =
        std::sync::Mutex::new(Vec::new());

    /// Makes the fixture server open a scripted session for `binding` from now on: it answers
    /// document symbols (one function, `a`), an empty reference list and an empty workspace-symbol
    /// list, and fails every call-hierarchy request with an error reply, so tests can drive the
    /// worker's real exchange-failure paths. `failing` additionally names requests
    /// (`"documentSymbol"`, `"references"`, `"workspaceSymbol"`) that fail with an error reply.
    ///
    /// The latest call for a binding wins, but only for the next session opened for it: a session
    /// that already exists keeps its script until it is released.
    pub(crate) fn fixture_serve_session(
        binding: &crate::assistance::host_binding::BindingRef,
        failing: &'static [&'static str],
    ) {
        FIXTURE_SESSIONS
            .lock()
            .expect("fixture sessions")
            .push((fixture_tag(binding), failing));
    }

    /// Profile of the scripted fixture session: accepts any server, opens `.epsilon` as `epsilon`.
    #[derive(Debug)]
    struct FixtureProfile;

    impl crate::intelligence::session::SessionProfile for FixtureProfile {
        /// An empty configuration object.
        fn workspace_configuration(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        /// Any server identity is accepted.
        fn accepts_server(&self, _info: Option<&async_lsp::lsp_types::ServerInfo>) -> bool {
            true
        }
        /// Every file opens as `epsilon`.
        fn language_id(&self, _path: &std::path::Path) -> &'static str {
            "epsilon"
        }
    }

    /// The document symbols the scripted session answers for the fixture source `sym a\nend\n`:
    /// one symbol `a` over both lines, exactly what epsilon's source outline reads from that text.
    #[allow(deprecated)]
    pub(crate) fn fixture_document_symbols() -> Vec<async_lsp::lsp_types::DocumentSymbol> {
        use async_lsp::lsp_types as lsp;
        vec![lsp::DocumentSymbol {
            name: "a".into(),
            detail: None,
            kind: lsp::SymbolKind::FUNCTION,
            tags: None,
            deprecated: None,
            range: lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(1, 3)),
            selection_range: lsp::Range::new(lsp::Position::new(0, 4), lsp::Position::new(0, 5)),
            children: None,
        }]
    }

    /// Opens a live session against a scripted in-process language server (see
    /// [`fixture_serve_session`]).
    ///
    /// `source` supplies the worktree and authority epoch the session is opened for; `failing`
    /// names the extra requests the script answers with an error reply. The in-process server runs
    /// as a spawned task that ends with the connection, owned by the returned session. Errors are
    /// those of opening the client and its initialize handshake.
    async fn fixture_open_session(
        source: &crate::workspace::observation::SourceObservation,
        failing: &'static [&'static str],
    ) -> std::io::Result<crate::intelligence::session::LiveSession> {
        use async_lsp::{MainLoop, lsp_types as lsp, lsp_types::request, router::Router};
        use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
        let (client, peer) = tokio::io::duplex(65536);
        let (input, output) = tokio::io::split(client);
        let (peer_input, peer_output) = tokio::io::split(peer);
        let (server, _) = MainLoop::new_server(|client| {
            let mut router = Router::new(client);
            router.request::<request::Initialize, _>(|_, _| async {
                Ok(lsp::InitializeResult {
                    capabilities: lsp::ServerCapabilities {
                        text_document_sync: Some(lsp::TextDocumentSyncCapability::Kind(
                            lsp::TextDocumentSyncKind::FULL,
                        )),
                        document_symbol_provider: Some(lsp::OneOf::Left(true)),
                        references_provider: Some(lsp::OneOf::Left(true)),
                        hover_provider: Some(lsp::HoverProviderCapability::Simple(true)),
                        call_hierarchy_provider: Some(lsp::CallHierarchyServerCapability::Simple(
                            true,
                        )),
                        workspace_symbol_provider: Some(lsp::OneOf::Left(true)),
                        ..Default::default()
                    },
                    server_info: None,
                })
            });
            router.request::<request::DocumentSymbolRequest, _>(move |_, _| async move {
                if failing.contains(&"documentSymbol") {
                    return Err(async_lsp::ResponseError::new(
                        async_lsp::ErrorCode::INTERNAL_ERROR,
                        "scripted documentSymbol failure",
                    ));
                }
                Ok(Some(lsp::DocumentSymbolResponse::Nested(
                    fixture_document_symbols(),
                )))
            });
            router.request::<request::References, _>(move |_, _| async move {
                if failing.contains(&"references") {
                    return Err(async_lsp::ResponseError::new(
                        async_lsp::ErrorCode::INTERNAL_ERROR,
                        "scripted references failure",
                    ));
                }
                Ok(Some(Vec::new()))
            });
            router.request::<request::WorkspaceSymbolRequest, _>(move |_, _| async move {
                if failing.contains(&"workspaceSymbol") {
                    return Err(async_lsp::ResponseError::new(
                        async_lsp::ErrorCode::INTERNAL_ERROR,
                        "scripted workspaceSymbol failure",
                    ));
                }
                Ok(Some(lsp::WorkspaceSymbolResponse::Flat(Vec::new())))
            });
            router.request::<request::Shutdown, _>(|_, _| async { Ok(()) });
            router.notification::<lsp::notification::Exit>(|_, _| {
                std::ops::ControlFlow::Break(Ok(()))
            });
            router.unhandled_notification(|_, _| std::ops::ControlFlow::Continue(()));
            router
        });
        tokio::spawn(server.run_buffered(peer_input.compat(), peer_output.compat_write()));
        crate::intelligence::session::LiveSession::open(
            input,
            output,
            source.worktree().clone(),
            source.authority_epoch(),
            crate::intelligence::freshness::ViewGeneration::default(),
            crate::intelligence::session::ProviderSettings::new(FixtureProfile),
            std::time::Duration::from_secs(5),
        )
        .await
    }

    /// Appends one line to [`FIXTURE_LOG`].
    fn fixture_record(line: String) {
        FIXTURE_LOG.lock().expect("fixture log").push(line);
    }

    /// Language server of [`EPSILON`]: it spawns nothing. Its backend resolves the retained cache
    /// namespace exactly like the real backends do before they spawn, records which binding it did
    /// so for, and then reports the server as still loading, so a unit test can observe namespace
    /// ownership and release order through the production worker paths without a real server.
    pub(crate) struct FixtureServer;

    impl crate::intelligence::server::LanguageServer for FixtureServer {
        /// The fixture language.
        fn language(&self) -> Language {
            EPSILON
        }
        /// Launcher `settings` identifier selecting the fixture server.
        fn settings_key(&self) -> &'static str {
            "fixture_epsilon"
        }
        /// The optional companion program a declaration may carry.
        fn option_fields(&self) -> &'static [&'static str] {
            &["companion"]
        }
        /// Decodes the optional companion program.
        fn parse_options(
            &self,
            mut fields: serde_json::Map<String, serde_json::Value>,
        ) -> Result<std::sync::Arc<dyn std::any::Any + Send + Sync>, serde_json::Error> {
            let companion = fields
                .remove("companion")
                .map(serde_json::from_value::<crate::assistance::launcher::AcceptedExecutable>)
                .transpose()?;
            Ok(std::sync::Arc::new(companion))
        }
        /// Any declaration is acceptable.
        fn validate_launch(&self, _launch: &crate::assistance::launcher::ProviderLaunch) -> bool {
            true
        }
        /// The declared companion program, as a language's toolchain executables would be.
        fn launch_executables<'a>(
            &self,
            launch: &'a crate::assistance::launcher::ProviderLaunch,
        ) -> Vec<&'a crate::assistance::launcher::AcceptedExecutable> {
            launch
                .options::<Option<crate::assistance::launcher::AcceptedExecutable>>()
                .and_then(Option::as_ref)
                .into_iter()
                .collect()
        }
        /// Reply name of the fixture server.
        fn name(&self) -> &'static str {
            "fixtureserver"
        }
        /// Fixed cache settings identity.
        fn cache_settings(&self) -> &'static str {
            "epsilon-settings"
        }
        /// Fixed configuration identity.
        fn effective_configuration(&self) -> &'static str {
            "epsilon-configuration"
        }
        /// One private subdirectory.
        fn cache_directories(&self) -> &'static [&'static str] {
            &["epsilon-state"]
        }
        /// Context requests for `.epsilon` files.
        fn context_extensions(&self) -> &'static [&'static str] {
            &["epsilon"]
        }
        /// Symbol tools for `.epsilon` files use the fixture session.
        fn session_extensions(&self) -> &'static [&'static str] {
            &["epsilon"]
        }
        /// The one project input file of the fixture language, `epsilon.cfg`.
        fn project_inputs(&self) -> &'static [&'static str] {
            &["epsilon.cfg"]
        }
        /// A fresh recording backend.
        fn new_backend(&self) -> Box<dyn crate::intelligence::server::ServerBackend> {
            Box::new(FixtureBackend::default())
        }
    }

    /// Per-worker state of [`FixtureServer`]: the bindings that currently hold a (pretend)
    /// session.
    #[derive(Default)]
    struct FixtureBackend {
        /// Bindings whose session `ensure_live` established and `release_live` has not yet ended.
        live: std::collections::BTreeSet<crate::assistance::host_binding::BindingRef>,
        /// Scripted sessions opened for bindings armed by [`fixture_serve_session`].
        sessions: std::collections::BTreeMap<
            crate::assistance::host_binding::BindingRef,
            crate::intelligence::session::LiveSession,
        >,
    }

    impl crate::intelligence::server::ServerBackend for FixtureBackend {
        /// Ignores its inputs and always answers `ProviderUnavailable`: no test asks the fixture for
        /// semantic context, only for sessions.
        fn context<'a>(
            &'a mut self,
            _host: &'a mut dyn crate::intelligence::server::ProviderHost,
            _job: &'a mut dyn crate::intelligence::server::ProviderJob,
            _launch: &'a crate::assistance::launcher::ProviderLaunch,
            _source: &'a crate::workspace::observation::SourceObservation,
            _bytes: &'a [u8],
            _query: crate::intelligence::context::ContextQuery,
        ) -> crate::checks::BoxFuture<
            'a,
            Result<
                crate::intelligence::server::ProviderContext,
                crate::assistance::reply::FailureCode,
            >,
        > {
            Box::pin(async { Err(crate::assistance::reply::FailureCode::ProviderUnavailable) })
        }

        /// Resolves the job binding's retained namespace like a real backend, records it, marks
        /// the binding live, and answers `ProviderLoading` (the namespace was obtained; a real
        /// server would now be starting). A binding without a retained namespace gets the real
        /// backends' bare `ProviderUnavailable`, as does a binding armed by
        /// [`fixture_fail_next_ensure`].
        fn ensure_live<'a>(
            &'a mut self,
            host: &'a mut dyn crate::intelligence::server::ProviderHost,
            job: &'a mut dyn crate::intelligence::server::ProviderJob,
            launch: &'a crate::assistance::launcher::ProviderLaunch,
            source: &'a crate::workspace::observation::SourceObservation,
        ) -> crate::checks::BoxFuture<'a, Result<(), crate::assistance::reply::FailureCode>>
        {
            Box::pin(async move {
                let binding = job.binding().clone();
                let authority = host.authority(&binding).await?;
                let fault = {
                    let mut armed = FIXTURE_ENSURE_FAULTS.lock().expect("fixture ensure faults");
                    let tag = fixture_tag(&binding);
                    armed
                        .iter()
                        .position(|(armed, _)| *armed == tag)
                        .map(|position| armed.remove(position).1)
                };
                let lookup = match fault {
                    Some(false) => {
                        return Err(crate::assistance::reply::FailureCode::ProviderUnavailable);
                    }
                    Some(true) => crate::assistance::host_binding::BindingRef::fixture(
                        "fixture-unowned",
                        "fixture-unowned-channel",
                        1,
                    ),
                    None => binding.clone(),
                };
                let namespace = host.cache_namespace(
                    &lookup,
                    &authority,
                    launch,
                    &crate::intelligence::server::effective_trust(launch),
                )?;
                fixture_record(format!("ensure:{}:{namespace}", fixture_tag(&binding)));
                let serving = FIXTURE_SESSIONS
                    .lock()
                    .expect("fixture sessions")
                    .iter()
                    .rev()
                    .find(|(tag, _)| *tag == fixture_tag(&binding))
                    .map(|(_, failing)| *failing);
                self.live.insert(binding.clone());
                let Some(failing) = serving else {
                    return Err(crate::assistance::reply::FailureCode::ProviderLoading);
                };
                if let std::collections::btree_map::Entry::Vacant(slot) =
                    self.sessions.entry(binding)
                {
                    let session = fixture_open_session(source, failing)
                        .await
                        .map_err(|_| crate::assistance::reply::FailureCode::ProviderUnavailable)?;
                    slot.insert(session);
                }
                Ok(())
            })
        }

        /// Ends the binding's pretend session and records it, and drops its scripted session (and
        /// with it the in-process server connection) when one was opened.
        fn release_live<'a>(
            &'a mut self,
            _host: &'a mut dyn crate::intelligence::server::ProviderHost,
            binding: &'a crate::assistance::host_binding::BindingRef,
        ) -> crate::checks::BoxFuture<'a, ()> {
            self.sessions.remove(binding);
            if self.live.remove(binding) {
                fixture_record(format!("release:{}", fixture_tag(binding)));
            }
            Box::pin(async {})
        }

        /// Releases no resource, but fails once for a binding armed by [`fixture_fail_next_close`]
        /// and records the failure.
        fn close_binding<'a>(
            &'a mut self,
            _host: &'a mut dyn crate::intelligence::server::ProviderHost,
            binding: &'a crate::assistance::host_binding::BindingRef,
        ) -> crate::checks::BoxFuture<'a, Result<(), crate::assistance::reply::FailureCode>>
        {
            let tag = fixture_tag(binding);
            let mut armed = FIXTURE_CLOSE_FAILURES
                .lock()
                .expect("fixture close failures");
            let failing = armed.iter().position(|armed| *armed == tag);
            if let Some(position) = failing {
                armed.remove(position);
                fixture_record(format!("close-failed:{tag}"));
            }
            Box::pin(async move {
                if failing.is_some() {
                    Err(crate::assistance::reply::FailureCode::Internal)
                } else {
                    Ok(())
                }
            })
        }

        /// The scripted session of `binding`, when one was opened.
        fn live_session(
            &mut self,
            binding: &crate::assistance::host_binding::BindingRef,
        ) -> Option<&mut crate::intelligence::session::LiveSession> {
            self.sessions.get_mut(binding)
        }

        /// The bindings holding a pretend session.
        fn live_bindings(&self) -> Vec<crate::assistance::host_binding::BindingRef> {
            self.live.iter().cloned().collect()
        }
    }

    /// First checked test language.
    pub(crate) const ALPHA: Language = Language::of(&ALPHA_DESCRIPTOR);
    /// Second checked test language.
    pub(crate) const BETA: Language = Language::of(&BETA_DESCRIPTOR);
    /// Third checked test language.
    pub(crate) const GAMMA: Language = Language::of(&GAMMA_DESCRIPTOR);
    /// Unchecked test language.
    pub(crate) const DELTA: Language = Language::of(&DELTA_DESCRIPTOR);
    /// Unchecked test language whose server is the recording [`FixtureServer`].
    pub(crate) const EPSILON: Language = Language::of(&EPSILON_DESCRIPTOR);

    /// Registers the test languages (idempotent) for tests that look languages up by path or id.
    pub(crate) fn install() {
        super::install(&[ALPHA, BETA, GAMMA, DELTA, EPSILON]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_helpers_count_and_slice_inclusively() {
        let source = "a\nb\nc";
        assert_eq!(line_count(source), 3);
        assert_eq!(line_count("a\nb\n"), 2);
        assert_eq!(line_count(""), 0);
        assert_eq!(slice_lines(source, LineRange::new(2, 3)), "b\nc");
        assert_eq!(slice_lines("a\nb\nc\n", LineRange::new(1, 1)), "a\n");
        assert_eq!(LineRange::new(5, 2), LineRange { start: 5, end: 5 });
        assert_eq!(LineRange::new(3, 7).to_string(), "3–7");
    }

    #[test]
    fn lsp_ranges_become_one_based_inclusive_lines() {
        let range = lsp::Range::new(lsp::Position::new(4, 0), lsp::Position::new(9, 1));
        assert_eq!(lines_of(&range), LineRange::new(5, 10));
        let to_line_start = lsp::Range::new(lsp::Position::new(4, 0), lsp::Position::new(9, 0));
        assert_eq!(lines_of(&to_line_start), LineRange::new(5, 9));
    }

    #[test]
    fn language_is_selected_by_extension() {
        testing::install();
        assert_eq!(
            Language::for_path(Path::new("a/b.alpha")),
            Some(testing::ALPHA)
        );
        assert_eq!(
            Language::for_path(Path::new("x.gamma")),
            Some(testing::GAMMA)
        );
        assert_eq!(
            Language::for_path(Path::new("x.delta")),
            Some(testing::DELTA)
        );
        assert_eq!(Language::for_path(Path::new("README.md")), None);
        assert_eq!(Language::by_id("beta"), Some(testing::BETA));
        assert!(testing::ALPHA < testing::BETA && testing::BETA < testing::GAMMA);
    }

    #[test]
    fn a_method_wins_over_a_same_named_field_of_its_type() {
        let symbol = |path: &[&str], kind, line, children| Symbol {
            path: SymbolPath::new(None, path.iter().map(|s| (*s).to_owned()).collect()),
            kind,
            name: (*path.last().unwrap()).to_owned(),
            range: LineRange::new(line, line),
            body: LineRange::new(line, line),
            signature: String::new(),
            doc: None,
            children,
        };
        let outline = Outline {
            file: PathBuf::from("a.alpha"),
            language: testing::ALPHA,
            line_count: 9,
            symbols: vec![
                symbol(
                    &["Foo"],
                    SymbolKind::Struct,
                    1,
                    vec![
                        symbol(&["Foo", "source"], SymbolKind::Field, 2, vec![]),
                        symbol(&["Foo", "only_field"], SymbolKind::Field, 3, vec![]),
                    ],
                ),
                symbol(
                    &["Foo"],
                    SymbolKind::Impl,
                    5,
                    vec![symbol(&["Foo", "source"], SymbolKind::Method, 6, vec![])],
                ),
            ],
        };
        let find = |text: &str| {
            outline
                .find(&SymbolPath::parse(text).unwrap())
                .map(|s| (s.kind, s.range.start))
        };
        assert_eq!(find("Foo/source"), Some((SymbolKind::Method, 6)));
        assert_eq!(find("Foo/only_field"), Some((SymbolKind::Field, 3)));
        assert_eq!(find("Foo"), Some((SymbolKind::Struct, 1)));
        assert_eq!(find("Foo/missing"), None);
    }
}
