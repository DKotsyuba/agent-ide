//! The Rust language as a bundled `bundled-module/0` module (`bundled.rust`).
//!
//! The same sealed binary serves it in the hidden `agent-ide module rust <role>` mode. The
//! analyzer role answers every interactive Rust computation with the unchanged [`RustSupport`]
//! (project facts, lexical outline and syntax verdict, insertion geometry, test selection and
//! output parsing, formatter choice, module-graph scope) and with
//! rust-analyzer; the checker role plans and interprets `cargo check`. Every process a module
//! needs beyond its language server is an effect recipe the core expands and runs
//! ([`RECIPES`]); the module never spawns it and never writes.

use std::{path::Path, sync::atomic::AtomicBool, time::Duration};

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use agent_ide_core::{
    assistance::launcher::ProviderLaunch,
    checks::{
        BoxFuture, CheckConfig, CheckRequest, LanguageChecks, ProblemSnapshot, UnavailableReason,
        runner::{ConfinedRunner, RunOutput, RunSpec},
    },
    intelligence::server::LanguageServer,
    lang::LanguageSupport,
    modules::{
        contract::{
            Capability, CapabilityDecl, Cause, Declaration, ErrorCode, HelloOffer, ModuleId, Role,
            Support,
        },
        payload::{
            self, AnalysisScopeRequest, AnalyzeSource, Arg, CheckParseRequest, CheckPlanRequest,
            ChecksDescription, DeclaredExecutable, DescribeQuery, EffectOutcome, EffectRecipe,
            EffectRequest, EnvRule, ExecutableSlot, Field, FileDocRequest, FormatPlanRequest,
            InsertSiteRequest, LaunchDescription, NamedProgram, Param, PathRole, PathRule,
            ProjectQuery, RunClass, SlotSource, SourceAnalysis, SourceField, SourceRef, SourceText,
            Stdin, SyntaxQuery, TestFacts, TestParseRequest, TestPlanQuery,
        },
        serve::{Answer, Effects, Incoming, ModuleServer, ServeError},
        wire::Attachment,
    },
};

use crate::{
    backend::RustServer,
    checks::{CargoCheckPlan, ProjectRustChecksConfig, RustChecker, RustChecks, map_run_output},
    support::RustSupport,
};

/// Ancestor files a nested project check reads (relative to each ancestor directory).
const ANCESTOR_FILES: &[&str] = &["Cargo.toml", ".cargo/config.toml", ".cargo/config"];

/// The project check: `cargo check` with the pinned toolchain, a private target and the
/// linker-bypass environment. The core's expansion equals
/// [`RustChecker::cargo_check_spec`] field for field (see the equality test below).
pub const CARGO_CHECK: EffectRecipe = EffectRecipe {
    id: "cargo_check",
    program: "cargo",
    args: &[
        Arg::Literal("check"),
        Arg::Literal("--workspace"),
        Arg::Literal("--all-targets"),
        Arg::Literal("--message-format=json"),
        Arg::Literal("--offline"),
        Arg::Literal("--keep-going"),
        Arg::Literal("--locked"),
    ],
    env: &[
        EnvRule::SearchPath {
            name: "PATH",
            param: "toolchain_bin",
            fixed: &["/usr/bin", "/bin"],
        },
        EnvRule::Home { name: "HOME" },
        EnvRule::Param {
            name: "CARGO_HOME",
            param: "cargo_home",
            optional: false,
        },
        EnvRule::Param {
            name: "TMPDIR",
            param: "tmp",
            optional: false,
        },
        EnvRule::Param {
            name: "CARGO_TARGET_DIR",
            param: "target",
            optional: false,
        },
        EnvRule::Literal {
            name: "CARGO_NET_OFFLINE",
            value: "true",
        },
        EnvRule::Pattern {
            prefix: "CARGO_TARGET_",
            suffix: "_LINKER",
            param: "linker",
            roles: &[PathRole::DeveloperDir],
        },
        EnvRule::Joined {
            name: "RUSTFLAGS",
            prefix: "-Clinker=",
            param: "linker_flag",
        },
        EnvRule::Param {
            name: "CC",
            param: "cc",
            optional: true,
        },
        EnvRule::Param {
            name: "CXX",
            param: "cxx",
            optional: true,
        },
        EnvRule::Param {
            name: "AR",
            param: "ar",
            optional: true,
        },
        EnvRule::Param {
            name: "RANLIB",
            param: "ranlib",
            optional: true,
        },
        EnvRule::Param {
            name: "SDKROOT",
            param: "sdkroot",
            optional: true,
        },
    ],
    paths: &[
        PathRule {
            param: "worktree",
            roles: &[PathRole::WorktreeRoot],
            existing_only: false,
            read_root: true,
        },
        PathRule {
            param: "toolchain",
            roles: &[PathRole::LauncherRoot],
            existing_only: false,
            read_root: true,
        },
        PathRule {
            param: "cargo_home",
            roles: &[PathRole::LauncherRoot, PathRole::HomeRelative],
            existing_only: false,
            read_root: true,
        },
        PathRule {
            param: "rustup_home",
            roles: &[
                PathRole::LauncherRootAncestor {
                    stop_at: "toolchains",
                },
                PathRole::HomeRelative,
            ],
            existing_only: false,
            read_root: true,
        },
        PathRule {
            param: "etc",
            roles: &[PathRole::Fixed(&["/private/etc"])],
            existing_only: false,
            read_root: true,
        },
        PathRule {
            param: "developer",
            roles: &[PathRole::DeveloperDir],
            existing_only: false,
            read_root: true,
        },
        PathRule {
            param: "ancestors",
            roles: &[PathRole::AncestorFile(ANCESTOR_FILES)],
            existing_only: true,
            read_root: true,
        },
        PathRule {
            param: "git_exclude",
            roles: &[PathRole::HomeRelative],
            existing_only: true,
            read_root: true,
        },
        PathRule {
            param: "toolchain_bin",
            roles: &[PathRole::LauncherRoot],
            existing_only: false,
            read_root: false,
        },
        PathRule {
            param: "tmp",
            roles: &[PathRole::Cache],
            existing_only: false,
            read_root: false,
        },
        PathRule {
            param: "target",
            roles: &[PathRole::Cache],
            existing_only: false,
            read_root: false,
        },
        PathRule {
            param: "linker_flag",
            roles: &[PathRole::DeveloperDir],
            existing_only: false,
            read_root: false,
        },
        PathRule {
            param: "cc",
            roles: &[PathRole::DeveloperDir],
            existing_only: false,
            read_root: false,
        },
        PathRule {
            param: "cxx",
            roles: &[PathRole::DeveloperDir],
            existing_only: false,
            read_root: false,
        },
        PathRule {
            param: "ar",
            roles: &[PathRole::DeveloperDir],
            existing_only: false,
            read_root: false,
        },
        PathRule {
            param: "ranlib",
            roles: &[PathRole::DeveloperDir],
            existing_only: false,
            read_root: false,
        },
        PathRule {
            param: "sdkroot",
            roles: &[PathRole::DeveloperDir],
            existing_only: false,
            read_root: false,
        },
    ],
    executables: &[ExecutableSlot {
        name: "cargo",
        source: SlotSource::Launcher("cargo"),
    }],
    stdin: Stdin::Null,
    class: RunClass::Background,
    timeout_ceiling_ms: 900_000,
    capture_bytes: 64 << 20,
};

/// The formatter: `rustfmt --edition <edition>` over the core's candidate on stdin. Like every
/// interactive recipe it runs with the daemon's formatter `PATH` (home tool directories first).
pub const RUSTFMT: EffectRecipe = EffectRecipe {
    id: "rustfmt",
    program: "rustfmt",
    args: &[Arg::Literal("--edition"), Arg::Param("edition")],
    env: &[],
    paths: &[],
    executables: &[ExecutableSlot {
        name: "rustfmt",
        source: SlotSource::HomeTool("rustfmt"),
    }],
    stdin: Stdin::Candidate,
    class: RunClass::Interactive,
    timeout_ceiling_ms: 10_000,
    capture_bytes: 64 << 20,
};

/// Every effect recipe the Rust module may name; the root registers them with the descriptor.
pub const RECIPES: &[EffectRecipe] = &[CARGO_CHECK, RUSTFMT];

/// The module's identity.
pub fn module_id() -> ModuleId {
    ModuleId::bundled(crate::DESCRIPTOR.id)
}

impl CargoCheckPlan {
    /// The recipe request that makes the core run exactly [`RustChecker::cargo_check_spec`].
    pub(crate) fn to_effect(&self, request: &CheckRequest) -> EffectRequest {
        let path = |p: &Path| Param::Path(p.to_path_buf());
        let mut params = std::collections::BTreeMap::from([
            ("cargo".to_owned(), Param::Executable("cargo".to_owned())),
            ("worktree".to_owned(), path(&request.worktree)),
            ("toolchain".to_owned(), path(&self.toolchain_dir)),
            (
                "toolchain_bin".to_owned(),
                Param::Paths(vec![self.toolchain_dir.join("bin")]),
            ),
            ("cargo_home".to_owned(), path(&self.cargo_home)),
            ("rustup_home".to_owned(), path(&self.rustup_home)),
            ("etc".to_owned(), path(Path::new("/private/etc"))),
            (
                "developer".to_owned(),
                Param::Paths(self.developer_roots.clone()),
            ),
            ("ancestors".to_owned(), Param::Paths(self.ancestors.clone())),
            ("tmp".to_owned(), path(&request.cache_dir.join("tmp"))),
            ("target".to_owned(), path(&request.cache_dir.join("target"))),
        ]);
        if let Some(exclude) = &self.git_exclude {
            params.insert("git_exclude".to_owned(), path(exclude));
        }
        let mut linkers = std::collections::BTreeMap::new();
        for (name, value) in &self.linker_env {
            match name.as_str() {
                "RUSTFLAGS" => {
                    if let Some(clang) = value.strip_prefix("-Clinker=") {
                        params.insert("linker_flag".to_owned(), path(Path::new(clang)));
                    }
                }
                "CC" => {
                    params.insert("cc".to_owned(), path(Path::new(value)));
                }
                "CXX" => {
                    params.insert("cxx".to_owned(), path(Path::new(value)));
                }
                "AR" => {
                    params.insert("ar".to_owned(), path(Path::new(value)));
                }
                "RANLIB" => {
                    params.insert("ranlib".to_owned(), path(Path::new(value)));
                }
                "SDKROOT" => {
                    params.insert("sdkroot".to_owned(), path(Path::new(value)));
                }
                _ => {
                    linkers.insert(name.clone(), value.clone());
                }
            }
        }
        if !linkers.is_empty() {
            params.insert("linker".to_owned(), Param::Env(linkers));
        }
        EffectRequest {
            recipe: CARGO_CHECK.id.to_owned(),
            params,
        }
    }
}

/// A runner for the checker the module builds only to reuse its planning and parsing: every
/// process is an effect the core runs, so this one refuses to start anything.
struct NoProcess;

impl ConfinedRunner for NoProcess {
    /// Always fails; the module never spawns a check itself.
    fn run(&self, _spec: RunSpec) -> BoxFuture<'_, std::io::Result<RunOutput>> {
        Box::pin(async { Err(std::io::Error::other("the module runs no process itself")) })
    }
}

/// The module's state: its role and the configuration `hello` handed it.
pub struct RustModule {
    /// The role this instance serves.
    role: Role,
}

impl RustModule {
    /// A module serving `role`.
    pub fn new(role: Role) -> Self {
        Self { role }
    }

    /// The capabilities the role supports; every other capability is declared unsupported.
    fn supported(role: Role, capability: Capability) -> bool {
        use Capability::*;
        match role {
            Role::Analyzer => matches!(
                capability,
                Project
                    | AnalyzeSource
                    | FileDoc
                    | InsertSite
                    | Syntax
                    | FormatPlan
                    | TestPlan
                    | TestParse
                    | AnalysisScope
                    | Describe
            ),
            Role::Checker => matches!(
                capability,
                CheckPlan | CheckParse | AnalysisScope | Describe
            ),
        }
    }
}

/// Decodes a payload, or the refusal that names it.
fn request<T: DeserializeOwned>(incoming: &Incoming) -> Result<T, Answer> {
    payload::decode(incoming.payload.clone())
        .map_err(|error| Answer::error(ErrorCode::InvalidRequest, error))
}

/// A complete, ready result.
fn reply(value: &impl Serialize) -> Answer {
    Answer::result(payload::encode(value))
}

/// The UTF-8 text of `source`, inline or from its attachment.
fn text_of(incoming: &Incoming, source: &SourceRef) -> Result<String, Answer> {
    match &source.text {
        SourceText::Inline(text) => Ok(text.clone()),
        SourceText::Attachment(id) => incoming
            .attachment(*id)
            .and_then(|attachment| String::from_utf8(attachment.bytes.clone()).ok())
            .ok_or_else(|| Answer::error(ErrorCode::InvalidRequest, "source attachment missing")),
        SourceText::Missing => Err(Answer::error(
            ErrorCode::InvalidRequest,
            "source is missing",
        )),
    }
}

/// An attachment's bytes as lossy UTF-8 (runner output).
fn lossy(incoming: &Incoming, id: u32) -> Result<String, Answer> {
    incoming
        .attachment(id)
        .map(|attachment| String::from_utf8_lossy(&attachment.bytes).into_owned())
        .ok_or_else(|| Answer::error(ErrorCode::InvalidRequest, "output attachment missing"))
}

/// The checker for `config`, planning and parsing only.
fn checker_for(config: &ProjectRustChecksConfig, timeout: Duration) -> RustChecker {
    RustChecker::new(
        std::sync::Arc::new(NoProcess),
        config.toolchain_dir().to_path_buf(),
        config.cargo_home().map(Path::to_path_buf),
        timeout,
        config.developer_dir().map(Path::to_path_buf),
    )
}

impl RustModule {
    /// `project`.
    fn project(&self, incoming: &Incoming) -> Answer {
        let query: ProjectQuery = match request(incoming) {
            Ok(query) => query,
            Err(answer) => return answer,
        };
        let support = RustSupport;
        match query {
            ProjectQuery::Detect { root } => reply(&support.detect(&root)),
            ProjectQuery::Environments { worktree, .. } => reply(&support.environments(&worktree)),
            ProjectQuery::CheckSelection {
                worktree,
                root,
                selector,
            } => reply(&support.check_selection(&worktree, &root, &selector)),
            ProjectQuery::CommandEnv {
                worktree,
                cwd,
                program,
                ..
            } => reply(&support.command_env(&worktree, &cwd, &program)),
            ProjectQuery::TestToolchain { program } => reply(&support.test_toolchain(&program)),
        }
    }

    /// `analyze_source`: the batched, provider-free per-file computation.
    fn analyze_source(&self, incoming: &Incoming) -> Answer {
        let query: AnalyzeSource = match request(incoming) {
            Ok(query) => query,
            Err(answer) => return answer,
        };
        let text = match text_of(incoming, &query.source) {
            Ok(text) => text,
            Err(answer) => return answer,
        };
        let support = RustSupport;
        let path = query.source.path.as_path();
        let wants = |field| query.fields.contains(&field);
        let analysis = SourceAnalysis {
            outline: if wants(SourceField::Outline) {
                Field::Available(support.outline_from_source(path, &text))
            } else {
                Field::NotRequested
            },
            file_doc: if wants(SourceField::FileDoc) {
                Field::Available(support.file_doc(&text))
            } else {
                Field::NotRequested
            },
            syntax: if wants(SourceField::Syntax) {
                Field::Available(support.syntax_verdict(path, &text))
            } else {
                Field::NotRequested
            },
            tests: if wants(SourceField::Tests) {
                Field::Available(TestFacts {
                    is_test_file: support.is_test_file(path),
                    test_binary: support.test_binary(path),
                })
            } else {
                Field::NotRequested
            },
            anchors: if wants(SourceField::Anchors) {
                Field::Unsupported
            } else {
                Field::NotRequested
            },
        };
        reply(&analysis)
    }

    /// `syntax`.
    fn syntax(&self, incoming: &Incoming) -> Answer {
        let query: SyntaxQuery = match request(incoming) {
            Ok(query) => query,
            Err(answer) => return answer,
        };
        match query {
            SyntaxQuery::Verdict { source } => match text_of(incoming, &source) {
                Ok(text) => reply(&RustSupport.syntax_verdict(&source.path, &text)),
                Err(answer) => answer,
            },
            // Rust checks the structure itself; there is no external probe.
            SyntaxQuery::ProbePlan { .. } => reply(&Option::<EffectRequest>::None),
        }
    }

    /// `format_plan`: the rustfmt recipe, or none.
    fn format_plan(&self, incoming: &Incoming) -> Answer {
        let query: FormatPlanRequest = match request(incoming) {
            Ok(query) => query,
            Err(answer) => return answer,
        };
        let plan = RustSupport
            .format_stdin_command(&query.project, &query.file)
            .and_then(|argv| {
                // `rustfmt --edition <edition>`: the edition is the third token.
                let edition = argv.get(2)?.clone();
                Some(EffectRequest {
                    recipe: RUSTFMT.id.to_owned(),
                    params: [
                        (
                            "rustfmt".to_owned(),
                            Param::Executable("rustfmt".to_owned()),
                        ),
                        ("edition".to_owned(), Param::Token(edition)),
                    ]
                    .into(),
                })
            });
        reply(&plan)
    }

    /// `test_plan`.
    fn test_plan(&self, incoming: &Incoming) -> Answer {
        let query: TestPlanQuery = match request(incoming) {
            Ok(query) => query,
            Err(answer) => return answer,
        };
        let support = RustSupport;
        match query {
            TestPlanQuery::Selection { project, target } => {
                reply(&support.test_selection(&project, &target))
            }
            TestPlanQuery::TestIds {
                file,
                outline_paths,
            } => reply(
                &outline_paths
                    .iter()
                    .map(|path| support.test_id(&file, path))
                    .collect::<Vec<_>>(),
            ),
        }
    }

    /// `describe`: launcher-time interpretation, with no session.
    fn describe(&self, incoming: &Incoming) -> Answer {
        let query: DescribeQuery = match request(incoming) {
            Ok(query) => query,
            Err(answer) => return answer,
        };
        match query {
            DescribeQuery::Provider { declaration } => reply(&describe_provider(declaration)),
            DescribeQuery::VerifyProvider { declaration } => reply(&verify_provider(declaration)),
            DescribeQuery::Checks { section } => reply(&describe_checks(section)),
            DescribeQuery::Presence { worktree } => reply(&RustChecks.is_present(&worktree)),
        }
    }
}

/// Decodes and validates one provider declaration.
fn describe_provider(declaration: Value) -> Result<LaunchDescription, String> {
    let launch: ProviderLaunch =
        serde_json::from_value(declaration).map_err(|error| error.to_string())?;
    let server = RustServer;
    Ok(LaunchDescription {
        valid: server.validate_launch(&launch),
        executables: server
            .launch_executables(&launch)
            .into_iter()
            .map(|executable| DeclaredExecutable {
                path: executable.path.clone(),
                identity: executable.identity.clone(),
                blake3: executable.blake3.clone(),
            })
            .collect(),
        toolchain_programs: server.toolchain_programs(&launch),
        probe_programs: server.probe_programs(&launch),
    })
}

/// Further startup verification of one declaration (nothing beyond the executables for Rust).
fn verify_provider(declaration: Value) -> Result<(), String> {
    let launch: ProviderLaunch =
        serde_json::from_value(declaration).map_err(|error| error.to_string())?;
    RustServer
        .verify_launch(&launch, &AtomicBool::new(false))
        .map_err(|error| format!("{error:?}"))
}

/// Decodes and validates the `project_checks.rust` section and names what the core admits from it.
fn describe_checks(section: Value) -> Result<ChecksDescription, String> {
    let config: ProjectRustChecksConfig =
        serde_json::from_value(section).map_err(|error| error.to_string())?;
    let mut launcher_roots = vec![config.toolchain_dir().to_path_buf()];
    launcher_roots.extend(config.cargo_home().map(Path::to_path_buf));
    Ok(ChecksDescription {
        valid: config.validate(),
        programs: vec![NamedProgram {
            name: "cargo".to_owned(),
            path: config.toolchain_dir().join("bin").join("cargo"),
            interpreter: None,
        }],
        launcher_roots,
        developer_dirs: config
            .developer_dir()
            .map(Path::to_path_buf)
            .into_iter()
            .collect(),
    })
}

impl RustModule {
    /// `analysis_scope`: why the project check does not analyse `path`.
    fn analysis_scope(&self, incoming: &Incoming) -> Answer {
        let query: AnalysisScopeRequest = match request(incoming) {
            Ok(query) => query,
            Err(answer) => return answer,
        };
        reply(
            &RustChecks
                .not_analysed(&query.worktree, &query.path)
                .map(str::to_owned),
        )
    }

    /// `check_plan`: plan one `cargo check`, let the core run it, interpret its output.
    async fn check_plan(
        &self,
        incoming: &Incoming,
        effects: &mut Effects<'_>,
    ) -> Result<Answer, ServeError> {
        let query: CheckPlanRequest = match request(incoming) {
            Ok(query) => query,
            Err(answer) => return Ok(answer),
        };
        let config: ProjectRustChecksConfig = match serde_json::from_value(query.config.clone()) {
            Ok(config) => config,
            Err(error) => {
                return Ok(Answer::error(ErrorCode::InvalidRequest, error.to_string()));
            }
        };
        let checker = checker_for(&config, Duration::from_millis(query.timeout_ms));
        let started = std::time::Instant::now();
        if checker.cargo_missing(&query.request) {
            return Ok(reply(&ProblemSnapshot::unavailable(
                crate::LANGUAGE,
                UnavailableReason::ToolMissing,
                query.request.input_generation,
            )));
        }
        let effect = checker
            .cargo_check_plan(&query.request)
            .to_effect(&query.request);
        let (outcome, attachments) = effects.run(effect).await?;
        Ok(reply(&interpret(
            &query.request,
            &outcome,
            &attachments,
            started.elapsed().as_millis() as u64,
        )))
    }

    /// `check_parse`: interpret one completed run the core performed.
    fn check_parse(&self, incoming: &Incoming) -> Answer {
        let query: CheckParseRequest = match request(incoming) {
            Ok(query) => query,
            Err(answer) => return answer,
        };
        let (Some(stdout), Some(stderr)) = (
            incoming.attachment(query.stdout),
            incoming.attachment(query.stderr),
        ) else {
            return Answer::error(ErrorCode::InvalidRequest, "output attachment missing");
        };
        let attachments = [stdout.clone(), stderr.clone()];
        reply(&interpret(&query.request, &query.outcome, &attachments, 0))
    }
}

/// Maps one run outcome and its output attachments (stdout first, stderr second) to a snapshot.
fn interpret(
    request: &CheckRequest,
    outcome: &EffectOutcome,
    attachments: &[Attachment],
    duration_ms: u64,
) -> ProblemSnapshot {
    match outcome {
        EffectOutcome::Completed {
            status,
            timed_out,
            truncated,
            ..
        } => {
            let bytes = |position: usize| {
                attachments
                    .get(position)
                    .map(|attachment| attachment.bytes.clone())
                    .unwrap_or_default()
            };
            map_run_output(
                request,
                &RunOutput {
                    status: *status,
                    stdout: bytes(0),
                    stderr: bytes(1),
                    timed_out: *timed_out,
                    truncated: *truncated,
                },
                duration_ms,
            )
        }
        EffectOutcome::Refused { cause, message } => {
            let reason = match cause {
                Cause::ToolMissing => UnavailableReason::ToolMissing,
                _ => UnavailableReason::Fatal,
            };
            ProblemSnapshot::unavailable_with_detail(
                crate::LANGUAGE,
                reason,
                request.input_generation,
                duration_ms,
                Some(agent_ide_core::checks::truncate_bytes(
                    message,
                    agent_ide_core::checks::MAX_CAUSE_BYTES,
                )),
            )
        }
    }
}

impl ModuleServer for RustModule {
    /// Every capability declared; the role decides which are supported.
    fn declaration(&self) -> Declaration {
        Declaration {
            module_id: module_id(),
            package_version: env!("CARGO_PKG_VERSION").to_owned(),
            capabilities: Capability::ALL
                .into_iter()
                .map(|capability| {
                    CapabilityDecl::v0(
                        capability,
                        if Self::supported(self.role, capability) {
                            Support::Supported
                        } else {
                            Support::Unsupported
                        },
                    )
                })
                .collect(),
            linkage_kinds: Vec::new(),
        }
    }

    /// Accepts the offer; the role was checked against the declaration by the serve loop.
    async fn hello(&mut self, _offer: &HelloOffer) -> Result<(), String> {
        Ok(())
    }

    /// Answers one request.
    async fn call<'a>(
        &'a mut self,
        incoming: Incoming,
        mut effects: Effects<'a>,
    ) -> Result<Answer, ServeError> {
        Ok(match incoming.capability {
            Capability::Project => self.project(&incoming),
            Capability::AnalyzeSource => self.analyze_source(&incoming),
            Capability::FileDoc => match request::<FileDocRequest>(&incoming) {
                Ok(query) => match text_of(&incoming, &query.source) {
                    Ok(text) => reply(&RustSupport.file_doc(&text)),
                    Err(answer) => answer,
                },
                Err(answer) => answer,
            },
            Capability::InsertSite => match request::<InsertSiteRequest>(&incoming) {
                Ok(query) => match text_of(&incoming, &query.source) {
                    Ok(text) => reply(&RustSupport.insert_site(
                        &text,
                        &query.outline,
                        &query.anchor,
                        query.placement,
                    )),
                    Err(answer) => answer,
                },
                Err(answer) => answer,
            },
            Capability::Syntax => self.syntax(&incoming),
            Capability::FormatPlan => self.format_plan(&incoming),
            Capability::TestPlan => self.test_plan(&incoming),
            Capability::TestParse => match request::<TestParseRequest>(&incoming) {
                Ok(query) => match (
                    lossy(&incoming, query.stdout),
                    lossy(&incoming, query.stderr),
                ) {
                    (Ok(stdout), Ok(stderr)) => {
                        reply(&RustSupport.parse_test_output(&stdout, &stderr))
                    }
                    (Err(answer), _) | (_, Err(answer)) => answer,
                },
                Err(answer) => answer,
            },
            Capability::AnalysisScope => self.analysis_scope(&incoming),
            Capability::Describe => self.describe(&incoming),
            Capability::CheckPlan => self.check_plan(&incoming, &mut effects).await?,
            Capability::CheckParse => self.check_parse(&incoming),
            _ => Answer::error(ErrorCode::Unsupported, "capability not supported"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ide_core::{
        checks::CheckState,
        modules::{
            contract::Outcome,
            fake::{FakeEffects, in_memory, offer},
            host::{Call, HostChannel, NoEffects},
            payload::decode,
            recipe::{Described, expand},
        },
    };
    use serde_json::json;
    use std::path::PathBuf;

    /// Registers Rust, the only language these tests name.
    fn install() {
        agent_ide_core::lang::install(&[crate::LANGUAGE]);
    }

    /// A scratch directory below the canonical temporary directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        /// Creates `tag` fresh.
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("rust-module-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        /// Removes the tree.
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Opens a module of `role` over the in-memory fake transport.
    async fn open(role: Role) -> HostChannel {
        install();
        in_memory(
            RustModule::new(role),
            offer(module_id(), env!("CARGO_PKG_VERSION"), role, 1),
        )
        .await
        .unwrap()
        .0
    }

    /// One call of `capability` with `payload`, and the decoded result.
    async fn ask<T: DeserializeOwned>(
        channel: &mut HostChannel,
        capability: Capability,
        payload: Value,
        attachments: Vec<Attachment>,
    ) -> T {
        let reply = channel
            .call(
                Call {
                    capability,
                    scope_key: "scope".into(),
                    revision_key: "revision".into(),
                    payload,
                    attachments,
                },
                Duration::from_secs(10),
                &mut NoEffects,
            )
            .await
            .unwrap();
        match reply.outcome {
            Outcome::Result(value) => decode(value).unwrap(),
            Outcome::Error(error) => panic!("{capability:?}: {error:?}"),
        }
    }

    /// The role decides what is supported; everything else is declared unsupported.
    #[tokio::test]
    async fn roles_declare_their_capabilities() {
        install();
        for (role, supported, unsupported) in [
            (
                Role::Analyzer,
                Capability::AnalyzeSource,
                Capability::CheckPlan,
            ),
            (
                Role::Checker,
                Capability::CheckPlan,
                Capability::AnalyzeSource,
            ),
        ] {
            let (_, reply) = in_memory(
                RustModule::new(role),
                offer(module_id(), env!("CARGO_PKG_VERSION"), role, 1),
            )
            .await
            .unwrap();
            let support = |capability| {
                reply
                    .capabilities
                    .iter()
                    .find(|decl| decl.capability == capability)
                    .map(|decl| decl.support)
            };
            assert_eq!(support(supported), Some(Support::Supported), "{role:?}");
            assert_eq!(support(unsupported), Some(Support::Unsupported), "{role:?}");
            assert_eq!(support(Capability::Linkage), Some(Support::Unsupported));
            assert!(reply.linkage_kinds.is_empty());
        }
    }

    /// The batched source analysis equals the unchanged in-process answers, field by field.
    #[tokio::test]
    async fn analyze_source_equals_the_in_process_answers() {
        let text = "//! Crate docs.\n\npub struct S;\n\nimpl S {\n    pub fn f(&self) {}\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n";
        let mut channel = open(Role::Analyzer).await;
        let analysis: SourceAnalysis = ask(
            &mut channel,
            Capability::AnalyzeSource,
            payload::encode(&AnalyzeSource {
                source: SourceRef {
                    path: "src/lib.rs".into(),
                    revision: "r1".into(),
                    text: SourceText::Inline(text.into()),
                },
                fields: vec![
                    SourceField::Outline,
                    SourceField::FileDoc,
                    SourceField::Syntax,
                    SourceField::Tests,
                    SourceField::Anchors,
                ],
            }),
            Vec::new(),
        )
        .await;
        let support = RustSupport;
        let path = Path::new("src/lib.rs");
        let outline = support.outline_from_source(path, text);
        assert!(
            outline.is_some(),
            "the lexical outline is the loading fallback"
        );
        assert_eq!(analysis.outline, Field::Available(outline));
        assert_eq!(analysis.file_doc, Field::Available(support.file_doc(text)));
        assert_eq!(
            analysis.syntax,
            Field::Available(support.syntax_verdict(path, text))
        );
        assert_eq!(
            analysis.tests,
            Field::Available(TestFacts {
                is_test_file: support.is_test_file(path),
                test_binary: support.test_binary(path),
            })
        );
        assert_eq!(analysis.anchors, Field::Unsupported);
    }

    /// The formatter plan is the rustfmt recipe with the project's edition, or nothing.
    #[tokio::test]
    async fn format_plan_names_the_rustfmt_recipe() {
        let dir = Scratch::new("format");
        std::fs::write(
            dir.0.join("Cargo.toml"),
            "[package]\nname = \"x\"\nedition = \"2024\"\n",
        )
        .unwrap();
        install();
        let project = RustSupport.detect(&dir.0).expect("a Cargo project");
        let mut channel = open(Role::Analyzer).await;
        let plan: Option<EffectRequest> = ask(
            &mut channel,
            Capability::FormatPlan,
            payload::encode(&FormatPlanRequest {
                project: project.clone(),
                file: "src/lib.rs".into(),
            }),
            Vec::new(),
        )
        .await;
        let plan = plan.expect("rustfmt applies to a Cargo project");
        assert_eq!(plan.recipe, "rustfmt");
        assert_eq!(plan.params["edition"], Param::Token("2024".into()));
        assert_eq!(plan.params["rustfmt"], Param::Executable("rustfmt".into()));
        let none: Option<EffectRequest> = ask(
            &mut channel,
            Capability::FormatPlan,
            payload::encode(&FormatPlanRequest {
                project,
                file: "notes.txt".into(),
            }),
            Vec::new(),
        )
        .await;
        assert_eq!(none, None);
    }

    /// A fake developer directory with a clang and its sibling tools.
    fn developer(base: &Path) -> PathBuf {
        let dir = base.join("developer");
        std::fs::create_dir_all(dir.join("usr/bin")).unwrap();
        for tool in ["clang", "clang++", "ar", "ranlib"] {
            std::fs::write(dir.join("usr/bin").join(tool), "").unwrap();
        }
        std::fs::create_dir_all(dir.join("SDKs/MacOSX.sdk")).unwrap();
        dir
    }

    /// The core's expansion of the module's recipe request equals today's in-process run
    /// specification field for field, with and without a derivable target triple.
    #[test]
    fn cargo_check_expansion_equals_the_in_process_specification() {
        install();
        for toolchain_name in ["stable-aarch64-apple-darwin", "tc"] {
            let base = Scratch::new(&format!("equal-{toolchain_name}"));
            let worktree = base.0.join("outer/ws");
            let cache = base.0.join("cache");
            let toolchain = base.0.join("toolchains").join(toolchain_name);
            std::fs::create_dir_all(&worktree).unwrap();
            std::fs::create_dir_all(toolchain.join("bin")).unwrap();
            std::fs::write(toolchain.join("bin/cargo"), "").unwrap();
            std::fs::write(base.0.join("outer/Cargo.toml"), "").unwrap();
            let developer = developer(&base.0);
            let config = json!({
                "toolchain_dir": toolchain,
                "developer_dir": developer,
            });
            let section: ProjectRustChecksConfig = serde_json::from_value(config.clone()).unwrap();
            let checker = checker_for(&section, Duration::from_secs(300));
            let request = CheckRequest {
                worktree: worktree.clone(),
                cache_dir: cache.clone(),
                input_generation: 3,
                read_denies: Vec::new(),
            };
            let plan = checker.cargo_check_plan(&request);
            let effect = plan.to_effect(&request);
            let described = Described::new(&describe_checks(config).unwrap());
            let home = crate::home::real_home();
            let mut admission = described.admission(
                &worktree,
                &cache,
                &[],
                Some(&home),
                Duration::from_secs(300),
            );
            let mut platform = described.developer_dirs.clone();
            platform.extend(
                [
                    "/private/var/db/xcode_select_link",
                    "/Library/Developer/CommandLineTools",
                ]
                .map(PathBuf::from)
                .into_iter()
                .filter(|dir| dir.exists()),
            );
            admission.developer_dirs = &platform;
            assert_eq!(
                expand(RECIPES, &effect, &admission),
                Ok(checker.cargo_check_spec(&request)),
                "{toolchain_name}"
            );
        }
    }

    /// The checker plans one `cargo_check` effect and interprets the core's captured output; a
    /// refused run is an unavailable snapshot, never a clean one.
    #[tokio::test]
    async fn check_plan_runs_one_effect_and_parses_its_output() {
        install();
        let base = Scratch::new("plan");
        let worktree = base.0.join("ws");
        let toolchain = base.0.join("toolchains/stable-aarch64-apple-darwin");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(toolchain.join("bin")).unwrap();
        std::fs::write(toolchain.join("bin/cargo"), "").unwrap();
        let (mut channel, _) = in_memory(
            RustModule::new(Role::Checker),
            offer(module_id(), env!("CARGO_PKG_VERSION"), Role::Checker, 1),
        )
        .await
        .unwrap();
        let request = CheckRequest {
            worktree,
            cache_dir: base.0.join("cache"),
            input_generation: 9,
            read_denies: Vec::new(),
        };
        let payload = payload::encode(&CheckPlanRequest {
            request: request.clone(),
            config: json!({"toolchain_dir": toolchain}),
            timeout_ms: 60_000,
        });
        let mut effects = FakeEffects {
            stdout: br#"{"reason":"build-finished","success":true}
"#
            .to_vec(),
            runs: 0,
            last: None,
        };
        let reply = channel
            .call(
                Call {
                    capability: Capability::CheckPlan,
                    scope_key: "scope".into(),
                    revision_key: "revision".into(),
                    payload,
                    attachments: Vec::new(),
                },
                Duration::from_secs(10),
                &mut effects,
            )
            .await
            .unwrap();
        let Outcome::Result(value) = reply.outcome else {
            panic!("{:?}", reply.outcome)
        };
        let snapshot: ProblemSnapshot = decode(value).unwrap();
        assert_eq!(snapshot.state, CheckState::Ready);
        assert_eq!(snapshot.input_generation, 9);
        assert_eq!(effects.runs, 1);
        assert_eq!(effects.last.unwrap().recipe, "cargo_check");
    }

    /// Without the pinned cargo the check is `tool_missing` before any effect is asked for.
    #[tokio::test]
    async fn a_missing_cargo_asks_for_no_effect() {
        install();
        let base = Scratch::new("missing");
        let (mut channel, _) = in_memory(
            RustModule::new(Role::Checker),
            offer(module_id(), env!("CARGO_PKG_VERSION"), Role::Checker, 1),
        )
        .await
        .unwrap();
        let payload = payload::encode(&CheckPlanRequest {
            request: CheckRequest {
                worktree: base.0.clone(),
                cache_dir: base.0.join("cache"),
                input_generation: 1,
                read_denies: Vec::new(),
            },
            config: json!({"toolchain_dir": base.0.join("none")}),
            timeout_ms: 60_000,
        });
        let mut effects = FakeEffects {
            stdout: Vec::new(),
            runs: 0,
            last: None,
        };
        let reply = channel
            .call(
                Call {
                    capability: Capability::CheckPlan,
                    scope_key: "s".into(),
                    revision_key: "r".into(),
                    payload,
                    attachments: Vec::new(),
                },
                Duration::from_secs(10),
                &mut effects,
            )
            .await
            .unwrap();
        let Outcome::Result(value) = reply.outcome else {
            panic!("{:?}", reply.outcome)
        };
        let snapshot: ProblemSnapshot = decode(value).unwrap();
        assert_eq!(
            snapshot.state,
            CheckState::Unavailable(UnavailableReason::ToolMissing)
        );
        assert_eq!(effects.runs, 0);
    }

    /// Describe interprets the launcher section and presence without a session.
    #[tokio::test]
    async fn describe_interprets_the_launcher_section() {
        let base = Scratch::new("describe");
        std::fs::write(base.0.join("Cargo.toml"), "").unwrap();
        let mut channel = open(Role::Analyzer).await;
        let checks: Result<ChecksDescription, String> = ask(
            &mut channel,
            Capability::Describe,
            payload::encode(&DescribeQuery::Checks {
                section: json!({"toolchain_dir": "/t/tc", "cargo_home": "/h/.cargo"}),
            }),
            Vec::new(),
        )
        .await;
        let checks = checks.unwrap();
        assert!(checks.valid);
        assert_eq!(checks.programs[0].path, PathBuf::from("/t/tc/bin/cargo"));
        assert_eq!(
            checks.launcher_roots,
            [PathBuf::from("/t/tc"), PathBuf::from("/h/.cargo")]
        );
        let relative: Result<ChecksDescription, String> = ask(
            &mut channel,
            Capability::Describe,
            payload::encode(&DescribeQuery::Checks {
                section: json!({"toolchain_dir": "t/tc"}),
            }),
            Vec::new(),
        )
        .await;
        assert!(!relative.unwrap().valid);
        let unknown: Result<ChecksDescription, String> = ask(
            &mut channel,
            Capability::Describe,
            payload::encode(&DescribeQuery::Checks {
                section: json!({"toolchain_dir": "/t", "surprise": 1}),
            }),
            Vec::new(),
        )
        .await;
        assert!(unknown.is_err());
        let present: bool = ask(
            &mut channel,
            Capability::Describe,
            payload::encode(&DescribeQuery::Presence {
                worktree: base.0.clone(),
            }),
            Vec::new(),
        )
        .await;
        assert!(present);
    }
}
