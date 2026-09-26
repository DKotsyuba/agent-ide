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

pub mod go;
pub mod path;
pub mod python;
pub mod render;
pub mod rust;
pub mod typescript;

pub use path::SymbolPath;

/// Languages with a support module; the order is the order `ide.start` reports them.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum Language {
    Rust,
    Python,
    TypeScript,
    Go,
}

impl Language {
    /// Stable lowercase name used in replies and journal lines.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::TypeScript => "typescript",
            Self::Go => "go",
        }
    }

    /// Selects the language by file extension; `None` for files no support module owns.
    pub fn for_path(path: &Path) -> Option<Self> {
        match path.extension().and_then(|value| value.to_str())? {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "ts" | "tsx" | "js" | "jsx" | "mts" | "cts" | "mjs" | "cjs" => Some(Self::TypeScript),
            "go" => Some(Self::Go),
            _ => None,
        }
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Closed symbol kinds shared by every language; language-specific kinds map onto the nearest one.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
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
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
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
#[derive(Clone, Debug, PartialEq)]
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
#[derive(Clone, Debug, PartialEq)]
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
    /// Same-named siblings are tried in source order and the walk backtracks: in Rust the struct
    /// `Foo` and its `impl Foo` blocks share the segment `Foo`, so `Foo` finds the struct while
    /// `Foo/new` finds the method inside whichever impl declares it.
    pub fn find(&self, path: &SymbolPath) -> Option<&Symbol> {
        fn descend<'a>(level: &'a [Symbol], segments: &[String]) -> Option<&'a Symbol> {
            let (first, rest) = segments.split_first()?;
            level
                .iter()
                .filter(|symbol| symbol.name == *first)
                .find_map(|symbol| {
                    if rest.is_empty() {
                        Some(symbol)
                    } else {
                        descend(&symbol.children, rest)
                    }
                })
        }
        descend(&self.symbols, path.segments())
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
#[derive(Clone, Debug, Eq, PartialEq)]
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
#[derive(Clone, Debug, PartialEq)]
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
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TestId {
    pub file: PathBuf,
    /// Runner-specific identifier: `assistance::worker::tests::stop_cancels_queue`,
    /// `tests/test_candles.py::test_empty_day`, `Class > method`.
    pub name: String,
}

/// Command that runs a selection, plus the identifiers it is expected to cover.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestSelection {
    pub tests: Vec<TestId>,
    /// argv, program first; run from the project root.
    pub command: Vec<String>,
}

/// One failed test as the runner reported it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestFailure {
    pub name: String,
    /// `file:line` when the runner names one.
    pub location: Option<(PathBuf, u32)>,
    /// First line of the assertion or panic message.
    pub message: String,
}

/// Parsed runner output; counts are what the runner printed, never inferred from silence.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TestReport {
    pub passed: u32,
    pub failed: u32,
    pub ignored: u32,
    pub failures: Vec<TestFailure>,
    /// The runner stopped before printing a summary (budget, crash, signal).
    pub incomplete: bool,
}

/// Where a project command came from; replies print it so the agent knows how far to trust it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandSource {
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
            Self::Ci => "ci",
            Self::Makefile => "makefile",
            Self::Manifest => "manifest",
            Self::Readme => "readme",
            Self::Default => "default",
        }
    }
}

/// One project command with its provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectCommand {
    pub argv: Vec<String>,
    pub source: CommandSource,
}

/// The commands a language module knows for a project; `None` means the project has none.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProjectCommands {
    pub build: Option<ProjectCommand>,
    pub check: Option<ProjectCommand>,
    pub test: Option<ProjectCommand>,
    pub lint: Option<ProjectCommand>,
    pub format: Option<ProjectCommand>,
    pub typecheck: Option<ProjectCommand>,
}

/// One language's view of a project root.
#[derive(Clone, Debug, PartialEq)]
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

/// Failures a language module reports; the reply layer maps them onto closed failure codes.
#[derive(Clone, Debug, Eq, PartialEq)]
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
/// its inputs except `detect`, which reads manifests under `root`.
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
}

/// The support module for a language, or `None` while it is not implemented.
pub fn support(language: Language) -> Option<&'static dyn LanguageSupport> {
    match language {
        Language::Rust => Some(&rust::RustSupport),
        Language::Go => Some(&go::GoSupport),
        Language::Python => Some(&python::Python),
        Language::TypeScript => Some(&typescript::TypeScript),
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
        assert_eq!(
            Language::for_path(Path::new("a/b.rs")),
            Some(Language::Rust)
        );
        assert_eq!(
            Language::for_path(Path::new("x.tsx")),
            Some(Language::TypeScript)
        );
        assert_eq!(
            Language::for_path(Path::new("x.pyi")),
            Some(Language::Python)
        );
        assert_eq!(Language::for_path(Path::new("main.go")), Some(Language::Go));
        assert_eq!(Language::for_path(Path::new("README.md")), None);
    }
}
