//! The Rust language as a bundled `bundled-module/0` module (`bundled.rust`).
//!
//! The same sealed binary serves it in the hidden `agent-ide module rust <role>` mode. The
//! analyzer role answers every interactive Rust computation with the unchanged [`RustSupport`]
//! (project facts, lexical outline and syntax verdict, insertion geometry, test selection and
//! output parsing, formatter choice, module-graph scope) and with
//! rust-analyzer; the checker role plans and interprets `cargo check`. Every process a module
//! needs beyond its language server is an effect recipe the core expands and runs
//! ([`RECIPES`]); the module never spawns it and never writes.

use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Duration,
};

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use agent_ide_core::{
    assistance::launcher::ProviderLaunch,
    checks::{
        BoxFuture, CheckConfig, CheckRequest, LanguageChecks, ProblemSnapshot, UnavailableReason,
        runner::{ConfinedRunner, RunOutput, RunSpec},
    },
    intelligence::{server::LanguageServer, session::ProviderSettings},
    modules::{
        contract::{
            Capability, CapabilityDecl, Cause, Declaration, ErrorCode, HelloOffer, ModuleId, Role,
            Support,
        },
        payload::{
            self, Arg, CheckParseRequest, CheckPlanRequest, ChecksDescription, DeclaredExecutable,
            DescribeQuery, EffectOutcome, EffectRecipe, EffectRequest, EnvRule, ExecutableSlot,
            LaunchDescription, NamedProgram, Param, PathRole, PathRule, RunClass, SlotSource,
            Stdin,
        },
        provider::{ProviderBuilder, ProviderLaunchPlan},
        serve::{Answer, Effects, Incoming, ModuleServer, ServeError},
        wire::Attachment,
    },
};

use crate::{
    backend::{RustLaunchOptions, RustServer},
    checks::{CargoCheckPlan, ProjectRustChecksConfig, RustChecker, RustChecks, map_run_output},
    profile::{RustProfile, RustProfileError, RustProfileIdentity},
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
    assets: &[],
};

/// The platform's selected developer directory (`/usr/bin/xcode-select -p`), the one finite probe
/// the project check needs: the module starts no process itself, so the core runs it. The
/// daemon's `DEVELOPER_DIR`, which `xcode-select` honours, travels as the module environment.
pub const XCODE_SELECT: EffectRecipe = EffectRecipe {
    id: "xcode_select",
    program: "tool",
    args: &[Arg::Literal("-p")],
    env: &[EnvRule::Param {
        name: "DEVELOPER_DIR",
        param: "developer_dir",
        optional: true,
    }],
    paths: &[
        PathRule {
            param: "tool",
            roles: &[PathRole::Fixed(&["/usr/bin/xcode-select"])],
            existing_only: false,
            read_root: false,
        },
        PathRule {
            param: "developer_dir",
            roles: &[PathRole::DeveloperDir],
            existing_only: false,
            read_root: true,
        },
    ],
    executables: &[],
    stdin: Stdin::Null,
    class: RunClass::Background,
    timeout_ceiling_ms: 10_000,
    capture_bytes: 4096,
    assets: &[],
};

/// rustfmt formatting the candidate on stdin with the project's edition, the home tool the core
/// finds on its formatter PATH exactly as for the in-process formatter.
pub const RUSTFMT: EffectRecipe = EffectRecipe {
    id: "rustfmt",
    program: "rustfmt",
    args: &[Arg::Literal("--edition"), Arg::Param("edition")],
    env: &[
        EnvRule::Home { name: "HOME" },
        EnvRule::Literal {
            name: "PATH",
            value: "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin",
        },
    ],
    paths: &[],
    executables: &[ExecutableSlot {
        name: "rustfmt",
        source: SlotSource::HomeTool("rustfmt"),
    }],
    stdin: Stdin::Candidate,
    class: RunClass::Interactive,
    timeout_ceiling_ms: 10_000,
    capture_bytes: 64 << 20,
    assets: &[],
};

/// Every effect recipe the Rust module may name; the root registers them with the descriptor.
pub const RECIPES: &[EffectRecipe] = &[CARGO_CHECK, XCODE_SELECT, RUSTFMT];

/// The daemon variables the Rust module receives (root composition data): the test toolchain
/// and the developer-directory selection `xcode-select` honours.
pub const MODULE_ENV: [&str; 2] = ["AGENT_IDE_RUST_TOOLCHAIN_DIR", "DEVELOPER_DIR"];

/// The recipe request of the in-process formatter's argument vector (`rustfmt --edition <e>`);
/// `None` for any other shape, which the core then refuses.
pub fn interactive_effect(argv: &[String]) -> Option<EffectRequest> {
    let [program, flag, edition] = argv else {
        return None;
    };
    if program != "rustfmt" || flag != "--edition" {
        return None;
    }
    Some(EffectRequest {
        recipe: RUSTFMT.id.to_owned(),
        params: std::collections::BTreeMap::from([
            (
                "rustfmt".to_owned(),
                Param::Executable("rustfmt".to_owned()),
            ),
            ("edition".to_owned(), Param::Token(edition.clone())),
        ]),
    })
}

/// Serves `agent-ide module rust <role>` over this process's stdin and stdout until the core
/// closes it: the analyzer hosts rust-analyzer beside the language's own support, the checker
/// plans and interprets `cargo check`.
pub async fn serve(role: Role) -> Result<(), ServeError> {
    use agent_ide_core::modules::serve::serve_stdio;
    match role {
        Role::Analyzer => serve_stdio(RustModule::new(role, provider_server()), role).await,
        Role::Checker => serve_stdio(RustModule::new(role, support_server()), role).await,
    }
}

/// Rust's own support served through the module, its formatter plans as [`RUSTFMT`] requests.
fn support_server() -> agent_ide_core::modules::adapter::SupportServer {
    agent_ide_core::modules::adapter::SupportServer::new(crate::LANGUAGE, env!("CARGO_PKG_VERSION"))
        .with_effect_plans(interactive_effect)
}

/// The analyzer's server: the support beside the rust-analyzer session the core grants.
fn provider_server() -> agent_ide_core::modules::provider::ProviderServer<RustBuilder> {
    agent_ide_core::modules::provider::ProviderServer::new(support_server(), RustBuilder)
}

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

/// The bytes of attachment `id`, empty when the run captured none.
fn stream(attachments: &[Attachment], id: u32) -> Vec<u8> {
    attachments
        .iter()
        .find(|attachment| attachment.id == id)
        .map(|attachment| attachment.bytes.clone())
        .unwrap_or_default()
}

/// Maps one run outcome and its captured `stdout`/`stderr` bytes to a snapshot.
fn interpret(
    request: &CheckRequest,
    outcome: &EffectOutcome,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    duration_ms: u64,
) -> ProblemSnapshot {
    match outcome {
        EffectOutcome::Completed {
            status,
            timed_out,
            truncated,
            ..
        } => map_run_output(
            request,
            &RunOutput {
                status: *status,
                stdout,
                stderr,
                timed_out: *timed_out,
                truncated: *truncated,
            },
            duration_ms,
        ),
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

/// Decodes a payload, or the refusal that names it.
fn request<T: DeserializeOwned>(incoming: &Incoming) -> Result<T, Answer> {
    payload::decode(incoming.payload.clone())
        .map_err(|error| Answer::error(ErrorCode::InvalidRequest, error))
}

/// A complete, ready result.
fn reply(value: &impl Serialize) -> Answer {
    Answer::result(payload::encode(value))
}

/// The primary Apple developer directory, selected as the in-process checker does: the section's
/// override when it exists, else the platform selection (`xcode-select -p`, run by the core as a
/// finite probe effect), else the standard install locations.
async fn developer_dir(
    config: &ProjectRustChecksConfig,
    effects: &mut Effects<'_>,
) -> Result<Option<PathBuf>, ServeError> {
    if let Some(dir) = config.developer_dir().filter(|dir| dir.is_dir()) {
        return Ok(Some(dir.to_path_buf()));
    }
    let mut params = std::collections::BTreeMap::from([(
        "tool".to_owned(),
        Param::Path(PathBuf::from("/usr/bin/xcode-select")),
    )]);
    if let Some(dir) = std::env::var_os("DEVELOPER_DIR") {
        params.insert("developer_dir".to_owned(), Param::Path(dir.into()));
    }
    let probe = EffectRequest {
        recipe: XCODE_SELECT.id.to_owned(),
        params,
    };
    let (outcome, attachments) = effects.run(probe).await?;
    let selected = match outcome {
        EffectOutcome::Completed {
            status: Some(0), ..
        } => String::from_utf8(stream(&attachments, 1))
            .ok()
            .map(|text| PathBuf::from(text.trim()))
            .filter(|path| path.is_dir()),
        _ => None,
    };
    Ok(selected.or_else(crate::checks::standard_developer_dir))
}

/// The checker for `config`, planning and parsing only, around the resolved `primary` developer
/// directory.
fn checker_for(
    config: &ProjectRustChecksConfig,
    timeout: Duration,
    primary: Option<PathBuf>,
) -> RustChecker {
    RustChecker::for_planning(
        std::sync::Arc::new(NoProcess),
        config.toolchain_dir().to_path_buf(),
        config.cargo_home().map(Path::to_path_buf),
        timeout,
        primary,
    )
}

/// What the core hands the analyzer in `hello.config.provider.settings`, and what the module
/// rebuilds the same [`RustProfile`] from: both sides derive the session settings and the
/// analyzer's environment from this one value, so they cannot disagree.
#[derive(Clone, Debug, Serialize, serde::Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RustProviderSettings {
    /// The rust-analyzer executable.
    pub binary: PathBuf,
    /// Its accepted identity.
    pub version: String,
    /// The declared `cargo`.
    pub cargo: PathBuf,
    /// The operator-declared Cargo home, when any.
    pub cargo_home: Option<PathBuf>,
    /// Its accepted identity.
    pub cargo_version: String,
    /// The declared `rustc`.
    pub rustc: PathBuf,
    /// Its accepted identity.
    pub rustc_version: String,
    /// The rustup toolchain selector.
    pub toolchain: String,
    /// The operator trust identity.
    pub trust: String,
    /// The core-granted private provider namespace.
    pub cache_namespace: String,
}

impl RustProviderSettings {
    /// The settings of a validated declaration in the namespace the core allocated.
    pub fn from_launch(launch: &ProviderLaunch, cache_namespace: &str) -> Option<Self> {
        let options = launch.options::<RustLaunchOptions>()?;
        Some(Self {
            binary: launch.executable.path.clone(),
            version: launch.executable.identity.clone(),
            cargo: options.cargo.as_ref()?.path.clone(),
            cargo_home: options.cargo_home.clone(),
            cargo_version: options.cargo_version.clone()?,
            rustc: options.rustc.as_ref()?.path.clone(),
            rustc_version: options.rustc_version.clone()?,
            toolchain: launch.toolchain.clone(),
            trust: launch.trust.clone(),
            cache_namespace: cache_namespace.to_owned(),
        })
    }

    /// The immutable analyzer profile of these settings.
    pub fn profile(&self) -> Result<RustProfile, RustProfileError> {
        RustProfile::new(RustProfileIdentity {
            binary: self.binary.clone(),
            rust_analyzer_version: self.version.clone(),
            cargo: self.cargo.clone(),
            cargo_home: self.cargo_home.clone(),
            cargo_version: self.cargo_version.clone(),
            rustc: self.rustc.clone(),
            rustc_version: self.rustc_version.clone(),
            rustup_toolchain: self.toolchain.clone(),
            configuration: RustServer.effective_configuration().into(),
            trust: self.trust.clone(),
            transport: "stdio-v1".into(),
            cache_namespace: self.cache_namespace.clone(),
        })
    }
}

/// Starts rust-analyzer in the module from the one launch the core granted.
pub struct RustBuilder;

impl ProviderBuilder for RustBuilder {
    /// The session settings and the launch plan of the analyzer: its binary, no arguments and
    /// exactly the profile's cleared environment.
    fn plan(
        &self,
        _worktree: &Path,
        settings: &Value,
    ) -> std::io::Result<(ProviderSettings, ProviderLaunchPlan)> {
        let refused =
            |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, what.to_owned());
        let settings: RustProviderSettings =
            serde_json::from_value(settings.clone()).map_err(|_| refused("provider settings"))?;
        let profile = settings
            .profile()
            .map_err(|_| refused("provider profile"))?;
        let plan = ProviderLaunchPlan {
            program: settings.binary.clone(),
            args: Vec::new(),
            env: profile.launch_environment(),
            reads: Vec::new(),
        };
        Ok((ProviderSettings::new(profile), plan))
    }
}

/// The Rust module: a language server (analyzer) or checker wrapped around the host's
/// provider/support servers, adding what only Rust knows — describe, cargo check.
pub struct RustModule<S: ModuleServer> {
    /// The role this instance serves.
    role: Role,
    /// The host server it extends.
    inner: S,
}

/// `check_plan`: plan one `cargo check`, let the core run it, interpret its output.
async fn check_plan(incoming: &Incoming, effects: &mut Effects<'_>) -> Result<Answer, ServeError> {
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
    let started = std::time::Instant::now();
    // The missing-cargo answer depends on the toolchain alone, so it needs no probe.
    if checker_for(&config, Duration::ZERO, None).cargo_missing(&query.request) {
        return Ok(reply(&ProblemSnapshot::unavailable(
            crate::LANGUAGE,
            UnavailableReason::ToolMissing,
            query.request.input_generation,
        )));
    }
    let primary = developer_dir(&config, effects).await?;
    let checker = checker_for(&config, Duration::from_millis(query.timeout_ms), primary);
    let effect = checker
        .cargo_check_plan(&query.request)
        .to_effect(&query.request);
    let (outcome, attachments) = effects.run(effect).await?;
    Ok(reply(&interpret(
        &query.request,
        &outcome,
        stream(&attachments, 1),
        stream(&attachments, 2),
        started.elapsed().as_millis() as u64,
    )))
}

impl<S: ModuleServer> RustModule<S> {
    /// Wraps `inner` for `role`.
    pub fn new(role: Role, inner: S) -> Self {
        Self { role, inner }
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
        reply(&interpret(
            &query.request,
            &query.outcome,
            stdout.bytes.clone(),
            stderr.bytes.clone(),
            0,
        ))
    }
}

impl<S: ModuleServer> ModuleServer for RustModule<S> {
    /// The host server's declaration plus what the role adds.
    fn declaration(&self) -> Declaration {
        let mut declaration = self.inner.declaration();
        for decl in &mut declaration.capabilities {
            let added = match decl.capability {
                Capability::Describe => true,
                Capability::CheckPlan | Capability::CheckParse => self.role == Role::Checker,
                _ => false,
            };
            if added {
                *decl = CapabilityDecl::v0(decl.capability, Support::Supported);
            }
        }
        declaration
    }

    /// The host server keeps the grant and settings.
    fn hello(&mut self, offer: &HelloOffer) -> impl Future<Output = Result<(), String>> + Send {
        self.inner.hello(offer)
    }

    /// Rust's own capabilities here, everything else through the host server.
    async fn call<'a>(
        &'a mut self,
        incoming: Incoming,
        mut effects: Effects<'a>,
    ) -> Result<Answer, ServeError> {
        match incoming.capability {
            Capability::Describe => Ok(self.describe(&incoming)),
            Capability::CheckPlan if self.role == Role::Checker => {
                check_plan(&incoming, &mut effects).await
            }
            Capability::CheckParse if self.role == Role::Checker => Ok(self.check_parse(&incoming)),
            _ => self.inner.call(incoming, effects).await,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::RustSupport;
    use agent_ide_core::{
        checks::CheckState,
        lang::LanguageSupport,
        modules::{
            contract::{Fence, Outcome},
            fake::{FakeEffects, in_memory, offer},
            host::{Call, EffectRunner, HostChannel, NoEffects},
            payload::{
                AnalyzeSource, Field, FormatPlanRequest, SourceAnalysis, SourceField, SourceRef,
                SourceText, TestFacts, decode,
            },
            recipe::{Admission, Described, expand},
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

    /// The Rust module of `role` as the root assembles it, over the in-memory fake transport.
    async fn serve(role: Role) -> (HostChannel, agent_ide_core::modules::contract::HelloReply) {
        install();
        let offer = offer(module_id(), env!("CARGO_PKG_VERSION"), role, 1);
        match role {
            Role::Analyzer => in_memory(RustModule::new(role, provider_server()), offer)
                .await
                .unwrap(),
            Role::Checker => in_memory(RustModule::new(role, support_server()), offer)
                .await
                .unwrap(),
        }
    }

    /// Opens a module of `role` over the in-memory fake transport.
    async fn open(role: Role) -> HostChannel {
        serve(role).await.0
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
        for (role, supported, unsupported) in [
            (Role::Analyzer, Capability::Semantic, Capability::CheckPlan),
            (Role::Checker, Capability::CheckPlan, Capability::Semantic),
        ] {
            let (_, reply) = serve(role).await;
            let support = |capability| {
                reply
                    .capabilities
                    .iter()
                    .find(|decl| decl.capability == capability)
                    .map(|decl| decl.support)
            };
            assert_eq!(support(supported), Some(Support::Supported), "{role:?}");
            assert_eq!(support(unsupported), Some(Support::Unsupported), "{role:?}");
            assert_eq!(support(Capability::Describe), Some(Support::Supported));
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

    /// The formatter plan is the rustfmt command with the project's edition, or nothing.
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
        // The core's expansion runs exactly the in-process argument vector: the home tool it
        // resolves for `rustfmt`, then the same arguments, the candidate on stdin.
        let argv = RustSupport
            .format_stdin_command(&project, Path::new("src/lib.rs"))
            .unwrap();
        let tool = dir.0.join("bin/rustfmt");
        let programs = [("rustfmt".to_owned(), tool.clone())];
        let spec = expand(
            RECIPES,
            &plan,
            &Admission {
                worktree: &dir.0,
                cache_dir: &dir.0.join("cache"),
                read_denies: &[],
                home: Some(&dir.0),
                launcher_roots: &[],
                developer_dirs: &[],
                programs: &programs,
                timeout: Duration::from_secs(10),
            },
        )
        .unwrap();
        assert_eq!(argv[0], "rustfmt");
        assert_eq!(spec.program, tool);
        assert_eq!(
            spec.args,
            argv[1..]
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(spec.cwd, dir.0);
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
            let checker = checker_for(&section, Duration::from_secs(300), Some(developer.clone()));
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
        let (mut channel, _) = serve(Role::Checker).await;
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
        assert_eq!(
            effects.runs, 2,
            "the developer-directory probe, then the check"
        );
        assert_eq!(effects.last.unwrap().recipe, "cargo_check");
    }

    /// Answers the developer-directory probe with `selected` and exit `status`, every other
    /// effect with a clean build, and records each request.
    struct Platform {
        /// The probe's stdout.
        selected: Vec<u8>,
        /// The probe's exit status.
        status: i32,
        /// Every effect requested, in order.
        seen: Vec<EffectRequest>,
    }

    impl EffectRunner for Platform {
        fn run<'a>(
            &'a mut self,
            _fence: &'a Fence,
            effect: EffectRequest,
        ) -> BoxFuture<'a, (EffectOutcome, Vec<Attachment>)> {
            let (status, stdout) = if effect.recipe == XCODE_SELECT.id {
                (self.status, self.selected.clone())
            } else {
                (0, br#"{"reason":"build-finished","success":true}"#.to_vec())
            };
            self.seen.push(effect);
            Box::pin(async move {
                (
                    EffectOutcome::Completed {
                        effect_id: "e".into(),
                        status: Some(status),
                        timed_out: false,
                        truncated: false,
                        stdout_bytes: stdout.len() as u64,
                        stderr_bytes: 0,
                    },
                    vec![Attachment::octets(1, stdout)],
                )
            })
        }
    }

    /// The developer directory the check is planned around is the in-process checker's choice:
    /// the section's existing override without any probe; else the platform selection the core
    /// probes (`xcode-select -p`), even a non-standard Xcode; else the standard locations.
    #[tokio::test]
    async fn the_check_follows_the_selected_developer_directory() {
        install();
        let base = Scratch::new("platform");
        let worktree = base.0.join("ws");
        let toolchain = base.0.join("toolchains/stable-aarch64-apple-darwin");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(toolchain.join("bin")).unwrap();
        std::fs::write(toolchain.join("bin/cargo"), "").unwrap();
        let overridden = developer(&base.0.join("override"));
        let selected = developer(&base.0.join("Xcode-beta.app/Contents"));
        let request = CheckRequest {
            worktree,
            cache_dir: base.0.join("cache"),
            input_generation: 4,
            read_denies: Vec::new(),
        };
        let cases = [
            (Some(&overridden), 0, Some(overridden.clone()), 1),
            (None, 0, Some(selected.clone()), 2),
            (None, 1, crate::checks::standard_developer_dir(), 2),
        ];
        for (override_dir, status, expected, runs) in cases {
            let config = json!({"toolchain_dir": toolchain, "developer_dir": override_dir});
            let (mut channel, _) = serve(Role::Checker).await;
            let mut platform = Platform {
                selected: format!("{}\n", selected.display()).into_bytes(),
                status,
                seen: Vec::new(),
            };
            let reply = channel
                .call(
                    Call {
                        capability: Capability::CheckPlan,
                        scope_key: "scope".into(),
                        revision_key: "revision".into(),
                        payload: payload::encode(&CheckPlanRequest {
                            request: request.clone(),
                            config: config.clone(),
                            timeout_ms: 60_000,
                        }),
                        attachments: Vec::new(),
                    },
                    Duration::from_secs(10),
                    &mut platform,
                )
                .await
                .unwrap();
            assert!(matches!(reply.outcome, Outcome::Result(_)), "{reply:?}");
            assert_eq!(platform.seen.len(), runs, "{expected:?}");
            if runs == 2 {
                assert_eq!(platform.seen[0].recipe, "xcode_select");
            }
            let section: ProjectRustChecksConfig = serde_json::from_value(config).unwrap();
            let in_process = checker_for(&section, Duration::from_secs(60), expected.clone());
            // Only what the developer directory decides (home-derived values are another
            // test's, which may move `AGENT_IDE_HOME` concurrently).
            let developer_params = |effect: &EffectRequest| {
                let mut params = effect.params.clone();
                params.retain(|name, _| {
                    [
                        "developer",
                        "cc",
                        "cxx",
                        "ar",
                        "ranlib",
                        "sdkroot",
                        "linker",
                    ]
                    .contains(&name.as_str())
                        || name == "linker_flag"
                });
                (effect.recipe.clone(), params)
            };
            assert_eq!(
                developer_params(platform.seen.last().unwrap()),
                developer_params(&in_process.cargo_check_plan(&request).to_effect(&request)),
                "{expected:?}"
            );
            if let Some(expected) = expected {
                assert!(
                    matches!(&platform.seen.last().unwrap().params["developer"],
                        Param::Paths(dirs) if dirs.contains(&expected)),
                    "the selected developer directory is a read root"
                );
            }
        }
    }

    /// The core expands the probe to exactly `/usr/bin/xcode-select -p` with no environment, or
    /// with the daemon's `DEVELOPER_DIR` when it lies in an admitted developer directory; any
    /// other program or directory is refused.
    #[test]
    fn the_probe_expands_to_xcode_select_only() {
        let base = Scratch::new("probe");
        let developer = developer(&base.0);
        let developer_dirs = [developer.clone()];
        let admission = Admission {
            worktree: &base.0,
            cache_dir: &base.0.join("cache"),
            read_denies: &[],
            home: None,
            launcher_roots: &[],
            developer_dirs: &developer_dirs,
            programs: &[],
            timeout: Duration::from_secs(60),
        };
        let probe = |extra: Option<(&str, &Path)>| EffectRequest {
            recipe: "xcode_select".into(),
            params: [("tool", Path::new("/usr/bin/xcode-select"))]
                .into_iter()
                .chain(extra)
                .map(|(name, path)| (name.to_owned(), Param::Path(path.to_path_buf())))
                .collect(),
        };
        let spec = expand(RECIPES, &probe(None), &admission).unwrap();
        assert_eq!(spec.program, PathBuf::from("/usr/bin/xcode-select"));
        assert_eq!(spec.args, ["-p"]);
        assert!(spec.env.iter().all(|(name, _)| name != "DEVELOPER_DIR"));
        assert!(spec.timeout <= Duration::from_secs(10));
        let spec = expand(
            RECIPES,
            &probe(Some(("developer_dir", &developer))),
            &admission,
        )
        .unwrap();
        assert!(
            spec.env
                .contains(&("DEVELOPER_DIR".to_owned(), developer.display().to_string()))
        );
        assert!(
            expand(
                RECIPES,
                &probe(Some(("developer_dir", &base.0))),
                &admission
            )
            .is_err()
        );
        let mut other = probe(None);
        other
            .params
            .insert("tool".into(), Param::Path(PathBuf::from("/bin/sh")));
        assert!(expand(RECIPES, &other, &admission).is_err());
    }

    /// Output streams are found by attachment id, not by position: a lone stderr or a reversed
    /// pair keeps each stream's bytes.
    #[test]
    fn streams_are_looked_up_by_attachment_id() {
        let attachment = |id: u32, bytes: &[u8]| Attachment {
            id,
            content_type: "text/plain; charset=utf-8".into(),
            bytes: bytes.to_vec(),
        };
        let reversed = [attachment(2, b"err"), attachment(1, b"out")];
        assert_eq!(stream(&reversed, 1), b"out");
        assert_eq!(stream(&reversed, 2), b"err");
        let stderr_only = [attachment(2, b"err")];
        assert!(stream(&stderr_only, 1).is_empty());
        assert_eq!(stream(&stderr_only, 2), b"err");
    }

    /// Without the pinned cargo the check is `tool_missing` before any effect is asked for.
    #[tokio::test]
    async fn a_missing_cargo_asks_for_no_effect() {
        install();
        let base = Scratch::new("missing");
        let (mut channel, _) = serve(Role::Checker).await;
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
