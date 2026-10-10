//! The async language-computation facade every core call site uses: the same operations as
//! [`LanguageSupport`], answered in process or by the language's module per the daemon's
//! [`ModuleHost`], never by blocking a worker thread on IPC.
//!
//! Without an installed host (unit tests, tools that never start a daemon) every language computes
//! in process, exactly as before. A module failure is the typed [`ModuleUnavailable`] the caller
//! maps onto its existing refusal; it never falls back in process silently. A complete
//! `analyze_source` answer of text- and path-only fields (outline from source, file doc, structural
//! syntax verdict, test facts) is cached by language, worktree, path, exact source revision and
//! requested fields for the daemon's one pinned module build: those fields depend on nothing else
//! (no configuration, environment or provider readiness). Anchors, whose coverage the live
//! instance declares, and any warming or unsupported field are never cached.
//!
//! [`LanguageSupport`]: crate::lang::LanguageSupport

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use super::{
    contract::{Capability, ModuleUnavailable},
    mode::Mode,
    payload::{
        AnalysisScopeRequest, AnalyzeSource, AnchorBatch, CheckSelectionAnswer, CommandEnvAnswer,
        DetectAnswer, EffectRequest, EnvironmentsAnswer, Field, FileDocRequest, FileVerdict,
        FormatPlanRequest, InsertSiteAnswer, InsertSiteRequest, ProjectQuery, SelectionAnswer,
        SourceAnalysis, SourceField, SourceRef, SourceText, SyntaxQuery, TestFacts,
        TestParseRequest, TestPlanQuery, TestToolchainAnswer, encode,
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

/// The cache key of one `analyze_source` batch: language, worktree, path, source revision and
/// requested fields (the module build is the daemon's one pinned executable).
type AnalysisKey = (String, PathBuf, PathBuf, String, Vec<SourceField>);

/// Complete `analyze_source` answers (bounded; cleared when full).
fn analyses() -> &'static Mutex<HashMap<AnalysisKey, SourceAnalysis>> {
    static CACHE: OnceLock<Mutex<HashMap<AnalysisKey, SourceAnalysis>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// Whether `analysis` is a cacheable fact: every requested field computed (a warming field may
/// compute later, an unsupported one is not this source's fact) and none of them anchors (their
/// coverage is the live instance's declaration, validated per call, never a fact of the text).
fn complete(analysis: &SourceAnalysis, fields: &[SourceField]) -> bool {
    fields.iter().all(|field| match field {
        SourceField::Outline => matches!(analysis.outline, Field::Available(_)),
        SourceField::FileDoc => matches!(analysis.file_doc, Field::Available(_)),
        SourceField::Syntax => matches!(analysis.syntax, Field::Available(_)),
        SourceField::Tests => matches!(analysis.tests, Field::Available(_)),
        SourceField::Anchors => false,
    })
}

/// One `analyze_source` batch for `fields` of `path` (`text` absent for path-only facts), from
/// the cache when an identical batch of the same source revision was answered completely.
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
    let key = (
        language.name().to_owned(),
        worktree.to_path_buf(),
        path.to_path_buf(),
        source.revision.clone(),
        fields.clone(),
    );
    if let Some(analysis) = analyses()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
    {
        return Ok(analysis.clone());
    }
    let analysis: SourceAnalysis = host
        .request(
            language,
            worktree,
            Capability::AnalyzeSource,
            encode(&AnalyzeSource {
                source,
                fields: fields.clone(),
            }),
            attachments,
        )
        .await?;
    if complete(&analysis, &fields) {
        let mut cache = analyses()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.len() >= 4096 {
            cache.clear();
        }
        cache.insert(key, analysis.clone());
    }
    Ok(analysis)
}

/// The linkage anchors of `path` with `text`: `Ok(None)` when `language` computes in process
/// (the caller extracts its name facts itself); in module mode one `analyze_source` anchors
/// batch, validated against the request's source and the coverage the module declared in its
/// `hello` (a language that computes none, or not yet, answers an empty skipped batch). A module
/// fault or an invalid batch is the typed failure. Async: a synchronous caller blocks on it from
/// a blocking thread, never a runtime worker.
pub async fn anchors(
    language: Language,
    worktree: &Path,
    path: &Path,
    text: &str,
) -> Routed<Option<AnchorBatch>> {
    let Some(host) = module(language) else {
        return Ok(None);
    };
    let analysis = analyze(
        host,
        language,
        worktree,
        path,
        Some(text),
        vec![SourceField::Anchors],
    )
    .await?;
    let skipped = |reason: &str| AnchorBatch {
        verdict: FileVerdict::Skipped(reason.to_owned()),
        capped: false,
        rejected: 0,
        coverage: Vec::new(),
        anchors: Vec::new(),
    };
    let batch = match analysis.anchors {
        Field::Available(batch) => batch,
        Field::Warming => return Ok(Some(skipped("warming"))),
        Field::Unsupported | Field::NotRequested => return Ok(Some(skipped("unsupported"))),
    };
    let declared = host.linkage(language, worktree).await.unwrap_or_default();
    let (source, _) = source(path, text);
    batch
        .validate(&source, text, &declared)
        .map_err(|_| ModuleUnavailable {
            module_id: super::contract::ModuleId::bundled(language.name()),
            module_version: env!("CARGO_PKG_VERSION").to_owned(),
            role: super::contract::Role::Analyzer,
            stage: super::contract::Stage::Decode,
            cause: super::contract::Cause::Malformed,
            instance: None,
            retry_after_ms: None,
        })?;
    Ok(Some(batch))
}

/// `value` when computed; an unsupported or warming field answers `default` (the language does
/// not compute it, which is what the in-process default returns).
fn field<T>(value: Field<T>, default: T) -> T {
    match value {
        Field::Available(value) => value,
        _ => default,
    }
}

/// The language's interpretation of the project `inputs` read for `document`: `None` when the
/// language computes in process (the caller interprets them itself), otherwise its module's
/// verdict.
pub async fn project_inputs(
    language: Language,
    worktree: &Path,
    document: &Path,
    inputs: Vec<super::payload::ProjectInput>,
) -> Option<Routed<super::payload::InputsVerdict>> {
    let host = module(language)?;
    Some(
        host.request(
            language,
            worktree,
            Capability::Describe,
            encode(&super::payload::DescribeQuery::ProjectInputs {
                document: document.to_path_buf(),
                inputs,
            }),
            Vec::new(),
        )
        .await,
    )
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

/// The test facts of `path` (`is_test_file`, `test_binary`).
pub async fn test_facts(language: Language, worktree: &Path, path: &Path) -> Routed<TestFacts> {
    let Some(host) = module(language) else {
        let support = language.support();
        return Ok(TestFacts {
            is_test_file: support.is_test_file(path),
            test_binary: support.test_binary(path),
        });
    };
    Ok(field(
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
    ))
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

/// PATH for formatters, probes and the home tools a recipe names: the registered languages' home
/// tool directories (see
/// [`LanguageDescriptor::home_tool_dirs`](crate::lang::LanguageDescriptor::home_tool_dirs)), the
/// daemon's own configured PATH, then the system directories — never the agent's shell
/// environment.
pub fn tool_path() -> String {
    let mut parts = vec![];
    if let Some(home) = crate::userhome::user_home() {
        for language in crate::lang::registered() {
            for dir in language.descriptor().home_tool_dirs {
                parts.push(format!("{}/{dir}", home.display()));
            }
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        parts.push(path.to_string_lossy().into_owned());
    }
    parts.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"].map(String::from));
    parts.join(":")
}

/// One candidate-on-stdin run (a formatter or a syntax probe): the in-process language's
/// argument vector, or the run specification the core expanded from a module's request of one of
/// the language's declared recipes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StdinRun {
    /// Run as the in-process path always has (formatter PATH, daemon environment).
    Argv(Vec<String>),
    /// Run exactly this core-built specification.
    Spec(crate::checks::runner::RunSpec),
}

/// Expands a module's formatter or probe request under the core's interactive admission: the
/// worktree, the real home, the language's declared install roots, the environments the worktree
/// accepts and the home tools its recipes
/// name, resolved on [`tool_path`]. A request outside the declared recipes (the generic `argv`
/// one included) is refused, never run.
fn stdin_run(
    language: Language,
    worktree: &Path,
    effect: Option<EffectRequest>,
) -> Routed<Option<StdinRun>> {
    let Some(effect) = effect else {
        return Ok(None);
    };
    let recipes = super::recipe::declared(language.name());
    let path = tool_path();
    let programs: Vec<(String, PathBuf)> = recipes
        .iter()
        .flat_map(|recipe| recipe.executables.iter())
        .filter_map(|slot| match slot.source {
            super::payload::SlotSource::HomeTool(name) => Some((
                name.to_owned(),
                crate::execution::job::executable_on(name, worktree, &path)?,
            )),
            super::payload::SlotSource::Launcher(_) => None,
        })
        .collect();
    let mut roots = super::recipe::declared_roots(language.name());
    roots.extend(super::recipe::environment_roots(language.name(), worktree));
    let developer_dirs = super::recipe::platform_developer_dirs();
    let home = crate::userhome::user_home();
    let scratch = std::env::temp_dir().join("agent-ide-interactive");
    let admission = super::recipe::Admission {
        worktree,
        cache_dir: &scratch,
        read_denies: &[],
        home: home.as_deref(),
        launcher_roots: &roots,
        developer_dirs: &developer_dirs,
        programs: &programs,
        timeout: std::time::Duration::from_secs(10),
    };
    super::recipe::expand(recipes, &effect, &admission)
        .map(|spec| Some(StdinRun::Spec(spec)))
        .map_err(|_| ModuleUnavailable {
            module_id: super::contract::ModuleId::bundled(language.name()),
            module_version: env!("CARGO_PKG_VERSION").to_owned(),
            role: super::contract::Role::Analyzer,
            stage: super::contract::Stage::Decode,
            cause: super::contract::Cause::PolicyRefused,
            instance: None,
            retry_after_ms: None,
        })
}

/// `LanguageSupport::format_stdin_command`.
pub async fn format_stdin_command(
    language: Language,
    worktree: &Path,
    project: &LanguageProject,
    file: &Path,
) -> Routed<Option<StdinRun>> {
    match module(language) {
        None => Ok(language
            .support()
            .format_stdin_command(project, file)
            .map(StdinRun::Argv)),
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
            stdin_run(language, worktree, effect)
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
) -> Routed<Option<StdinRun>> {
    match module(language) {
        None => Ok(language
            .support()
            .syntax_probe_command(project, root, file, configured)
            .map(StdinRun::Argv)),
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
            stdin_run(language, worktree, effect)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Only an answer that computed every requested field is a cacheable fact: a warming or
    /// unsupported field (whose in-process default the caller substitutes) never is, nor is an
    /// anchor batch, however complete.
    #[test]
    fn only_complete_analyses_are_cached() {
        let analysis = |tests: Field<TestFacts>| SourceAnalysis {
            outline: Field::NotRequested,
            file_doc: Field::NotRequested,
            syntax: Field::NotRequested,
            tests,
            anchors: Field::NotRequested,
        };
        let facts = TestFacts {
            is_test_file: true,
            test_binary: None,
        };
        let fields = [SourceField::Tests];
        assert!(complete(&analysis(Field::Available(facts)), &fields));
        assert!(!complete(&analysis(Field::Warming), &fields));
        assert!(!complete(&analysis(Field::Unsupported), &fields));
        assert!(!complete(
            &analysis(Field::Available(TestFacts {
                is_test_file: false,
                test_binary: None,
            })),
            &[SourceField::Tests, SourceField::Syntax]
        ));
        let anchors = SourceAnalysis {
            anchors: Field::Available(AnchorBatch {
                verdict: FileVerdict::Indexed,
                capped: false,
                rejected: 0,
                coverage: Vec::new(),
                anchors: Vec::new(),
            }),
            ..analysis(Field::NotRequested)
        };
        assert!(!complete(&anchors, &[SourceField::Anchors]));
    }
}
