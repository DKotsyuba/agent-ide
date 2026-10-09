//! The async language-computation facade every core call site uses: the same operations as
//! [`LanguageSupport`], answered in process or by the language's module per the daemon's
//! [`ModuleHost`], never by blocking a worker thread on IPC.
//!
//! Without an installed host (unit tests, tools that never start a daemon) every language computes
//! in process, exactly as before. A module failure is the typed [`ModuleUnavailable`] the caller
//! maps onto its existing refusal; it never falls back in process silently. Per-file test facts
//! are memoized per module build (they depend on the path alone).
//!
//! [`LanguageSupport`]: crate::lang::LanguageSupport

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use super::{
    adapter::effect_argv,
    contract::{Capability, ModuleUnavailable},
    mode::Mode,
    payload::{
        AnalysisScopeRequest, AnalyzeSource, CheckSelectionAnswer, CommandEnvAnswer, DetectAnswer,
        EffectRequest, EnvironmentsAnswer, Field, FileDocRequest, FormatPlanRequest,
        InsertSiteAnswer, InsertSiteRequest, ProjectQuery, SelectionAnswer, SourceAnalysis,
        SourceField, SourceRef, SourceText, SyntaxQuery, TestFacts, TestParseRequest,
        TestPlanQuery, TestToolchainAnswer, encode,
    },
    router::ModuleHost,
    wire::Attachment,
};
use crate::lang::{
    InsertSite, InsertWhere, LangError, Language, LanguageProject, Outline, ProbePrograms,
    SymbolPath, SyntaxVerdict, TestReport, TestSelection, TestTarget, environment,
};

/// The daemon's module routing, installed once at startup.
static HOST: OnceLock<Arc<ModuleHost>> = OnceLock::new();

/// Installs the daemon's module routing; later calls keep the first host.
pub fn install(host: Arc<ModuleHost>) {
    let _ = HOST.set(host);
}

/// The installed host when `language` runs in module mode.
fn module(language: Language) -> Option<&'static Arc<ModuleHost>> {
    HOST.get()
        .filter(|host| host.mode(language) == Mode::Module)
}

/// The project checker of `language` with its raw launcher `section` when the language computes
/// in its module; `None` keeps the in-process checker.
pub fn checker(
    language: Language,
    section: &serde_json::Value,
    runner: Arc<dyn crate::checks::runner::ConfinedRunner>,
    timeout: std::time::Duration,
) -> Option<Arc<dyn crate::checks::Checker>> {
    let host = module(language)?.clone();
    Some(Arc::new(super::checker::ModuleChecker::new(
        language,
        host,
        section.clone(),
        runner,
        timeout,
    )))
}

/// The daemon's modes line for `languages` ([`ModuleHost::modes_line`]); `None` without a host
/// or when none of them ships a module.
pub fn modes_line(languages: &[Language]) -> Option<String> {
    HOST.get()?.modes_line(languages)
}

/// Where a backend starts `language`'s provider: `None` in process; in module mode the pinned
/// module executable, or the typed refusal when it cannot be pinned (never a silent fallback).
pub fn module_executable(
    language: Language,
) -> Option<Routed<Arc<super::launch::ModuleExecutable>>> {
    Some(module(language)?.executable(language))
}

/// Where `language` computes in this process.
pub fn mode(language: Language) -> Mode {
    module(language).map_or(Mode::InProcess, |_| Mode::Module)
}

/// Result of one routed computation.
pub type Routed<T> = Result<T, ModuleUnavailable>;

/// An inline source for `path` with `text`, keyed by its content digest.
fn source(path: &Path, text: &str) -> (SourceRef, Vec<Attachment>) {
    let revision = format!("blake3:{}", blake3::hash(text.as_bytes()).to_hex());
    if text.len() <= super::payload::MAX_INLINE_SOURCE {
        return (
            SourceRef {
                path: path.to_path_buf(),
                revision,
                text: SourceText::Inline(text.to_owned()),
            },
            Vec::new(),
        );
    }
    (
        SourceRef {
            path: path.to_path_buf(),
            revision,
            text: SourceText::Attachment(1),
        },
        vec![Attachment {
            id: 1,
            content_type: "text/plain; charset=utf-8".to_owned(),
            bytes: text.as_bytes().to_vec(),
        }],
    )
}

/// One `analyze_source` batch for `fields` of `path` (`text` absent for path-only facts).
async fn analyze(
    host: &ModuleHost,
    language: Language,
    worktree: &Path,
    path: &Path,
    text: Option<&str>,
    fields: Vec<SourceField>,
) -> Routed<SourceAnalysis> {
    let (source, attachments) = match text {
        Some(text) => source(path, text),
        None => (
            SourceRef {
                path: path.to_path_buf(),
                revision: "path".to_owned(),
                text: SourceText::Missing,
            },
            Vec::new(),
        ),
    };
    host.request(
        language,
        worktree,
        Capability::AnalyzeSource,
        encode(&AnalyzeSource { source, fields }),
        attachments,
    )
    .await
}

/// `value` when computed; an unsupported or warming field answers `default` (the language does
/// not compute it, which is what the in-process default returns).
fn field<T>(value: Field<T>, default: T) -> T {
    match value {
        Field::Available(value) => value,
        _ => default,
    }
}

/// `LanguageSupport::detect`.
pub async fn detect(language: Language, root: &Path) -> Routed<Option<LanguageProject>> {
    match module(language) {
        None => {
            let root = root.to_path_buf();
            Ok(
                tokio::task::spawn_blocking(move || language.support().detect(&root))
                    .await
                    .unwrap_or(None),
            )
        }
        Some(host) => {
            host.request::<DetectAnswer>(
                language,
                root,
                Capability::Project,
                encode(&ProjectQuery::Detect {
                    root: root.to_path_buf(),
                }),
                Vec::new(),
            )
            .await
        }
    }
}

/// `LanguageSupport::environments` with the stored selections.
pub async fn environments(
    language: Language,
    worktree: &Path,
) -> Routed<Vec<environment::ResolvedEnv>> {
    match module(language) {
        None => {
            let worktree = worktree.to_path_buf();
            Ok(
                tokio::task::spawn_blocking(move || language.support().environments(&worktree))
                    .await
                    .unwrap_or_default(),
            )
        }
        Some(host) => {
            host.request::<EnvironmentsAnswer>(
                language,
                worktree,
                Capability::Project,
                encode(&ProjectQuery::Environments {
                    worktree: worktree.to_path_buf(),
                    selections: environment::selections(worktree, language),
                }),
                Vec::new(),
            )
            .await
        }
    }
}

/// `LanguageSupport::check_selection`.
pub async fn check_selection(
    language: Language,
    worktree: &Path,
    root: &Path,
    selector: &str,
) -> Routed<Result<(), String>> {
    match module(language) {
        None => Ok(language.support().check_selection(worktree, root, selector)),
        Some(host) => {
            host.request::<CheckSelectionAnswer>(
                language,
                worktree,
                Capability::Project,
                encode(&ProjectQuery::CheckSelection {
                    worktree: worktree.to_path_buf(),
                    root: root.to_path_buf(),
                    selector: selector.to_owned(),
                }),
                Vec::new(),
            )
            .await
        }
    }
}

/// `LanguageSupport::command_env` with the stored selections.
pub async fn command_env(
    language: Language,
    worktree: &Path,
    cwd: &Path,
    program: &str,
) -> Routed<Option<environment::CommandEnv>> {
    match module(language) {
        None => Ok(language.support().command_env(worktree, cwd, program)),
        Some(host) => {
            host.request::<CommandEnvAnswer>(
                language,
                worktree,
                Capability::Project,
                encode(&ProjectQuery::CommandEnv {
                    worktree: worktree.to_path_buf(),
                    cwd: cwd.to_path_buf(),
                    program: program.to_owned(),
                    selections: environment::selections(worktree, language),
                }),
                Vec::new(),
            )
            .await
        }
    }
}

/// `LanguageSupport::test_toolchain`.
pub async fn test_toolchain(
    language: Language,
    worktree: &Path,
    program: &str,
) -> Routed<Option<(PathBuf, PathBuf)>> {
    match module(language) {
        None => Ok(language.support().test_toolchain(program)),
        Some(host) => {
            host.request::<TestToolchainAnswer>(
                language,
                worktree,
                Capability::Project,
                encode(&ProjectQuery::TestToolchain {
                    program: program.to_owned(),
                }),
                Vec::new(),
            )
            .await
        }
    }
}

/// `LanguageSupport::file_doc` of `text` (the file at `path`).
pub async fn file_doc(
    language: Language,
    worktree: &Path,
    path: &Path,
    text: &str,
) -> Routed<Option<String>> {
    match module(language) {
        None => Ok(language.support().file_doc(text)),
        Some(host) => {
            let (source, attachments) = source(path, text);
            host.request(
                language,
                worktree,
                Capability::FileDoc,
                encode(&FileDocRequest { source }),
                attachments,
            )
            .await
        }
    }
}

/// `LanguageSupport::outline_from_source`.
pub async fn outline_from_source(
    language: Language,
    worktree: &Path,
    path: &Path,
    text: &str,
) -> Routed<Option<Outline>> {
    match module(language) {
        None => Ok(language.support().outline_from_source(path, text)),
        Some(host) => Ok(field(
            analyze(
                host,
                language,
                worktree,
                path,
                Some(text),
                vec![SourceField::Outline],
            )
            .await?
            .outline,
            None,
        )),
    }
}

/// `LanguageSupport::syntax_verdict`.
pub async fn syntax_verdict(
    language: Language,
    worktree: &Path,
    path: &Path,
    text: &str,
) -> Routed<SyntaxVerdict> {
    match module(language) {
        None => Ok(language.support().syntax_verdict(path, text)),
        Some(host) => Ok(field(
            analyze(
                host,
                language,
                worktree,
                path,
                Some(text),
                vec![SourceField::Syntax],
            )
            .await?
            .syntax,
            SyntaxVerdict::Unchecked,
        )),
    }
}

/// Memoized test facts by module build, language and path.
fn test_facts_memo() -> &'static Mutex<HashMap<(String, PathBuf), TestFacts>> {
    static MEMO: OnceLock<Mutex<HashMap<(String, PathBuf), TestFacts>>> = OnceLock::new();
    MEMO.get_or_init(Mutex::default)
}

/// The test facts of `path` (`is_test_file`, `test_binary`).
pub async fn test_facts(language: Language, worktree: &Path, path: &Path) -> Routed<TestFacts> {
    let Some(host) = module(language) else {
        let support = language.support();
        return Ok(TestFacts {
            is_test_file: support.is_test_file(path),
            test_binary: support.test_binary(path),
        });
    };
    let key = (language.name().to_owned(), path.to_path_buf());
    if let Some(facts) = test_facts_memo()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
    {
        return Ok(facts.clone());
    }
    let facts = field(
        analyze(
            host,
            language,
            worktree,
            path,
            None,
            vec![SourceField::Tests],
        )
        .await?
        .tests,
        TestFacts {
            is_test_file: false,
            test_binary: None,
        },
    );
    let mut memo = test_facts_memo()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if memo.len() >= 4096 {
        memo.clear();
    }
    memo.insert(key, facts.clone());
    Ok(facts)
}

/// `LanguageSupport::insert_site`.
#[allow(clippy::too_many_arguments)]
pub async fn insert_site(
    language: Language,
    worktree: &Path,
    path: &Path,
    text: &str,
    outline: &Outline,
    anchor: &SymbolPath,
    placement: InsertWhere,
) -> Routed<Result<InsertSite, LangError>> {
    match module(language) {
        None => Ok(language
            .support()
            .insert_site(text, outline, anchor, placement)),
        Some(host) => {
            let (source, attachments) = source(path, text);
            host.request::<InsertSiteAnswer>(
                language,
                worktree,
                Capability::InsertSite,
                encode(&InsertSiteRequest {
                    source,
                    outline: outline.clone(),
                    anchor: anchor.clone(),
                    placement,
                }),
                attachments,
            )
            .await
        }
    }
}

/// `LanguageSupport::test_selection`.
pub async fn test_selection(
    language: Language,
    worktree: &Path,
    project: &LanguageProject,
    target: &TestTarget,
) -> Routed<Result<TestSelection, LangError>> {
    match module(language) {
        None => Ok(language.support().test_selection(project, target)),
        Some(host) => {
            host.request::<SelectionAnswer>(
                language,
                worktree,
                Capability::TestPlan,
                encode(&TestPlanQuery::Selection {
                    project: Box::new(project.clone()),
                    target: target.clone(),
                }),
                Vec::new(),
            )
            .await
        }
    }
}

/// `LanguageSupport::test_id` of one outline path.
pub async fn test_id(
    language: Language,
    worktree: &Path,
    file: &Path,
    outline_path: &str,
) -> Routed<String> {
    match module(language) {
        None => Ok(language.support().test_id(file, outline_path)),
        Some(host) => {
            let mut ids: Vec<String> = host
                .request(
                    language,
                    worktree,
                    Capability::TestPlan,
                    encode(&TestPlanQuery::TestIds {
                        file: file.to_path_buf(),
                        outline_paths: vec![outline_path.to_owned()],
                    }),
                    Vec::new(),
                )
                .await?;
            Ok(ids.pop().unwrap_or_default())
        }
    }
}

/// `LanguageSupport::parse_test_output`.
pub async fn parse_test_output(
    language: Language,
    worktree: &Path,
    stdout: &str,
    stderr: &str,
) -> Routed<TestReport> {
    match module(language) {
        None => Ok(language.support().parse_test_output(stdout, stderr)),
        Some(host) => {
            host.request(
                language,
                worktree,
                Capability::TestParse,
                encode(&TestParseRequest {
                    stdout: 1,
                    stderr: 2,
                }),
                vec![
                    Attachment::octets(1, stdout.as_bytes().to_vec()),
                    Attachment::octets(2, stderr.as_bytes().to_vec()),
                ],
            )
            .await
        }
    }
}

/// The argv of a module's `argv` recipe answer; another recipe is refused as malformed.
fn argv_of(language: Language, effect: Option<EffectRequest>) -> Routed<Option<Vec<String>>> {
    match effect {
        None => Ok(None),
        Some(effect) => effect_argv(&effect)
            .map(Some)
            .ok_or_else(|| ModuleUnavailable {
                module_id: super::contract::ModuleId::bundled(language.name()),
                module_version: env!("CARGO_PKG_VERSION").to_owned(),
                role: super::contract::Role::Analyzer,
                stage: super::contract::Stage::Decode,
                cause: super::contract::Cause::PolicyRefused,
                instance: None,
                retry_after_ms: None,
            }),
    }
}

/// `LanguageSupport::format_stdin_command`.
pub async fn format_stdin_command(
    language: Language,
    worktree: &Path,
    project: &LanguageProject,
    file: &Path,
) -> Routed<Option<Vec<String>>> {
    match module(language) {
        None => Ok(language.support().format_stdin_command(project, file)),
        Some(host) => {
            let effect: Option<EffectRequest> = host
                .request(
                    language,
                    worktree,
                    Capability::FormatPlan,
                    encode(&FormatPlanRequest {
                        project: project.clone(),
                        file: file.to_path_buf(),
                    }),
                    Vec::new(),
                )
                .await?;
            argv_of(language, effect)
        }
    }
}

/// `LanguageSupport::syntax_probe_command`.
pub async fn syntax_probe_command(
    language: Language,
    worktree: &Path,
    project: &LanguageProject,
    root: &Path,
    file: &Path,
    configured: Option<&ProbePrograms>,
) -> Routed<Option<Vec<String>>> {
    match module(language) {
        None => Ok(language
            .support()
            .syntax_probe_command(project, root, file, configured)),
        Some(host) => {
            let effect: Option<EffectRequest> = host
                .request(
                    language,
                    worktree,
                    Capability::Syntax,
                    encode(&SyntaxQuery::ProbePlan {
                        project: Box::new(project.clone()),
                        root: root.to_path_buf(),
                        file: file.to_path_buf(),
                        configured: configured.cloned(),
                    }),
                    Vec::new(),
                )
                .await?;
            argv_of(language, effect)
        }
    }
}

/// `LanguageChecks::not_analysed`, as an owned reason.
pub async fn not_analysed(
    language: Language,
    worktree: &Path,
    path: &Path,
) -> Routed<Option<String>> {
    match module(language) {
        None => Ok(language
            .checks()
            .and_then(|checks| checks.not_analysed(worktree, path))
            .map(str::to_owned)),
        Some(host) => {
            host.request(
                language,
                worktree,
                Capability::AnalysisScope,
                encode(&AnalysisScopeRequest {
                    worktree: worktree.to_path_buf(),
                    path: path.to_path_buf(),
                }),
                Vec::new(),
            )
            .await
        }
    }
}
