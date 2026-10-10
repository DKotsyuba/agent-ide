//! The TypeScript/JavaScript module: what `agent-ide module typescript <role>` serves, for front-end
//! and Node back-end code alike (all eight script extensions).
//!
//! Both roles answer every pure computation with the language's own support through the generic
//! support module (project facts, source outline and syntax verdict, insertion geometry, test
//! selection and output parsing, formatter and syntax-probe plans, name-fact anchors) and answer
//! `describe` for the launcher: the provider declaration's rules and the `project_checks`
//! section. The checker role runs the unchanged [`TypeScriptChecker`] for planning and parsing;
//! the one process it needs is the [`TSC`] recipe, which the core expands, stages the embedded
//! adapter for, and runs itself. The module never spawns a check and never writes.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use agent_ide_core::{
    assistance::launcher::ProviderLaunch,
    checks::{
        BoxFuture, LanguageChecks,
        runner::{ConfinedRunner, RunOutput, RunSpec},
    },
    intelligence::{server::LanguageServer, session::ProviderSettings},
    modules::{
        adapter::SupportServer,
        contract::{Capability, CapabilityDecl, Declaration, ErrorCode, HelloOffer, Role, Support},
        payload::{
            Arg, CheckPlanRequest, ChecksDescription, DeclaredExecutable, DescribeQuery,
            EffectOutcome, EffectRecipe, EffectRequest, EnvRule, ExecutableSlot, LaunchDescription,
            NamedProgram, Param, PathRole, PathRule, RecipeAsset, RunClass, SlotSource, Stdin,
            decode, encode,
        },
        provider::{ProviderBuilder, ProviderLaunchPlan, ProviderServer},
        serve::{Answer, Effects, Incoming, ModuleServer, ServeError, serve_stdio},
        wire::Attachment,
    },
};

use crate::{
    backend::TypeScriptServer,
    checks::{ProjectTypeScriptChecksConfig, TypeScriptChecker, TypeScriptChecks},
    profile::{
        ModuleSessionProfile, TypeScriptBundleFileV1, TypeScriptProviderBundleV1,
        TypeScriptProviderBundleV1Identity,
    },
};

/// One accepted file of the bridge bundle as the core hands it to the analyzer module.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedFile {
    /// Absolute accepted path.
    pub path: PathBuf,
    /// Its accepted BLAKE3 digest, hex.
    pub blake3: String,
    /// Its exact accepted byte length.
    pub bytes: u64,
}

impl AcceptedFile {
    /// The bundle member this file names.
    fn member(&self) -> io::Result<TypeScriptBundleFileV1> {
        Ok(TypeScriptBundleFileV1 {
            path: self.path.clone(),
            blake3: digest(&self.blake3)?,
            bytes: self.bytes,
        })
    }

    /// The file `member` measured as.
    fn of(member: &TypeScriptBundleFileV1) -> Self {
        Self {
            path: member.path.clone(),
            blake3: member.blake3.to_hex().to_string(),
            bytes: member.bytes,
        }
    }
}

/// A hex BLAKE3 digest, or the configuration refusal.
fn digest(hex: &str) -> io::Result<blake3::Hash> {
    blake3::Hash::from_hex(hex)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "typescript digest"))
}

/// The launcher-accepted bridge bundle the core hands its analyzer module
/// (`hello.config.provider.settings`); the module re-measures every file before it starts the
/// bridge. Project resolution stays with the core, which admits each document before asking.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalyzerSettings {
    /// Accepted Node executable.
    pub node: PathBuf,
    /// Its accepted BLAKE3 digest, hex.
    pub node_digest: String,
    /// Accepted Node release identity.
    pub node_version: String,
    /// Accepted bridge entry module.
    pub bridge: AcceptedFile,
    /// Accepted bridge release identity.
    pub bridge_version: String,
    /// Accepted `tsserver.js`.
    pub tsserver: AcceptedFile,
    /// Accepted TypeScript release identity.
    pub typescript_version: String,
    /// The accepted runtime closure, strictly sorted.
    pub closure: Vec<AcceptedFile>,
    /// The bridge's private temporary directory (the core created it).
    pub tmp: PathBuf,
}

impl AnalyzerSettings {
    /// The settings that grant `bundle`, its bridge running with `tmp` as its temporary directory.
    pub fn of(bundle: &TypeScriptProviderBundleV1, tmp: PathBuf) -> Self {
        let identity = bundle.identity();
        Self {
            node: identity.node.clone(),
            node_digest: identity.node_blake3.to_hex().to_string(),
            node_version: identity.node_version.clone(),
            bridge: AcceptedFile::of(&identity.bridge),
            bridge_version: identity.bridge_version.clone(),
            tsserver: AcceptedFile::of(&identity.tsserver),
            typescript_version: identity.typescript_version.clone(),
            closure: identity.closure.iter().map(AcceptedFile::of).collect(),
            tmp,
        }
    }

    /// Every file the module may load, with its digest: the core grants exactly these.
    pub fn accepted(&self) -> Vec<(PathBuf, String)> {
        std::iter::once((self.node.clone(), self.node_digest.clone()))
            .chain(
                std::iter::once(&self.bridge)
                    .chain(std::iter::once(&self.tsserver))
                    .chain(&self.closure)
                    .map(|file| (file.path.clone(), file.blake3.clone())),
            )
            .collect()
    }

    /// The measured bundle: every file must still match what the core accepted.
    fn bundle(&self) -> io::Result<TypeScriptProviderBundleV1> {
        TypeScriptProviderBundleV1::new(TypeScriptProviderBundleV1Identity {
            node: self.node.clone(),
            node_blake3: digest(&self.node_digest)?,
            node_version: self.node_version.clone(),
            bridge: self.bridge.member()?,
            bridge_version: self.bridge_version.clone(),
            tsserver: self.tsserver.member()?,
            typescript_version: self.typescript_version.clone(),
            closure: self
                .closure
                .iter()
                .map(AcceptedFile::member)
                .collect::<io::Result<_>>()?,
        })
        .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "typescript bundle refused"))
    }
}

/// Plans the bridge session exactly as the in-process backend starts it: `node <bridge> --stdio`
/// with a private temporary directory and nothing else in its environment.
struct Host;

impl ProviderBuilder for Host {
    fn plan(
        &self,
        _worktree: &Path,
        settings: &Value,
    ) -> io::Result<(ProviderSettings, ProviderLaunchPlan)> {
        let settings: AnalyzerSettings = serde_json::from_value(settings.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "typescript settings"))?;
        let bundle = settings.bundle()?;
        let plan = ProviderLaunchPlan {
            program: settings.node.clone(),
            args: vec![bundle.bridge().display().to_string(), "--stdio".to_owned()],
            env: BTreeMap::from([("TMPDIR".to_owned(), settings.tmp.display().to_string())]),
            reads: vec![bundle.bridge().to_path_buf()],
        };
        Ok((
            ProviderSettings::new(ModuleSessionProfile::new(bundle.tsserver().to_path_buf())),
            plan,
        ))
    }
}

/// The embedded compiler adapter, staged by the core in the private cache before each run.
const ADAPTER: &[u8] = include_bytes!("typescript_adapter.js");

/// The project check: Node running the staged adapter around the pinned `tsc`, writes confined to
/// the private cache. The core's expansion equals [`TypeScriptChecker::run_spec`] field for field
/// (see the equality test below).
pub const TSC: EffectRecipe = EffectRecipe {
    id: "tsc",
    program: "node",
    args: &[
        Arg::Param("adapter"),
        Arg::Literal("--project"),
        Arg::Param("config"),
        Arg::Literal("--pretty"),
        Arg::Literal("false"),
        Arg::Literal("--diagnostics"),
        Arg::Literal("--listFiles"),
        Arg::Literal("--noEmit"),
        Arg::Literal("--tsBuildInfoFile"),
        Arg::Param("tsbuildinfo"),
        Arg::Literal("--extendedDiagnostics"),
        Arg::Literal("false"),
        Arg::Literal("--explainFiles"),
        Arg::Literal("false"),
        Arg::Literal("--traceResolution"),
        Arg::Literal("false"),
    ],
    env: &[
        EnvRule::SearchPath {
            name: "PATH",
            param: "node_bin",
            fixed: &["/usr/bin", "/bin"],
        },
        EnvRule::Home { name: "HOME" },
        EnvRule::Param {
            name: "TMPDIR",
            param: "tmp",
            optional: false,
        },
        EnvRule::Param {
            name: "CHECK_TSC_CLI",
            param: "tsc_cli",
            optional: false,
        },
        EnvRule::Param {
            name: "CHECK_CACHE",
            param: "cache",
            optional: false,
        },
        EnvRule::ReadRootsJson {
            name: "CHECK_READ_ROOTS",
        },
        EnvRule::ReadDeniesJson {
            name: "CHECK_READ_DENIES",
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
            param: "node_root",
            roles: &[PathRole::LauncherRoot],
            existing_only: false,
            read_root: true,
        },
        PathRule {
            param: "typescript_root",
            roles: &[PathRole::LauncherRoot],
            existing_only: false,
            read_root: true,
        },
        PathRule {
            param: "cache",
            roles: &[PathRole::Cache],
            existing_only: false,
            read_root: true,
        },
        PathRule {
            param: "ancestors",
            roles: &[PathRole::AncestorFile(&["node_modules", "package.json"])],
            existing_only: false,
            read_root: true,
        },
        PathRule {
            param: "config",
            roles: &[PathRole::Worktree],
            existing_only: false,
            read_root: false,
        },
        PathRule {
            param: "adapter",
            roles: &[PathRole::Cache],
            existing_only: false,
            read_root: false,
        },
        PathRule {
            param: "tsbuildinfo",
            roles: &[PathRole::Cache],
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
            param: "tmp_marker",
            roles: &[PathRole::Cache],
            existing_only: false,
            read_root: false,
        },
        PathRule {
            param: "tsc_cli",
            roles: &[PathRole::LauncherRoot],
            existing_only: false,
            read_root: false,
        },
        PathRule {
            param: "node_bin",
            roles: &[PathRole::LauncherRoot],
            existing_only: false,
            read_root: false,
        },
    ],
    executables: &[ExecutableSlot {
        name: "node",
        source: SlotSource::Launcher("node"),
    }],
    stdin: Stdin::Null,
    class: RunClass::Background,
    timeout_ceiling_ms: 900_000,
    capture_bytes: 64 << 20,
    assets: &[
        RecipeAsset {
            param: "adapter",
            bytes: ADAPTER,
        },
        // Creates `<cache>/tmp` (the run's `TMPDIR`), which the in-process check makes itself.
        RecipeAsset {
            param: "tmp_marker",
            bytes: b"",
        },
    ],
};

/// Install prefixes an accepted Node or TypeScript package may live under beyond the worktree
/// and the user's home (Homebrew, MacPorts, system and developer installs): the roots the
/// syntax-probe recipe admits its Node and `typescript.js` from.
pub const INSTALL_PREFIXES: [&str; 6] = [
    "/opt",
    "/usr/local",
    "/usr",
    "/Library/Frameworks",
    "/Library/Developer",
    "/Applications",
];

/// Roles of the Node and the `typescript.js` the syntax probe runs.
const TOOL_ROLES: &[PathRole] = &[
    PathRole::Worktree,
    PathRole::HomeRelative,
    PathRole::LauncherRoot,
    PathRole::DeveloperDir,
];

/// Home tools an interactive recipe may run, looked up by the core on its formatter PATH.
const HOME_TOOLS: &[ExecutableSlot] = &[
    ExecutableSlot {
        name: "npx",
        source: SlotSource::HomeTool("npx"),
    },
    ExecutableSlot {
        name: "node",
        source: SlotSource::HomeTool("node"),
    },
];

/// Fixed trailing `PATH` entries of an interactive run.
const SYSTEM_PATH: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"];

/// The environment of a run of a program found on the formatter `PATH`: its own directory leads
/// `PATH`, so a script tool finds the interpreter beside it.
const fn tool_env(slot: &'static str) -> [EnvRule; 2] {
    [
        EnvRule::Home { name: "HOME" },
        EnvRule::SlotDir {
            name: "PATH",
            slot,
            fixed: SYSTEM_PATH,
        },
    ]
}

/// The environment of a run of an admitted Node path.
const PATH_ENV: [EnvRule; 2] = [
    EnvRule::Home { name: "HOME" },
    EnvRule::Literal {
        name: "PATH",
        value: "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin",
    },
];

/// A recipe reading the candidate on stdin: `program` is a home tool slot or a path parameter
/// admitted under [`TOOL_ROLES`].
const fn interactive(
    id: &'static str,
    program: &'static str,
    args: &'static [Arg],
    env: &'static [EnvRule],
) -> EffectRecipe {
    EffectRecipe {
        id,
        program,
        args,
        env,
        paths: &[
            PathRule {
                param: "node_path",
                roles: TOOL_ROLES,
                existing_only: false,
                read_root: false,
            },
            PathRule {
                param: "module",
                roles: TOOL_ROLES,
                existing_only: false,
                read_root: false,
            },
        ],
        executables: HOME_TOOLS,
        stdin: Stdin::Candidate,
        class: RunClass::Interactive,
        timeout_ceiling_ms: 10_000,
        capture_bytes: 64 << 20,
        assets: &[],
    }
}

const NPX_ENV: [EnvRule; 2] = tool_env("npx");
const NODE_TOOL_ENV: [EnvRule; 2] = tool_env("node");

/// The effect recipes of the TypeScript module, declared by the root: the `tsc` check, the
/// Prettier formatter and the syntax probe that read the candidate on stdin.
pub const RECIPES: &[EffectRecipe] = &[
    TSC,
    interactive(
        "prettier-npx",
        "npx",
        &[
            Arg::Literal("prettier"),
            Arg::Literal("--stdin-filepath"),
            Arg::Param("file"),
        ],
        &NPX_ENV,
    ),
    interactive(
        "ts-probe",
        "node_path",
        &[
            Arg::Literal("-e"),
            Arg::Literal(crate::support::TS_PROBE),
            Arg::Param("module"),
            Arg::Param("file"),
        ],
        &PATH_ENV,
    ),
    interactive(
        "ts-probe-tool",
        "node",
        &[
            Arg::Literal("-e"),
            Arg::Literal(crate::support::TS_PROBE),
            Arg::Param("module"),
            Arg::Param("file"),
        ],
        &NODE_TOOL_ENV,
    ),
];

/// The recipe request of a formatter or probe argument vector the in-process support builds
/// ([`crate::support`]); `None` for any other shape, which the core then refuses.
pub fn interactive_effect(argv: &[String]) -> Option<EffectRequest> {
    let words: Vec<&str> = argv.iter().map(String::as_str).collect();
    let token = |value: &str| Param::Token(value.to_owned());
    let (recipe, params): (&str, Vec<(&str, Param)>) = match words.as_slice() {
        ["npx", "prettier", "--stdin-filepath", file] => (
            "prettier-npx",
            vec![
                ("npx", Param::Executable("npx".to_owned())),
                ("file", token(file)),
            ],
        ),
        [node, "-e", probe, module, file]
            if *probe == crate::support::TS_PROBE && module.starts_with('/') =>
        {
            if node.starts_with('/') {
                (
                    "ts-probe",
                    vec![
                        ("node_path", Param::Path(PathBuf::from(node))),
                        ("module", Param::Path(PathBuf::from(module))),
                        ("file", token(file)),
                    ],
                )
            } else if *node == "node" {
                (
                    "ts-probe-tool",
                    vec![
                        ("node", Param::Executable("node".to_owned())),
                        ("module", Param::Path(PathBuf::from(module))),
                        ("file", token(file)),
                    ],
                )
            } else {
                return None;
            }
        }
        _ => return None,
    };
    Some(EffectRequest {
        recipe: recipe.to_owned(),
        params: params
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    })
}

/// Name of the Node slot and program.
const NODE: &str = "node";

/// The `tsc` recipe request that reproduces `spec`, a run [`TypeScriptChecker`] built for its
/// request ([`TypeScriptChecker::run_spec`]): the module names its inputs, the core admits each
/// one, stages the adapter and rebuilds the run itself.
///
/// # Errors
///
/// `InvalidData` when `spec` is not a run of [`TypeScriptChecker`] (another argument or root
/// shape, a missing variable): it is never asked of the core.
pub fn tsc_effect(spec: &RunSpec) -> io::Result<EffectRequest> {
    let malformed = || io::Error::new(io::ErrorKind::InvalidData, "not a tsc run");
    let [adapter, _, config, .., tsbuildinfo, _, _, _, _, _, _] = spec.args.as_slice() else {
        return Err(malformed());
    };
    let [worktree, node, typescript_root, cache, ancestors @ ..] = spec.read_roots.as_slice()
    else {
        return Err(malformed());
    };
    let env = |name: &str| {
        spec.env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .ok_or_else(malformed)
    };
    let node_bin = env("PATH")?
        .split(':')
        .next()
        .map(PathBuf::from)
        .ok_or_else(malformed)?;
    let tmp = PathBuf::from(env("TMPDIR")?);
    let path = |path: &Path| Param::Path(path.to_path_buf());
    Ok(EffectRequest {
        recipe: TSC.id.to_owned(),
        params: std::collections::BTreeMap::from([
            ("node".to_owned(), Param::Executable(NODE.to_owned())),
            ("worktree".to_owned(), path(worktree)),
            ("node_root".to_owned(), path(node)),
            ("typescript_root".to_owned(), path(typescript_root)),
            ("cache".to_owned(), path(cache)),
            ("ancestors".to_owned(), Param::Paths(ancestors.to_vec())),
            ("config".to_owned(), Param::Path(PathBuf::from(config))),
            ("adapter".to_owned(), Param::Path(PathBuf::from(adapter))),
            (
                "tsbuildinfo".to_owned(),
                Param::Path(PathBuf::from(tsbuildinfo)),
            ),
            ("tmp_marker".to_owned(), Param::Path(tmp.join(".keep"))),
            ("tmp".to_owned(), Param::Path(tmp)),
            (
                "tsc_cli".to_owned(),
                Param::Path(PathBuf::from(env("CHECK_TSC_CLI")?)),
            ),
            ("node_bin".to_owned(), Param::Paths(vec![node_bin])),
        ]),
    })
}

/// Decodes and validates one provider declaration: its rules, the executables startup verifies,
/// the programs doctor probes and the syntax-probe programs.
fn describe_provider(declaration: Value) -> Result<LaunchDescription, String> {
    let launch: ProviderLaunch =
        serde_json::from_value(declaration).map_err(|error| error.to_string())?;
    let server = TypeScriptServer;
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

/// Further startup verification of one declaration: the accepted bundle is re-measured.
fn verify_provider(declaration: Value) -> Result<(), String> {
    let launch: ProviderLaunch =
        serde_json::from_value(declaration).map_err(|error| error.to_string())?;
    TypeScriptServer
        .verify_launch(&launch, &AtomicBool::new(false))
        .map_err(|error| format!("{error:?}"))
}

/// Decodes and validates the `project_checks.typescript` section and names what the core admits
/// from it: Node and the `tsc` module as programs; the Node executable, its directory (the
/// `PATH` entry) and the TypeScript package root as launcher roots.
fn describe_checks(section: Value) -> Result<ChecksDescription, String> {
    let config = TypeScriptChecks
        .parse_config(section)
        .map_err(|error| error.to_string())?;
    let config = config
        .downcast_ref::<ProjectTypeScriptChecksConfig>()
        .ok_or("typescript section")?;
    let (node, tsc) = (config.node(), config.tsc_cli());
    Ok(ChecksDescription {
        valid: agent_ide_core::checks::CheckConfig::validate(config),
        programs: vec![
            NamedProgram {
                name: NODE.to_owned(),
                path: node.to_path_buf(),
                interpreter: None,
            },
            NamedProgram {
                name: "tsc".to_owned(),
                path: tsc.to_path_buf(),
                interpreter: Some(node.to_path_buf()),
            },
        ],
        launcher_roots: [
            Some(node),
            node.parent(),
            tsc.parent().and_then(Path::parent),
            Some(tsc),
        ]
        .into_iter()
        .flatten()
        .map(Path::to_path_buf)
        .collect(),
        developer_dirs: Vec::new(),
    })
}

/// Test seam: `widen:check_plan:<flag file>` in the module fault seam makes this module ask once
/// (when it can remove the flag file) for a run with one undeclared read root, which the core
/// must refuse. Honoured only in `test-seams` builds.
fn widen_seam() -> bool {
    agent_ide_core::test_seams::var(agent_ide_core::modules::serve::FAULT_SEAM).is_some_and(
        |value| {
            value
                .strip_prefix("widen:check_plan:")
                .is_some_and(|flag| std::fs::remove_file(flag).is_ok())
        },
    )
}

/// Hands every confined run of a check to the serving loop, which asks the core to run it.
struct Bridge(
    tokio::sync::mpsc::UnboundedSender<(
        RunSpec,
        tokio::sync::oneshot::Sender<io::Result<RunOutput>>,
    )>,
);

impl ConfinedRunner for Bridge {
    /// Queues `spec` for the serving loop and waits for the core's outcome; fails once the check
    /// has ended (the module never runs a process itself).
    fn run(&self, spec: RunSpec) -> BoxFuture<'_, io::Result<RunOutput>> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let sent = self.0.send((spec, reply));
        Box::pin(async move {
            sent.map_err(|_| io::Error::other("check ended"))?;
            answer
                .await
                .unwrap_or_else(|_| Err(io::Error::other("check ended")))
        })
    }
}

/// The run output of a completed effect, or the refusal as the runner's error.
fn run_output(outcome: EffectOutcome, attachments: Vec<Attachment>) -> io::Result<RunOutput> {
    match outcome {
        EffectOutcome::Completed {
            status,
            timed_out,
            truncated,
            ..
        } => {
            let stream = |id| {
                attachments
                    .iter()
                    .find(|attachment| attachment.id == id)
                    .map(|attachment| attachment.bytes.clone())
                    .unwrap_or_default()
            };
            Ok(RunOutput {
                status,
                stdout: stream(1),
                stderr: stream(2),
                timed_out,
                truncated,
            })
        }
        EffectOutcome::Refused { cause, message } => {
            Err(io::Error::other(format!("{cause}: {message}")))
        }
    }
}

/// Answers `describe` for both roles; `None` for a capability the caller must route elsewhere.
fn describe(request: &Incoming) -> Answer {
    let reply = |value: Value| Answer::result(value);
    match decode::<DescribeQuery>(request.payload.clone()) {
        Ok(DescribeQuery::Provider { declaration }) => {
            reply(encode(&describe_provider(declaration)))
        }
        Ok(DescribeQuery::VerifyProvider { declaration }) => {
            reply(encode(&verify_provider(declaration)))
        }
        Ok(DescribeQuery::Checks { section }) => reply(encode(&describe_checks(section))),
        Ok(DescribeQuery::Presence { worktree }) => {
            reply(encode(&TypeScriptChecks.is_present(&worktree)))
        }
        Ok(DescribeQuery::ProjectInputs { document, inputs }) => reply(encode(
            &crate::profile::interpret_inputs(&document, &inputs),
        )),
        Err(error) => Answer::error(ErrorCode::InvalidRequest, error),
    }
}

/// The checker role: the language's own [`TypeScriptChecker`] plans and parses each check; the
/// `tsc` run it needs goes to the core as a recipe request.
struct CheckerServer {
    /// The language's support, for every other capability.
    support: SupportServer,
}

impl CheckerServer {
    /// Runs one check, its runs served through `effects`.
    async fn check(
        query: CheckPlanRequest,
        effects: &mut Effects<'_>,
    ) -> Result<Value, ServeError> {
        let config = TypeScriptChecks
            .parse_config(query.config)
            .map_err(|error| ServeError::Protocol(error.to_string()))?;
        let config = config
            .downcast_ref::<ProjectTypeScriptChecksConfig>()
            .ok_or_else(|| ServeError::Protocol("typescript section".to_owned()))?;
        let (runs, mut pending) = tokio::sync::mpsc::unbounded_channel();
        // The core stages the adapter and the temporary directory; this checker must not.
        let checker = TypeScriptChecker::new(
            std::sync::Arc::new(Bridge(runs)),
            config.node().to_path_buf(),
            config.tsc_cli().to_path_buf(),
            std::time::Duration::from_millis(query.timeout_ms),
        )
        .without_staging();
        let check = agent_ide_core::checks::Checker::check(&checker, query.request);
        tokio::pin!(check);
        let snapshot = loop {
            tokio::select! {
                snapshot = &mut check => break snapshot,
                Some((spec, reply)) = pending.recv() => {
                    let output = match tsc_effect(&spec) {
                        Ok(mut effect) => {
                            if widen_seam() {
                                effect.params.insert(
                                    "widened".to_owned(),
                                    Param::Path(PathBuf::from("/private/var/root")),
                                );
                            }
                            let (outcome, attachments) = effects.run(effect).await?;
                            run_output(outcome, attachments)
                        }
                        Err(error) => Err(error),
                    };
                    let _ = reply.send(output);
                }
            }
        };
        Ok(encode(&snapshot))
    }
}

impl ModuleServer for CheckerServer {
    /// The support declaration plus `check_plan` and `describe`.
    fn declaration(&self) -> Declaration {
        let mut declaration = self.support.declaration();
        for decl in &mut declaration.capabilities {
            if matches!(
                decl.capability,
                Capability::CheckPlan | Capability::Describe
            ) {
                *decl = CapabilityDecl::v0(decl.capability, Support::Supported);
            }
        }
        declaration
    }

    /// `check_plan` runs the check; `describe` answers the launcher questions; everything else
    /// is the language's own support.
    async fn call<'a>(
        &'a mut self,
        request: Incoming,
        mut effects: Effects<'a>,
    ) -> Result<Answer, ServeError> {
        match request.capability {
            Capability::CheckPlan => {
                let query: CheckPlanRequest = decode(request.payload)
                    .map_err(|error| ServeError::Protocol(error.to_string()))?;
                Ok(Answer::result(Self::check(query, &mut effects).await?))
            }
            Capability::Describe => Ok(describe(&request)),
            _ => self.support.call(request, effects).await,
        }
    }
}

/// The analyzer role: the language's support, the launcher `describe` answers and the bridge
/// session the core granted.
struct AnalyzerServer {
    /// The language's support and its hosted bridge, for every other capability.
    provider: ProviderServer<Host>,
}

impl ModuleServer for AnalyzerServer {
    /// The provider's declaration plus `describe`.
    fn declaration(&self) -> Declaration {
        let mut declaration = self.provider.declaration();
        for decl in &mut declaration.capabilities {
            if decl.capability == Capability::Describe {
                *decl = CapabilityDecl::v0(decl.capability, Support::Supported);
            }
        }
        declaration
    }

    /// Keeps the granted bridge launch (and starts it) as the provider server does.
    fn hello(
        &mut self,
        offer: &HelloOffer,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        self.provider.hello(offer)
    }

    /// `describe` answers the launcher questions; everything else is the language's own support
    /// or the hosted bridge.
    async fn call<'a>(
        &'a mut self,
        request: Incoming,
        effects: Effects<'a>,
    ) -> Result<Answer, ServeError> {
        match request.capability {
            Capability::Describe => Ok(describe(&request)),
            _ => self.provider.call(request, effects).await,
        }
    }
}

/// Serves `agent-ide module typescript <role>` over this process's stdin and stdout until the
/// core closes it.
///
/// # Errors
///
/// The transport fault that ended the loop.
pub async fn serve(role: Role) -> Result<(), ServeError> {
    let support = SupportServer::new(crate::LANGUAGE, env!("CARGO_PKG_VERSION"))
        .with_effect_plans(interactive_effect);
    match role {
        Role::Analyzer => {
            let provider = ProviderServer::new(support, Host);
            serve_stdio(AnalyzerServer { provider }, role).await
        }
        Role::Checker => serve_stdio(CheckerServer { support }, role).await,
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use agent_ide_core::{
        checks::{CheckRequest, runner::FakeRunner},
        modules::recipe::{Described, Refusal, expand_staged},
    };

    use super::*;

    /// A scratch layout: a worktree below a deeper parent, a Node install and a TypeScript
    /// package, all below one removable base.
    struct Layout {
        /// Removed on drop.
        base: PathBuf,
    }

    impl Drop for Layout {
        /// Removes the layout, ignoring a tree that is already gone.
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// The layout, the check request, the `project_checks` section, the node and `tsc` paths.
    fn layout(tag: &str) -> (Layout, CheckRequest, Value, PathBuf, PathBuf) {
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("typescript-recipe-{tag}-{}", std::process::id()));
        let worktree = base.join("outer/ws");
        let node = base.join("tools/bin/node");
        let tsc = base.join("tools/lib/node_modules/typescript/lib/tsc.js");
        for dir in [
            worktree.clone(),
            node.parent().unwrap().to_path_buf(),
            tsc.parent().unwrap().to_path_buf(),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        for file in [&node, &tsc] {
            std::fs::write(file, "").unwrap();
        }
        std::fs::write(worktree.join("tsconfig.json"), "{}").unwrap();
        let request = CheckRequest {
            worktree: worktree.clone(),
            cache_dir: base.join("cache/ws"),
            input_generation: 1,
            read_denies: vec![agent_ide_core::execution::seatbelt::ReadDeny::Path(
                worktree.join(".env"),
            )],
        };
        let section = serde_json::json!({"node": node, "tsc_cli": tsc});
        (Layout { base }, request, section, node, tsc)
    }

    /// The checker, its spec for `request` and the admission its section describes.
    fn expansion(
        layout: &Layout,
        request: &CheckRequest,
        section: Value,
        node: PathBuf,
        tsc: PathBuf,
    ) -> (RunSpec, EffectRequest, Described, PathBuf) {
        let checker = TypeScriptChecker::new(
            Arc::new(FakeRunner::default()),
            node,
            tsc,
            Duration::from_secs(600),
        );
        let mut spec = checker.run_spec(request, &request.worktree.join("tsconfig.json"));
        let home = layout.base.join("home");
        // The in-process run names the real user home; the admission names the fixture's.
        for (key, value) in &mut spec.env {
            if key == "HOME" {
                *value = home.display().to_string();
            }
        }
        let effect = tsc_effect(&spec).unwrap();
        (
            spec,
            effect,
            Described::new(&describe_checks(section).unwrap()),
            home,
        )
    }

    /// The run the core rebuilds from the module's `tsc` request — admitted through the module's
    /// describe answer — equals exactly the run the in-process checker builds for the same
    /// inputs: arguments, environment (including the JSON read roots and denies), read roots in
    /// order with every ancestor `node_modules` and `package.json`, write roots, denies, timeout
    /// and capture; the adapter and the temporary directory are staged by the core.
    #[test]
    fn tsc_recipe_reproduces_the_in_process_run() {
        let (layout, request, section, node, tsc) = layout("equal");
        let (spec, effect, described, home) = expansion(&layout, &request, section, node, tsc);
        let admission = described.admission(
            &request.worktree,
            &request.cache_dir,
            &request.read_denies,
            Some(&home),
            Duration::from_secs(600),
        );
        let (rebuilt, staged) = expand_staged(RECIPES, &effect, &admission).unwrap();
        assert_eq!(rebuilt, spec);
        assert!(spec.read_roots.len() > 8, "ancestors are read roots");
        let staged: Vec<(&Path, usize)> = staged
            .iter()
            .map(|(path, bytes)| (path.as_path(), bytes.len()))
            .collect();
        assert_eq!(
            staged,
            [
                (
                    request.cache_dir.join("typescript-check.js").as_path(),
                    ADAPTER.len()
                ),
                (request.cache_dir.join("tmp/.keep").as_path(), 0),
            ]
        );
        let env: std::collections::BTreeMap<_, _> = spec.env.iter().cloned().collect();
        assert_eq!(
            env["CHECK_READ_ROOTS"],
            serde_json::to_string(&spec.read_roots).unwrap()
        );
        assert!(env["CHECK_READ_DENIES"].contains(".env"));
    }

    /// Requests widened beyond the recipe are refused before anything runs: a config outside the
    /// worktree, a smuggled extra read root, an executable the section does not name.
    #[test]
    fn widened_tsc_requests_are_refused() {
        let (layout, request, section, node, tsc) = layout("widen");
        let (_, effect, described, home) = expansion(&layout, &request, section, node, tsc);
        let admission = described.admission(
            &request.worktree,
            &request.cache_dir,
            &request.read_denies,
            Some(&home),
            Duration::from_secs(600),
        );
        let mut outside = effect.clone();
        outside.params.insert(
            "config".into(),
            Param::Path(PathBuf::from("/private/var/root/tsconfig.json")),
        );
        assert!(matches!(
            expand_staged(RECIPES, &outside, &admission),
            Err(Refusal::OutOfRule(name, _)) if name == "config"
        ));
        let mut smuggled = effect.clone();
        smuggled.params.insert(
            "extra".into(),
            Param::Path(PathBuf::from("/private/var/root")),
        );
        assert!(matches!(
            expand_staged(RECIPES, &smuggled, &admission),
            Err(Refusal::Undeclared(name)) if name == "extra"
        ));
        let mut program = effect.clone();
        program
            .params
            .insert("node".into(), Param::Executable("tsc".into()));
        assert!(matches!(
            expand_staged(RECIPES, &program, &admission),
            Err(Refusal::UnknownSlot(_))
        ));
        let mut cache = effect;
        cache.params.insert(
            "adapter".into(),
            Param::Path(layout.base.join("elsewhere.js")),
        );
        assert!(matches!(
            expand_staged(RECIPES, &cache, &admission),
            Err(Refusal::OutOfRule(name, _)) if name == "adapter"
        ));
    }

    /// Only a `tsc` run of this checker is a recipe request; another shape is never asked.
    #[test]
    fn foreign_runs_are_not_recipe_requests() {
        let (layout, request, section, node, tsc) = layout("foreign");
        let (mut spec, ..) = expansion(&layout, &request, section, node, tsc);
        spec.args.truncate(3);
        assert_eq!(
            tsc_effect(&spec).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    /// A checker without staging writes nothing into the cache before its run, and a refused run
    /// reaches the check as an error rather than a spawn.
    #[tokio::test]
    async fn the_module_checker_writes_nothing_itself() {
        use agent_ide_core::checks::Checker;
        let (layout, request, _, node, tsc) = layout("nowrite");
        let checker = TypeScriptChecker::new(
            Arc::new(FakeRunner::default()),
            node,
            tsc,
            Duration::from_secs(5),
        )
        .without_staging();
        let _ = checker.check(request.clone()).await;
        assert!(
            !request.cache_dir.exists(),
            "no staging by the module: {}",
            layout.base.display()
        );
    }

    /// Provider and checks `describe` reject malformed input as typed errors, and the presence
    /// answer follows the root config.
    #[test]
    fn describe_validates_its_inputs() {
        assert!(
            describe_checks(serde_json::json!({"node": "relative", "tsc_cli": "/x/tsc.js"}))
                .unwrap()
                .valid
                .eq(&false)
        );
        assert!(describe_checks(serde_json::json!({"nope": 1})).is_err());
        assert!(describe_provider(serde_json::json!({"executable": 1})).is_err());
        let (layout, ..) = layout("presence");
        let worktree = layout.base.join("outer/ws");
        assert!(TypeScriptChecks.is_present(&worktree));
        assert!(!TypeScriptChecks.is_present(&layout.base));
    }

    /// The formatter and both syntax-probe argument vectors the in-process support builds become
    /// requests of the declared recipes that expand to exactly those argument vectors, with the
    /// tool's own directory leading `PATH`; anything else is no request, and an unadmitted Node
    /// is refused.
    #[test]
    fn interactive_plans_expand_to_the_in_process_commands() {
        let (layout, ..) = layout("interactive");
        let worktree = layout.base.join("outer/ws");
        let cache = layout.base.join("cache");
        let home = layout.base.join("home");
        let tools_dir = layout.base.join("tools/bin");
        let node = tools_dir.join("node");
        let npx = tools_dir.join("npx");
        let roots = vec![layout.base.join("tools")];
        let programs = vec![("npx".to_owned(), npx), ("node".to_owned(), node.clone())];
        let admission = agent_ide_core::modules::recipe::Admission {
            worktree: &worktree,
            cache_dir: &cache,
            read_denies: &[],
            home: Some(&home),
            launcher_roots: &roots,
            developer_dirs: &[],
            programs: &programs,
            timeout: Duration::from_secs(10),
        };
        let module = layout
            .base
            .join("tools/lib/node_modules/typescript/lib/typescript.js");
        let (node_text, module_text) = (node.display().to_string(), module.display().to_string());
        let probe = crate::support::TS_PROBE;
        for argv in [
            vec!["npx", "prettier", "--stdin-filepath", "src/a.ts"],
            vec![
                node_text.as_str(),
                "-e",
                probe,
                module_text.as_str(),
                "a.tsx",
            ],
            vec!["node", "-e", probe, module_text.as_str(), "a.mjs"],
        ] {
            let argv: Vec<String> = argv.into_iter().map(str::to_owned).collect();
            let effect = interactive_effect(&argv).expect("a recipe request");
            let spec = agent_ide_core::modules::recipe::expand(RECIPES, &effect, &admission)
                .unwrap_or_else(|refusal| panic!("{argv:?}: {refusal:?}"));
            let program = if argv[0].starts_with('/') {
                PathBuf::from(&argv[0])
            } else {
                tools_dir.join(&argv[0])
            };
            assert_eq!(spec.program, program, "{argv:?}");
            assert_eq!(
                spec.args,
                argv[1..]
                    .iter()
                    .map(std::ffi::OsString::from)
                    .collect::<Vec<_>>(),
                "{argv:?}"
            );
            assert_eq!(spec.cwd, worktree);
            if !argv[0].starts_with('/') {
                let path = spec.env.iter().find(|(key, _)| key == "PATH").unwrap();
                assert!(
                    path.1.starts_with(&tools_dir.display().to_string()),
                    "{path:?}"
                );
            }
        }
        assert!(interactive_effect(&["sh".into(), "-c".into(), "x".into()]).is_none());
        let outside = interactive_effect(&[
            "/private/var/root/node".into(),
            "-e".into(),
            probe.into(),
            module_text,
            "a.ts".into(),
        ])
        .unwrap();
        assert!(matches!(
            agent_ide_core::modules::recipe::expand(RECIPES, &outside, &admission),
            Err(Refusal::OutOfRule(name, _)) if name == "node_path"
        ));
    }
}
