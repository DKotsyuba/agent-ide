//! Typed payloads of the `bundled-module/0` capabilities.
//!
//! A [`super::contract::Request`] carries one of the request types below as its `payload` and the
//! matching [`super::contract::Response`] carries the documented answer as its `result`. Payloads
//! describe product facts in the core's own types ([`Outline`], [`TestReport`](crate::lang::TestReport), [`SyntaxVerdict`],
//! [`ProblemSnapshot`](crate::checks::ProblemSnapshot), ...), never serialized provider protocol objects. Locations are
//! scope-relative paths with zero-based UTF-8 byte ranges; the core validates every range against
//! its own observation before using it. Modules propose and interpret; the core reads, writes and
//! runs.

use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::contract::Cause;
use crate::{
    checks::CheckRequest,
    lang::{
        InsertSite, InsertWhere, LangError, LanguageProject, Outline, ProbePrograms, SymbolKind,
        SymbolPath, SyntaxVerdict, TestId, TestSelection, TestTarget,
        environment::{CommandEnv, EnvSelection, ResolvedEnv},
    },
};

/// Largest source text sent inline; larger sources travel as an attachment.
pub const MAX_INLINE_SOURCE: usize = 64 * 1024;
/// Largest number of anchors one file may emit (the existing per-file fact limit).
pub const MAX_ANCHORS_PER_FILE: usize = crate::lang::names::MAX_FACTS_PER_FILE;
/// Largest number of candidates one `linkage.resolve` may return.
pub const MAX_RESOLVE_CANDIDATES: usize = 32;
/// Largest encoded key of a file reference anchor.
pub const MAX_FILE_REF_KEY: usize = 4096;

/// Exact source the core observed, with its core-minted revision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRef {
    /// Scope-relative path.
    pub path: PathBuf,
    /// Opaque core revision of these bytes; echoed in locations inside this file.
    pub revision: String,
    /// The text, inline or as an attachment of the same message.
    pub text: SourceText,
}

/// Where a [`SourceRef`]'s text is.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceText {
    /// At most [`MAX_INLINE_SOURCE`] bytes of UTF-8.
    Inline(String),
    /// The id of a `text/plain; charset=utf-8` attachment.
    Attachment(u32),
    /// The path was observed missing.
    Missing,
}

/// One located range.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Location {
    /// Scope-relative path.
    pub path: PathBuf,
    /// Zero-based UTF-8 byte offset of the first byte.
    pub start_byte: u64,
    /// Zero-based UTF-8 byte offset past the last byte.
    pub end_byte: u64,
    /// The request source's revision when the range is inside it; `None` for a file the request
    /// did not carry, which the core observes itself before trusting the range.
    pub revision: Option<String>,
}

/// One field of a batched answer: computed, refused as today, or waiting for the provider.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field<T> {
    /// Computed.
    Available(T),
    /// The language does not compute it (the in-process answer is the same refusal).
    Unsupported,
    /// The provider is not ready; retry later.
    Warming,
    /// Not asked for.
    NotRequested,
}

// ---- project ----

/// `project`: language interpretation of a project or its environments. Paths are absolute here:
/// the module reads manifests and environment markers itself (trusted host reads, §2.6).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProjectQuery {
    /// `LanguageSupport::detect`; answers `Option<LanguageProject>`.
    Detect {
        /// Project root.
        root: PathBuf,
    },
    /// `LanguageSupport::environments` honouring the core's stored `selections`; answers
    /// `Vec<ResolvedEnv>`.
    Environments {
        /// Worktree.
        worktree: PathBuf,
        /// The core's stored selections for this worktree and language.
        selections: Vec<EnvSelection>,
    },
    /// `LanguageSupport::check_selection`; answers `Result<(), String>`.
    CheckSelection {
        /// Worktree.
        worktree: PathBuf,
        /// Worktree-relative project root.
        root: PathBuf,
        /// The proposed selector.
        selector: String,
    },
    /// `LanguageSupport::command_env` with the core's stored `selections`; answers
    /// `Option<CommandEnv>`.
    CommandEnv {
        /// Worktree.
        worktree: PathBuf,
        /// Command working directory.
        cwd: PathBuf,
        /// Program name.
        program: String,
        /// The core's stored selections for this worktree and language.
        selections: Vec<EnvSelection>,
    },
    /// `LanguageSupport::test_toolchain`; answers `Option<(PathBuf, PathBuf)>`.
    TestToolchain {
        /// Program name.
        program: String,
    },
}

/// Answer types of [`ProjectQuery`], for documentation and typed decoding.
pub type DetectAnswer = Option<LanguageProject>;
/// Answer of [`ProjectQuery::Environments`].
pub type EnvironmentsAnswer = Vec<ResolvedEnv>;
/// Answer of [`ProjectQuery::CheckSelection`].
pub type CheckSelectionAnswer = Result<(), String>;
/// Answer of [`ProjectQuery::CommandEnv`].
pub type CommandEnvAnswer = Option<CommandEnv>;
/// Answer of [`ProjectQuery::TestToolchain`].
pub type TestToolchainAnswer = Option<(PathBuf, PathBuf)>;

// ---- analyze_source, outline, file_doc, insert_site, syntax ----

/// Fields of one [`AnalyzeSource`] batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceField {
    /// `outline_from_source`.
    Outline,
    /// `file_doc`.
    FileDoc,
    /// In-process `syntax_verdict`.
    Syntax,
    /// `is_test_file` and `test_binary`.
    Tests,
    /// Linkage anchors.
    Anchors,
}

/// `analyze_source`: the batched per-file computation; answers [`SourceAnalysis`]. The core caches
/// the answer by exact source revision and module build and re-validates it per call.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeSource {
    /// The file.
    pub source: SourceRef,
    /// Requested fields; the rest answer [`Field::NotRequested`].
    pub fields: Vec<SourceField>,
}

/// Answer of [`AnalyzeSource`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceAnalysis {
    /// Source outline; `Available(None)` means the language keeps outlines provider-backed.
    pub outline: Field<Option<Outline>>,
    /// First documentation line.
    pub file_doc: Field<Option<String>>,
    /// Structural verdict of the exact text.
    pub syntax: Field<SyntaxVerdict>,
    /// Test classification.
    pub tests: Field<TestFacts>,
    /// Linkage anchors.
    pub anchors: Field<AnchorBatch>,
}

/// Test classification of one file.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestFacts {
    /// `is_test_file`.
    pub is_test_file: bool,
    /// `test_binary`.
    pub test_binary: Option<String>,
}

/// `outline`: provider document symbols normalized into an [`Outline`] for this source; answers
/// `Outline` (or a `warming` error while the provider loads).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutlineRequest {
    /// The file.
    pub source: SourceRef,
}

/// `file_doc`: answers `Option<String>`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileDocRequest {
    /// The file.
    pub source: SourceRef,
}

/// `insert_site`: answers `Result<InsertSite, LangError>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InsertSiteRequest {
    /// The file.
    pub source: SourceRef,
    /// The outline the core addresses the anchor in.
    pub outline: Outline,
    /// The anchor.
    pub anchor: SymbolPath,
    /// Where relative to it.
    pub placement: InsertWhere,
}

/// Answer of [`InsertSiteRequest`].
pub type InsertSiteAnswer = Result<InsertSite, LangError>;

/// `syntax`: a verdict on exact text, or the stdin probe command the core may run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum SyntaxQuery {
    /// `syntax_verdict`; answers [`SyntaxVerdict`].
    Verdict {
        /// The candidate text.
        source: SourceRef,
    },
    /// `syntax_probe_command` as a stdin probe recipe; answers `Option<EffectRequest>` (the core
    /// feeds the candidate, [`Stdin::Candidate`]).
    ProbePlan {
        /// The project.
        project: Box<LanguageProject>,
        /// Absolute project root.
        root: PathBuf,
        /// Root-relative file.
        file: PathBuf,
        /// Externally configured probe programs.
        configured: Option<ProbePrograms>,
    },
}

// ---- semantic and calls ----

/// `semantic`: provider evidence for one source.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum SemanticQuery {
    /// Context evidence and diagnostics; answers [`ContextEvidence`].
    Context {
        /// The file.
        source: SourceRef,
        /// Byte offset of the symbol, `None` for the whole file.
        byte_offset: Option<u64>,
    },
    /// Hover; answers `Option<Hover>`.
    Hover {
        /// The file.
        source: SourceRef,
        /// Byte offset.
        byte_offset: u64,
    },
    /// Definitions; answers `Option<Vec<Location>>` (`None`: not advertised).
    Definitions {
        /// The file.
        source: SourceRef,
        /// Byte offset.
        byte_offset: u64,
    },
    /// References; answers `Option<Vec<Location>>`.
    References {
        /// The file.
        source: SourceRef,
        /// Byte offset.
        byte_offset: u64,
    },
    /// Workspace symbols; answers `Option<Vec<WorkspaceSymbol>>`.
    WorkspaceSymbols {
        /// Query text.
        query: String,
    },
    /// Current diagnostics; answers [`DiagnosticsEvidence`].
    Diagnostics {
        /// The file.
        source: SourceRef,
    },
    /// The hosted provider's status barrier: waits within the request's budget for the
    /// provider's own readiness report (a provider without one is ready after its handshake) and
    /// answers [`Readiness`](super::contract::Readiness): `ready`, `warming` (still loading at the
    /// deadline) or `degraded` (it reported that the workspace failed to load).
    Readiness {},
}

/// Provider hover.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hover {
    /// Markdown or plain text as the provider sent it.
    pub contents: String,
    /// Range the hover covers, if reported.
    pub range: Option<Location>,
}

/// One workspace symbol.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSymbol {
    /// Name.
    pub name: String,
    /// Closed kind.
    pub kind: SymbolKind,
    /// Container name, if reported.
    pub container: Option<String>,
    /// Where.
    pub location: Location,
}

/// Provider context evidence; source text and identity stay core-derived.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextEvidence {
    /// `None` for semantic evidence, the lexical reason otherwise.
    pub lexical: Option<String>,
    /// Synchronized document version.
    pub document_version: Option<i32>,
    /// The provider's negotiated position encoding (`utf-8`, `utf-16` or `utf-32`): the core
    /// reports it and converts byte locations into its positions.
    pub position_encoding: String,
    /// `None`: unsupported; `Some([])`: an empty reply.
    pub definitions: Option<Vec<Location>>,
    /// `None`: unsupported; `Some([])`: an empty reply.
    pub references: Option<Vec<Location>>,
    /// A location ceiling omitted data.
    pub truncated: bool,
    /// Diagnostics of the same source.
    pub diagnostics: DiagnosticsEvidence,
}

/// Diagnostics bound to one source revision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsEvidence {
    /// The revision the provider's push was bound to; anything but the request's stays unknown.
    pub revision: Option<String>,
    /// The provider document version the push named, when bound to that revision; `None` for an
    /// unversioned push (its count is then a lower bound).
    pub document_version: Option<i32>,
    /// `clean`, `reported` or `unknown`.
    pub readiness: String,
    /// `current`, `stale`, `provisional` or `unknown`.
    pub freshness: String,
    /// At most 128 diagnostics.
    pub diagnostics: Vec<Diagnostic>,
    /// The count ceiling omitted items.
    pub truncated: bool,
}

/// One provider diagnostic.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    /// Where.
    pub location: Location,
    /// `error`, `warning`, `information` or `hint`.
    pub severity: Option<String>,
    /// Provider code.
    pub code: Option<String>,
    /// Provider source label.
    pub source: Option<String>,
    /// Message.
    pub message: String,
}

/// `calls`: call hierarchy; a language without it declares the capability unsupported.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum CallsQuery {
    /// Answers `Option<Vec<CallItem>>`.
    Prepare {
        /// The file.
        source: SourceRef,
        /// Byte offset.
        byte_offset: u64,
    },
    /// Answers `Option<Vec<Call>>`.
    Incoming {
        /// The item, from an earlier answer of this instance.
        item: CallItem,
    },
    /// Answers `Option<Vec<Call>>`.
    Outgoing {
        /// The item, from an earlier answer of this instance.
        item: CallItem,
    },
}

/// One call hierarchy item.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallItem {
    /// Name.
    pub name: String,
    /// Closed kind.
    pub kind: SymbolKind,
    /// Provider detail.
    pub detail: Option<String>,
    /// Whole declaration.
    pub location: Location,
    /// The name's range.
    pub selection: Location,
    /// Instance-local opaque handle the module needs to continue from this item; meaningless to
    /// the core and to any other instance.
    pub handle: Option<String>,
}

/// One incoming or outgoing call.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    /// The other end.
    pub item: CallItem,
    /// Call sites.
    pub ranges: Vec<Location>,
}

// ---- edit_plan.rename and edit proposals ----

/// `edit_plan.rename`: answers [`RenameAnswer`]. The first request carries only the symbol's file;
/// when the rename touches others the module answers [`RenameAnswer::NeedSources`] and the core
/// repeats the request with those files observed, within the original deadline.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameRequest {
    /// The symbol's file.
    pub source: SourceRef,
    /// Byte offset of the symbol.
    pub byte_offset: u64,
    /// New name.
    pub new_name: String,
    /// Every other file the core has observed for this rename.
    pub sources: Vec<SourceRef>,
}

/// Answer of [`RenameRequest`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenameAnswer {
    /// Observe these scope-relative paths and ask again.
    NeedSources(Vec<PathBuf>),
    /// The complete proposal over observed revisions.
    Proposal(EditProposal),
    /// The provider refused (not renameable here, resource operations needed).
    Refused(String),
}

/// A module's proposed text edits; it never writes. The core validates the whole proposal before
/// the first write ([`EditProposal::validate`]) and applies it through its own edit path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditProposal {
    /// One entry per file.
    pub files: Vec<FileEdit>,
}

/// Replacements in one file.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEdit {
    /// Scope-relative path.
    pub path: PathBuf,
    /// The core revision the ranges refer to.
    pub base_revision: String,
    /// Replacements, sorted and non-overlapping.
    pub replacements: Vec<Replacement>,
}

/// One replacement.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replacement {
    /// First replaced byte.
    pub start_byte: u64,
    /// Past the last replaced byte.
    pub end_byte: u64,
    /// New text.
    pub new_text: String,
}

/// Why the core refuses an edit proposal before writing anything.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditRefusal {
    /// The path is absolute, escapes the scope or is not normal.
    OutOfScope(PathBuf),
    /// The same file appears twice.
    DuplicateFile(PathBuf),
    /// The file is not observed, or its revision moved past the base.
    Stale(PathBuf),
    /// A range runs backwards, past the end, or overlaps or precedes its predecessor.
    BadRange(PathBuf),
    /// A range boundary falls inside a UTF-8 sequence.
    NotCharBoundary(PathBuf),
}

impl EditProposal {
    /// Checks the whole proposal against the core's current observations before any write:
    /// `observed(path)` returns the current revision and exact text of a scope-relative path.
    /// Every path must be relative and normal, appear once and be observed at its base revision;
    /// every range must be ordered, in bounds, on UTF-8 boundaries and not overlap the previous.
    pub fn validate<'a>(
        &self,
        observed: impl Fn(&Path) -> Option<(&'a str, &'a str)>,
    ) -> Result<(), EditRefusal> {
        for (index, file) in self.files.iter().enumerate() {
            let path = file.path.clone();
            let normal = !path.as_os_str().is_empty()
                && path
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)));
            if !normal {
                return Err(EditRefusal::OutOfScope(path));
            }
            if self.files[..index].iter().any(|seen| seen.path == path) {
                return Err(EditRefusal::DuplicateFile(path));
            }
            let Some((revision, text)) = observed(&path) else {
                return Err(EditRefusal::Stale(path));
            };
            if revision != file.base_revision {
                return Err(EditRefusal::Stale(path));
            }
            let mut previous_end = 0u64;
            for replacement in &file.replacements {
                let (start, end) = (replacement.start_byte, replacement.end_byte);
                if start > end || end > text.len() as u64 || start < previous_end {
                    return Err(EditRefusal::BadRange(path));
                }
                if !text.is_char_boundary(start as usize) || !text.is_char_boundary(end as usize) {
                    return Err(EditRefusal::NotCharBoundary(path));
                }
                previous_end = end;
            }
        }
        Ok(())
    }
}

// ---- format_plan, test_plan, test_parse ----

/// `format_plan`: answers [`FormatPlanAnswer`], the stdin formatter recipe for one file (today's
/// `format_stdin_command`), or `None` when the project has no formatter. The core feeds its own
/// candidate bytes on stdin ([`Stdin::Candidate`]) and treats stdout as the new candidate; no
/// module writes the file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormatPlanRequest {
    /// The project.
    pub project: LanguageProject,
    /// Root-relative file.
    pub file: PathBuf,
}

/// Answer of [`FormatPlanRequest`].
pub type FormatPlanAnswer = Option<EffectRequest>;

/// `test_plan`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum TestPlanQuery {
    /// `test_selection`; answers `Result<TestSelection, LangError>`.
    Selection {
        /// The project.
        project: Box<LanguageProject>,
        /// The target.
        target: TestTarget,
    },
    /// The run of `test_selection` as a recipe request; answers [`RunAnswer`]. The core runs
    /// module-planned tests only this way (an explicit `ide.test` command keeps its own path).
    Run {
        /// The project.
        project: Box<LanguageProject>,
        /// The target.
        target: TestTarget,
    },
    /// `test_id` for each outline path of `file`; answers `Vec<String>` in the same order.
    TestIds {
        /// Root-relative file.
        file: PathBuf,
        /// Outline paths (segments joined with `/`).
        outline_paths: Vec<String>,
    },
}

/// Answer of [`TestPlanQuery::Selection`].
pub type SelectionAnswer = Result<TestSelection, LangError>;

/// A selected test run as a finite recipe request: the tests it runs and the run of one of the
/// language's declared test recipes, which the core alone admits and expands.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestRun {
    /// The selected tests.
    pub tests: Vec<TestId>,
    /// The recipe request that runs them.
    pub effect: EffectRequest,
}

/// Answer of [`TestPlanQuery::Run`].
pub type RunAnswer = Result<TestRun, LangError>;

/// `test_parse`: runner output as attachments; answers [`TestReport`](crate::lang::TestReport).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestParseRequest {
    /// Attachment id of stdout.
    pub stdout: u32,
    /// Attachment id of stderr.
    pub stderr: u32,
}

// ---- check_plan, check_parse, analysis_scope ----

/// `check_plan`: run one project check. The module plans and interprets; every process it needs
/// is an [`EffectRequest`] the core expands, admits and runs (`Control::Effect`). Answers
/// [`ProblemSnapshot`](crate::checks::ProblemSnapshot).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckPlanRequest {
    /// The core's check invocation.
    pub request: CheckRequest,
    /// The language's `project_checks` launcher section, decoded by the module.
    pub config: Value,
    /// Total wall-clock ceiling of the check.
    pub timeout_ms: u64,
}

/// `check_parse`: interpret one completed run the core performed; stdout and stderr follow as
/// attachments. Answers [`ProblemSnapshot`](crate::checks::ProblemSnapshot).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckParseRequest {
    /// The core's check invocation.
    pub request: CheckRequest,
    /// The run's outcome.
    pub outcome: EffectOutcome,
    /// Attachment id of stdout.
    pub stdout: u32,
    /// Attachment id of stderr.
    pub stderr: u32,
}

/// `analysis_scope`: answers `Option<String>`, the reason the check does not analyse `path`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisScopeRequest {
    /// Absolute worktree.
    pub worktree: PathBuf,
    /// Worktree-relative path.
    pub path: PathBuf,
}

// ---- effects ----

/// One typed recipe parameter; never shell text. The module computes the values; the core admits
/// each one against the rule its recipe declares and alone builds the run specification. No
/// parameter carries a count ceiling of its own: the encoded message body is the only bound, so
/// nothing today's in-process path accepts is refused for its length.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Param {
    /// A path the core canonicalizes and admits under the recipe's [`PathRule`].
    Path(PathBuf),
    /// Paths admitted one by one under the recipe's [`PathRule`] (read roots, ancestor
    /// configuration files).
    Paths(Vec<PathBuf>),
    /// The name of an [`ExecutableSlot`] of the descriptor; the core alone resolves it.
    Executable(String),
    /// A literal argument or environment value.
    Token(String),
    /// Positional argument tokens (a test selection), expanded in order by [`Arg::Each`]: each
    /// non-empty, without NUL and not starting with `-`, so it can never become an option.
    Tokens(Vec<String>),
    /// Environment entries whose names must match the recipe's [`EnvRule::Pattern`].
    Env(BTreeMap<String, String>),
    /// A bounded scalar.
    Scalar(u64),
}

/// A module's request to run one recipe of its descriptor.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectRequest {
    /// Recipe id from the language's static descriptor.
    pub recipe: String,
    /// Named typed parameters.
    pub params: BTreeMap<String, Param>,
}

/// How a run ended, or why the core refused it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum EffectOutcome {
    /// The core ran it; output bytes follow as attachments.
    Completed {
        /// Core-minted effect id; a repeated call returns the same id and outcome.
        effect_id: String,
        /// Exit code, `None` when killed by a signal or the timeout.
        status: Option<i32>,
        /// The timeout expired.
        timed_out: bool,
        /// A stream reached its capture ceiling; the bytes are a prefix.
        truncated: bool,
        /// Bytes the process wrote on stdout (beyond the capture when truncated).
        stdout_bytes: u64,
        /// Bytes the process wrote on stderr.
        stderr_bytes: u64,
    },
    /// The core refused or could not run it.
    Refused {
        /// Typed cause.
        cause: Cause,
        /// Sanitized detail.
        message: String,
    },
}

/// One argument of a recipe template.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Arg {
    /// A fixed token.
    Literal(&'static str),
    /// The value of a named [`Param::Path`] (admitted), [`Param::Token`] or [`Param::Scalar`].
    Param(&'static str),
    /// Every admitted value of a named [`Param::Paths`], or every token of a named
    /// [`Param::Tokens`], in order.
    Each(&'static str),
    /// `--flag=<value>`-style concatenation of a literal prefix and a named parameter.
    Joined(&'static str, &'static str),
}

/// Where an admitted path may be. The core checks the path as given against its own resolution
/// of each root; read denies always win.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathRole {
    /// The instance's canonical worktree itself (`ModuleConfig::worktree`).
    WorktreeRoot,
    /// Inside the canonical worktree.
    Worktree,
    /// A file at one of these relative paths (subdirectories allowed, never `..` or absolute)
    /// below a canonical ancestor of the worktree, the worktree itself excluded.
    AncestorFile(&'static [&'static str]),
    /// Under the real user home.
    HomeRelative,
    /// Under a root the admitted launcher configuration declares (toolchain and tool roots,
    /// accepted executables' installation prefixes).
    LauncherRoot,
    /// The parent of the ancestor component named `stop_at` of a launcher root (for example the
    /// directory holding a `toolchains` component).
    LauncherRootAncestor {
        /// Component name whose parent is admitted.
        stop_at: &'static str,
    },
    /// Exactly one of these absolute paths.
    Fixed(&'static [&'static str]),
    /// Under a developer directory the core resolves itself (its platform tools selection,
    /// standard install locations, or the launcher's override).
    DeveloperDir,
    /// The private cache below the core's grant; the only writable root.
    Cache,
}

/// How one path parameter is admitted and used.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PathRule {
    /// The parameter name.
    pub param: &'static str,
    /// Where it may be: any one of these roles admits it.
    pub roles: &'static [PathRole],
    /// An optional file: read exactly when today's in-process check reads it (a regular file, a
    /// symlink to one included; with any host deny, only a regular file that is not itself a
    /// symlink) and never when a deny matches it as given or as resolved; otherwise it is simply
    /// not read instead of refusing the run.
    pub existing_only: bool,
    /// Added to the run's read roots, in rule order.
    pub read_root: bool,
}

/// One environment entry of a run; nothing else reaches the process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvRule {
    /// A literal value.
    Literal {
        /// Variable name.
        name: &'static str,
        /// Value.
        value: &'static str,
    },
    /// The value of a [`Param::Token`], or of a [`Param::Path`] admitted by its [`PathRule`];
    /// absent when `optional` and the parameter is missing.
    Param {
        /// Variable name.
        name: &'static str,
        /// Parameter name.
        param: &'static str,
        /// Whether the entry may be absent.
        optional: bool,
    },
    /// `prefix` followed by an admitted path (for example `-Clinker=<path>`); absent when the
    /// parameter is missing.
    Joined {
        /// Variable name.
        name: &'static str,
        /// Literal prefix.
        prefix: &'static str,
        /// Parameter name (a [`Param::Path`] with its own [`PathRule`]).
        param: &'static str,
    },
    /// Entries of a [`Param::Env`] whose names are `prefix` + one uppercase identifier +
    /// `suffix`; each value is a path admitted under `roles`.
    Pattern {
        /// Name prefix.
        prefix: &'static str,
        /// Name suffix.
        suffix: &'static str,
        /// Parameter name.
        param: &'static str,
        /// How each value is admitted.
        roles: &'static [PathRole],
    },
    /// A search path: the admitted paths of a [`Param::Paths`] followed by fixed entries, joined
    /// with `:`.
    SearchPath {
        /// Variable name.
        name: &'static str,
        /// Parameter name (its own [`PathRule`] admits the entries).
        param: &'static str,
        /// Fixed trailing entries.
        fixed: &'static [&'static str],
    },
    /// The directory of a resolved executable slot followed by fixed entries, joined with `:`
    /// (a script tool whose interpreter sits beside it, such as `npx` and `node`).
    SlotDir {
        /// Variable name.
        name: &'static str,
        /// The executable slot whose resolved program's directory leads the path.
        slot: &'static str,
        /// Fixed trailing entries.
        fixed: &'static [&'static str],
    },
    /// The real user home.
    Home {
        /// Variable name.
        name: &'static str,
    },
    /// The expanded run's final read roots, in order, as a JSON array of paths (for a tool that
    /// enforces them itself).
    ReadRootsJson {
        /// Variable name.
        name: &'static str,
    },
    /// The host read denies of the run as a JSON array, serialized exactly as the core holds
    /// them.
    ReadDeniesJson {
        /// Variable name.
        name: &'static str,
    },
}

/// A static file a recipe's run needs in the private cache (an embedded adapter script): before
/// the spawn the core writes `bytes` at the admitted [`PathRole::Cache`] path of the [`Param::Path`]
/// named `param` (a fresh temporary file renamed into place; nothing is followed).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecipeAsset {
    /// The path parameter naming where it is staged (a rule whose roles are exactly
    /// [`PathRole::Cache`]).
    pub param: &'static str,
    /// Exact bytes.
    pub bytes: &'static [u8],
}

/// Where a named executable comes from; the core alone resolves and measures it before a spawn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlotSource {
    /// A named program of the language's admitted launcher configuration, as its `describe`
    /// answer names it ([`NamedProgram`]).
    Launcher(&'static str),
    /// A user-installed tool by name, looked up in the descriptor's home tool directories and
    /// the daemon's tool `PATH`.
    HomeTool(&'static str),
}

/// One named executable a recipe may run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutableSlot {
    /// Slot name a [`Param::Executable`] names.
    pub name: &'static str,
    /// Where it comes from.
    pub source: SlotSource,
}

/// What a run reads on stdin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stdin {
    /// Nothing (closed).
    Null,
    /// The core's own candidate text (formatter, syntax probe).
    Candidate,
}

/// Admission class of a run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunClass {
    /// Interactive edit-path tools (formatter, probe); short budget.
    Interactive,
    /// Project checks; never holds the interactive lane.
    Background,
    /// A module-planned test run: an `ide.test` job under the call's own budget.
    Test,
}

/// A finite effect recipe, compiled into the root's language descriptor as data. The module
/// chooses a recipe id and computes its typed parameters; the core admits every parameter against
/// the declared rules and expands the confined run specification itself.
#[derive(Clone, Copy, Debug)]
pub struct EffectRecipe {
    /// Id the module names.
    pub id: &'static str,
    /// Parameter whose value runs: a [`Param::Executable`] slot the core resolves, or a
    /// [`Param::Path`] admitted under its [`PathRule`] (a project environment's tool) whose bytes
    /// the core measures before the spawn.
    pub program: &'static str,
    /// Argument template.
    pub args: &'static [Arg],
    /// Environment entries.
    pub env: &'static [EnvRule],
    /// Path parameters and their admission.
    pub paths: &'static [PathRule],
    /// The executable slots this recipe may name.
    pub executables: &'static [ExecutableSlot],
    /// What stdin carries.
    pub stdin: Stdin,
    /// Admission class.
    pub class: RunClass,
    /// Ceiling of the run's wall clock; the effective timeout is the smaller of this and the
    /// request's own (project checks declare at least 900 s).
    pub timeout_ceiling_ms: u64,
    /// Capture ceiling per stream; the core keeps the class-specific existing caps (64 MiB for
    /// project checks) and marks truncation explicitly.
    pub capture_bytes: u64,
    /// Static files staged in the private cache before the spawn.
    pub assets: &'static [RecipeAsset],
}

// ---- describe ----

/// `describe`: the language's interpretation of its launcher configuration and presence, asked in
/// the module's role with a bounded budget wherever it runs before or outside a session
/// (launcher parse, doctor, check scheduling). Static descriptor data (ids, settings keys, option
/// field names, cache directory names, marker file names) stays compiled in the core.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum DescribeQuery {
    /// Decode and validate one provider declaration (its option fields included) and name the
    /// executables startup verifies and the programs doctor probes; answers
    /// `Result<LaunchDescription, String>`.
    Provider {
        /// The raw declaration object from the launcher configuration.
        declaration: Value,
    },
    /// Further startup verification of one declaration after its executables were verified;
    /// answers `Result<(), String>`.
    VerifyProvider {
        /// The raw declaration object.
        declaration: Value,
    },
    /// Decode and validate the language's `project_checks` section and name the programs doctor
    /// probes; answers `Result<ChecksDescription, String>`.
    Checks {
        /// The raw section.
        section: Value,
    },
    /// Whether `worktree` is a project of the language for project checks; answers `bool`.
    Presence {
        /// Absolute worktree.
        worktree: PathBuf,
    },
    /// The language's interpretation of the project files the core read for `document` (exact
    /// reads, path admission, bounds, digests and re-observation stay with the core); answers
    /// [`InputsVerdict`]. Stateless: the core repeats it with more inputs while the verdict
    /// asks for them, at most [`MAX_INPUT_ROUNDS`] times. File contents travel as byte
    /// attachments of the same message, never inside this control body.
    ProjectInputs {
        /// Worktree-relative document.
        document: PathBuf,
        /// Every input read so far, at most [`MAX_PROJECT_INPUTS`].
        inputs: Vec<ProjectInput>,
    },
}

/// Most inputs one [`DescribeQuery::ProjectInputs`] carries.
pub const MAX_PROJECT_INPUTS: usize = 256;
/// Most paths one [`InputsVerdict::Need`] asks for.
pub const MAX_NEEDED_PATHS: usize = 64;
/// Longest [`InputsVerdict::Rejected`] reason, in bytes.
pub const MAX_INPUTS_REASON: usize = 512;
/// Most [`DescribeQuery::ProjectInputs`] rounds for one document.
pub const MAX_INPUT_ROUNDS: usize = 8;

/// One project file the core read for a language's interpretation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectInput {
    /// Worktree-relative path.
    pub path: PathBuf,
    /// Exact byte length.
    pub bytes: u64,
    /// BLAKE3 digest of the bytes, lowercase hex.
    pub blake3: String,
    /// The id of the attachment carrying the exact bytes, for a file the language parses;
    /// `None` for one it only fingerprints.
    pub contents: Option<u32>,
}

/// What a language makes of the project inputs read so far.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputsVerdict {
    /// The inputs form a closed, admissible project for the document.
    Accepted,
    /// Further files must be read (and the query repeated with them).
    Need {
        /// Worktree-relative paths, in the order the language wants them, at most
        /// [`MAX_NEEDED_PATHS`].
        paths: Vec<PathBuf>,
    },
    /// The document cannot be served from these inputs.
    Rejected {
        /// The input the refusal concerns, if one.
        file: Option<PathBuf>,
        /// Short plain-words reason, at most [`MAX_INPUTS_REASON`] bytes.
        reason: String,
    },
}

impl InputsVerdict {
    /// Whether the verdict keeps its bounds: at most [`MAX_NEEDED_PATHS`] relative paths
    /// without `..`, a reason of at most [`MAX_INPUTS_REASON`] bytes without control characters.
    pub fn bounded(&self) -> bool {
        let relative = |path: &Path| {
            !path.as_os_str().is_empty()
                && path
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
        };
        match self {
            Self::Accepted => true,
            Self::Need { paths } => {
                !paths.is_empty()
                    && paths.len() <= MAX_NEEDED_PATHS
                    && paths.iter().all(|path| relative(path))
            }
            Self::Rejected { file, reason } => {
                reason.len() <= MAX_INPUTS_REASON
                    && !reason.chars().any(char::is_control)
                    && file.as_deref().is_none_or(relative)
            }
        }
    }
}

/// One accepted executable as the launcher declares it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredExecutable {
    /// Absolute path.
    pub path: PathBuf,
    /// Accepted identity.
    pub identity: String,
    /// Accepted BLAKE3 digest.
    pub blake3: String,
}

/// A validated provider declaration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchDescription {
    /// The server-specific declaration rules hold.
    pub valid: bool,
    /// Additional executables startup verifies beside the server executable.
    pub executables: Vec<DeclaredExecutable>,
    /// Programs doctor probes, each with the interpreter that runs it.
    pub toolchain_programs: Vec<(PathBuf, Option<PathBuf>)>,
    /// The declaration's syntax-probe programs, if any.
    pub probe_programs: Option<ProbePrograms>,
}

/// A validated `project_checks` section.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChecksDescription {
    /// Every declared path is absolute and normal.
    pub valid: bool,
    /// The section's named programs: what doctor probes and what recipe slots name.
    pub programs: Vec<NamedProgram>,
    /// The roots the section declares ([`PathRole::LauncherRoot`]): toolchain and tool roots,
    /// accepted executables' installation prefixes. The core never parses a section itself.
    pub launcher_roots: Vec<PathBuf>,
    /// Developer directories the section names as an override ([`PathRole::DeveloperDir`]);
    /// the core adds its own platform resolution.
    pub developer_dirs: Vec<PathBuf>,
}

/// One named program of a launcher section.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedProgram {
    /// Slot name ([`SlotSource::Launcher`]).
    pub name: String,
    /// Absolute program path.
    pub path: PathBuf,
    /// The interpreter that runs it when it is not directly executable.
    pub interpreter: Option<PathBuf>,
}

// ---- linkage/0 ----

/// `linkage`: anchors of one file, or the core-routed resolution of one raw key.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum LinkageQuery {
    /// Answers [`AnchorBatch`]; the same DTO as `analyze_source.anchors`.
    Anchors {
        /// The file.
        source: SourceRef,
    },
    /// Answers [`ResolveAnswer`].
    Resolve(ResolveRequest),
}

/// Role of an anchor in its namespace.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorRole {
    /// Defines the key.
    Definition,
    /// Uses the key.
    Use,
}

/// How certain an anchor or candidate is.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorCertainty {
    /// Syntactically exact.
    Exact,
    /// A labelled heuristic.
    Heuristic,
    /// Not verified.
    Unverified,
}

/// One per-file linkage fact.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Anchor {
    /// Compiled namespace id (`class/v1`); a module cannot invent one.
    pub namespace: String,
    /// Scope inside the worktree; empty is worktree-global.
    pub domain: String,
    /// The normalized key.
    pub normalized_key: String,
    /// Definition or use.
    pub role: AnchorRole,
    /// Where.
    pub location: Location,
    /// Certainty.
    pub certainty: AnchorCertainty,
    /// The heuristic's label, when not exact.
    pub reason: Option<String>,
}

/// Whether a file was indexed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileVerdict {
    /// Indexed.
    Indexed,
    /// Skipped, with the reason (minified, too large).
    Skipped(String),
}

/// Anchors of one file.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnchorBatch {
    /// Indexed or skipped.
    pub verdict: FileVerdict,
    /// The per-file limit cut the list.
    pub capped: bool,
    /// Facts the extractor's own validation rejected.
    pub rejected: u32,
    /// The namespaces and roles this file was examined for.
    pub coverage: Vec<LinkageCoverage>,
    /// At most [`MAX_ANCHORS_PER_FILE`] anchors.
    pub anchors: Vec<Anchor>,
}

/// `linkage.resolve` of one raw key that needs language interpretation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveRequest {
    /// Namespace.
    pub namespace: String,
    /// The key as written.
    pub raw_key: String,
    /// Scope-relative file it was written in.
    pub from_path: PathBuf,
    /// That file's revision.
    pub source_revision: String,
    /// Revision of the resolution configuration (`tsconfig`, package rules).
    pub config_revision: String,
    /// At most [`MAX_RESOLVE_CANDIDATES`].
    pub candidate_limit: u32,
}

/// Candidates of one resolve; the core checks targets and builds any edge itself.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveAnswer {
    /// Candidate keys.
    pub candidates: Vec<ResolveCandidate>,
    /// The limit cut the list.
    pub capped: bool,
}

/// One resolution candidate.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveCandidate {
    /// Domain.
    pub domain: String,
    /// Normalized key.
    pub normalized_key: String,
    /// Certainty.
    pub certainty: AnchorCertainty,
    /// Bounded evidence text.
    pub evidence: String,
}

impl AnchorBatch {
    /// Checks the batch against the request's `source`, its exact `text` and the module's
    /// `declared` linkage coverage from `hello`: at most [`MAX_ANCHORS_PER_FILE`] anchors; no
    /// anchor on a skipped file; every claimed coverage entry within the declaration; each anchor
    /// in a compiled namespace ([`namespace_registered`]) and a role the declaration covers, in the
    /// request's file and revision with an ordered in-bounds range on UTF-8 boundaries (an empty
    /// point range is allowed), under that namespace's key rules ([`key_valid`]). The core still
    /// re-checks every location against its own observation before showing it.
    pub fn validate(
        &self,
        source: &SourceRef,
        text: &str,
        declared: &[LinkageCoverage],
    ) -> Result<(), String> {
        if self.anchors.len() > MAX_ANCHORS_PER_FILE {
            return Err("too many anchors".to_owned());
        }
        if matches!(self.verdict, FileVerdict::Skipped(_)) && !self.anchors.is_empty() {
            return Err("anchors on a skipped file".to_owned());
        }
        let declares = |namespace: &str, role: Option<AnchorRole>| {
            declared.iter().any(|coverage| {
                coverage.namespace == namespace
                    && match role {
                        Some(AnchorRole::Definition) => coverage.defines,
                        Some(AnchorRole::Use) => coverage.uses,
                        None => true,
                    }
            })
        };
        if let Some(claimed) = self.coverage.iter().find(|claimed| {
            !declared.iter().any(|coverage| {
                coverage.namespace == claimed.namespace
                    && (coverage.defines || !claimed.defines)
                    && (coverage.uses || !claimed.uses)
            })
        }) {
            return Err(format!("undeclared coverage `{}`", claimed.namespace));
        }
        for anchor in &self.anchors {
            let location = &anchor.location;
            let (start, end) = (location.start_byte, location.end_byte);
            let placed = location.path == source.path
                && location.revision.as_deref() == Some(source.revision.as_str())
                && start <= end
                && end <= text.len() as u64
                && text.is_char_boundary(start as usize)
                && text.is_char_boundary(end as usize);
            if !placed
                || !namespace_registered(&anchor.namespace)
                || !declares(&anchor.namespace, Some(anchor.role))
                || !key_valid(&anchor.namespace, &anchor.normalized_key, &anchor.domain)
            {
                return Err(format!("invalid anchor `{}`", anchor.normalized_key));
            }
        }
        Ok(())
    }
}

/// Whether `namespace` is compiled registry data: a core name namespace or the file-reference
/// namespace. A module cannot invent one.
pub fn namespace_registered(namespace: &str) -> bool {
    namespace == FILE_REF_NAMESPACE
        || crate::lang::names::ns::ALL
            .iter()
            .any(|registered| registered.id() == namespace)
}

/// Namespace of file references: a scope-relative path key of at most [`MAX_FILE_REF_KEY`]
/// bytes, spaces allowed.
pub const FILE_REF_NAMESPACE: &str = "file-ref/v1";

/// Per-namespace key rules: a file reference key is a nonempty path of at most
/// [`MAX_FILE_REF_KEY`] bytes without control characters; every other namespace keeps the
/// existing name rules (nonempty, at most 256 bytes, no whitespace or control characters) for
/// both key and domain.
pub fn key_valid(namespace: &str, key: &str, domain: &str) -> bool {
    let name = |text: &str| {
        text.len() <= crate::lang::names::MAX_NAME_BYTES
            && !text.chars().any(|ch| ch.is_control() || ch.is_whitespace())
    };
    if namespace == FILE_REF_NAMESPACE {
        !key.is_empty()
            && key.len() <= MAX_FILE_REF_KEY
            && !key.chars().any(char::is_control)
            && domain.is_empty()
    } else {
        !key.is_empty() && name(key) && name(domain)
    }
}

/// Which roles of one namespace a module emits, for coverage disclosure (`unavailable for:`).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkageCoverage {
    /// Namespace id.
    pub namespace: String,
    /// The module emits definitions.
    pub defines: bool,
    /// The module emits uses.
    pub uses: bool,
}

/// Everything a cached module fact depends on; the core keys its fact and anchor caches by the
/// whole fence, so a change of any part (a style-module domain follows the path) misses the cache.
/// Core-side only; never sent on the wire.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct FactFence {
    /// Scope-relative path.
    pub path: PathBuf,
    /// Opaque scope key (worktree, settings view).
    pub scope_key: String,
    /// Exact source revision.
    pub source_revision: String,
    /// Module id.
    pub module_id: super::contract::ModuleId,
    /// Module package version (the build).
    pub package_version: String,
    /// Protocol and capability schema version.
    pub schema: u32,
    /// Accepted configuration revision.
    pub config_revision: String,
    /// Selected environment identity.
    pub environment: String,
    /// Provider readiness generation the facts were computed under.
    pub readiness_generation: u64,
}

/// Decodes a payload or answer into its typed form; the error is the serde message.
pub fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|error| error.to_string())
}

/// Encodes a typed payload or answer.
pub fn encode(value: &impl Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One observed file.
    fn observed(path: &Path) -> Option<(&'static str, &'static str)> {
        (path == Path::new("a.txt")).then_some(("r1", "héllo world"))
    }

    /// A one-file proposal with `replacements`.
    fn proposal(path: &str, base: &str, replacements: &[(u64, u64)]) -> EditProposal {
        EditProposal {
            files: vec![FileEdit {
                path: path.into(),
                base_revision: base.into(),
                replacements: replacements
                    .iter()
                    .map(|&(start_byte, end_byte)| Replacement {
                        start_byte,
                        end_byte,
                        new_text: "x".into(),
                    })
                    .collect(),
            }],
        }
    }

    /// Stale bases, out-of-scope paths, overlaps, reversed or out-of-bounds ranges, split UTF-8
    /// sequences and duplicate files are refused before any write; a good proposal passes.
    #[test]
    fn edit_proposals_are_validated_before_writing() {
        assert_eq!(
            proposal("a.txt", "r1", &[(0, 1), (7, 12)]).validate(observed),
            Ok(())
        );
        let cases = [
            (proposal("a.txt", "r0", &[(0, 1)]), "stale"),
            (proposal("b.txt", "r1", &[(0, 1)]), "stale"),
            (proposal("../a.txt", "r1", &[(0, 1)]), "scope"),
            (proposal("/a.txt", "r1", &[(0, 1)]), "scope"),
            (proposal("./a.txt", "r1", &[(0, 1)]), "scope"),
            (proposal("a.txt", "r1", &[(0, 4), (3, 5)]), "range"),
            (proposal("a.txt", "r1", &[(4, 3)]), "range"),
            (proposal("a.txt", "r1", &[(0, 99)]), "range"),
            (proposal("a.txt", "r1", &[(2, 4)]), "boundary"),
        ];
        for (proposal, expected) in cases {
            let refusal = proposal.validate(observed).unwrap_err();
            let kind = match refusal {
                EditRefusal::Stale(_) => "stale",
                EditRefusal::OutOfScope(_) => "scope",
                EditRefusal::BadRange(_) => "range",
                EditRefusal::NotCharBoundary(_) => "boundary",
                EditRefusal::DuplicateFile(_) => "duplicate",
            };
            assert_eq!(kind, expected, "{proposal:?}");
        }
        let mut twice = proposal("a.txt", "r1", &[(0, 1)]);
        twice.files.push(twice.files[0].clone());
        assert!(matches!(
            twice.validate(observed),
            Err(EditRefusal::DuplicateFile(_))
        ));
    }

    /// Tagged queries and answers round-trip and refuse unknown fields.
    #[test]
    fn payloads_round_trip() {
        let source = SourceRef {
            path: "a.txt".into(),
            revision: "r1".into(),
            text: SourceText::Inline("x".into()),
        };
        let query = SemanticQuery::References {
            source: source.clone(),
            byte_offset: 0,
        };
        assert_eq!(decode::<SemanticQuery>(encode(&query)), Ok(query));
        let mut extra = encode(&LinkageQuery::Anchors { source });
        extra["surprise"] = Value::Bool(true);
        assert!(decode::<LinkageQuery>(extra).is_err());
        let answer: CheckSelectionAnswer = Err("no such env".into());
        assert_eq!(decode::<CheckSelectionAnswer>(encode(&answer)), Ok(answer));
        let field: Field<Option<String>> = Field::Warming;
        assert_eq!(decode::<Field<Option<String>>>(encode(&field)), Ok(field));
    }

    /// Unknown fields are refused inside nested reused types too, not only at the top level.
    #[test]
    fn nested_unknown_fields_are_refused() {
        crate::lang::testing::install();
        let request = InsertSiteRequest {
            source: SourceRef {
                path: "a.txt".into(),
                revision: "r1".into(),
                text: SourceText::Missing,
            },
            outline: Outline {
                file: "a.txt".into(),
                language: crate::lang::testing::ALPHA,
                line_count: 0,
                symbols: Vec::new(),
            },
            anchor: SymbolPath::parse("a.txt#f").unwrap(),
            placement: InsertWhere::After,
        };
        let good = encode(&request);
        assert_eq!(decode::<InsertSiteRequest>(good.clone()), Ok(request));
        for pointer in ["/outline", "/anchor", "/source"] {
            let mut bad = good.clone();
            bad.pointer_mut(pointer).unwrap()["extra"] = Value::Bool(true);
            assert!(decode::<InsertSiteRequest>(bad).is_err(), "{pointer}");
        }
        let outcome = serde_json::json!({"refused": {"cause": "timeout", "message": "", "x": 1}});
        assert!(decode::<EffectOutcome>(outcome).is_err());
    }

    /// Recipe parameters carry no count ceiling of their own (a deep worktree's ancestor files
    /// exceed any small fixed bound); describe queries and the hello configuration round-trip and
    /// refuse unknown fields.
    #[test]
    fn recipe_parameters_and_describe_round_trip() {
        let roots: Vec<PathBuf> = (0..300)
            .map(|n| PathBuf::from(format!("/a/{n}/Cargo.toml")))
            .collect();
        let effect = EffectRequest {
            recipe: "check".into(),
            params: BTreeMap::from([
                ("roots".to_owned(), Param::Paths(roots)),
                (
                    "linkers".to_owned(),
                    Param::Env(BTreeMap::from([(
                        "CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER".to_owned(),
                        "/usr/bin/cc".to_owned(),
                    )])),
                ),
                ("cargo".to_owned(), Param::Executable("cargo".into())),
            ]),
        };
        assert_eq!(decode::<EffectRequest>(encode(&effect)), Ok(effect));
        let query = DescribeQuery::Provider {
            declaration: serde_json::json!({"settings": "x"}),
        };
        assert_eq!(decode::<DescribeQuery>(encode(&query)), Ok(query));
        let config = crate::modules::contract::ModuleConfig {
            worktree: Some("/w".into()),
            provider: None,
            checks: Some(serde_json::json!({})),
            env: BTreeMap::from([("AGENT_IDE_HOME".to_owned(), "/h".to_owned())]),
            home: Some("/h".into()),
        };
        let mut value = encode(&config);
        assert_eq!(decode(value.clone()), Ok(config));
        value["credentials"] = Value::Bool(true);
        assert!(decode::<crate::modules::contract::ModuleConfig>(value).is_err());
    }

    /// File references allow long paths with spaces; other namespaces keep the name rules.
    #[test]
    fn key_rules_follow_the_namespace() {
        assert!(key_valid(FILE_REF_NAMESPACE, "web/my page.view", ""));
        assert!(key_valid(
            FILE_REF_NAMESPACE,
            &"a".repeat(MAX_FILE_REF_KEY),
            ""
        ));
        assert!(!key_valid(
            FILE_REF_NAMESPACE,
            &"a".repeat(MAX_FILE_REF_KEY + 1),
            ""
        ));
        assert!(!key_valid(FILE_REF_NAMESPACE, "a\nb", ""));
        assert!(key_valid("class/v1", "btn", "src/a.module.style"));
        assert!(!key_valid("class/v1", "my btn", ""));
        assert!(!key_valid("class/v1", &"a".repeat(257), ""));
        assert!(!key_valid("class/v1", "btn", "a b"));
        assert!(!key_valid("class/v1", "", ""));
    }
}
