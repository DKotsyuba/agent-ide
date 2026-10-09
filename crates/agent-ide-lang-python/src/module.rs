//! The Python module: what `agent-ide module python <role>` serves. The analyzer role hosts the
//! Pyright session the core granted beside the language's own support; the module computes the
//! interpreter and import roots itself.

use std::{collections::BTreeMap, io, path::Path, path::PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use agent_ide_core::{
    checks::{
        BoxFuture,
        runner::{ConfinedRunner, RunOutput, RunSpec},
    },
    intelligence::session::ProviderSettings,
    modules::{
        adapter::SupportServer,
        contract::{Capability, CapabilityDecl, Declaration, ErrorCode, Role, Support},
        payload::{
            Arg, CheckPlanRequest, ChecksDescription, DescribeQuery, EffectOutcome, EffectRecipe,
            EffectRequest, EnvRule, ExecutableSlot, NamedProgram, Param, PathRole, PathRule,
            RunClass, SlotSource, Stdin, decode, encode,
        },
        provider::{ProviderBuilder, ProviderLaunchPlan, ProviderServer},
        serve::{Answer, Effects, Incoming, ModuleServer, ServeError, serve_stdio},
        wire::Attachment,
    },
};

use crate::profile::{PyrightProfile, PyrightProfileIdentity};

/// The launcher-accepted Pyright identities the core hands its analyzer module
/// (`hello.config.provider.settings`); the module re-measures both files before starting them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalyzerSettings {
    /// Accepted Pyright language-server script.
    pub binary: PathBuf,
    /// Its accepted BLAKE3 digest, hex.
    pub script_digest: String,
    /// Accepted Pyright version identity.
    pub version: String,
    /// Accepted Node executable.
    pub node: PathBuf,
    /// Its accepted BLAKE3 digest, hex.
    pub node_digest: String,
    /// Accepted Node identity.
    pub node_identity: String,
    /// Operator trust identity.
    pub trust: String,
    /// Private per-worktree cache namespace.
    pub cache_namespace: String,
}

/// Plans the Pyright session exactly as the in-process backend starts it.
#[derive(Default)]
struct Pyright {
    /// The planned session found no interpreter: its import flood is summarized once, as the
    /// in-process backend does.
    no_interpreter: std::sync::atomic::AtomicBool,
}

impl ProviderBuilder for Pyright {
    fn plan(
        &self,
        worktree: &Path,
        settings: &Value,
    ) -> io::Result<(ProviderSettings, ProviderLaunchPlan)> {
        let settings: AnalyzerSettings = serde_json::from_value(settings.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pyright settings"))?;
        let digest = |hex: &str| {
            blake3::Hash::from_hex(hex)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pyright digest"))
        };
        let (interpreter, environment) = crate::environment::session(worktree);
        self.no_interpreter
            .store(interpreter.is_none(), std::sync::atomic::Ordering::Relaxed);
        let profile = PyrightProfile::new(PyrightProfileIdentity {
            binary: settings.binary.clone(),
            accepted_script_digest: digest(&settings.script_digest)?,
            version: settings.version,
            node: settings.node.clone(),
            accepted_node_digest: digest(&settings.node_digest)?,
            node_identity: settings.node_identity,
            trust: settings.trust,
            cache_namespace: settings.cache_namespace.clone(),
        })
        .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "pyright profile refused"))?
        .with_interpreter(interpreter, environment)
        .with_extra_paths(crate::support::import_roots(worktree));
        let node_parent = settings
            .node
            .parent()
            .filter(|path| path.is_absolute())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "node has no parent"))?;
        let plan = ProviderLaunchPlan {
            program: settings.node.clone(),
            args: vec![settings.binary.display().to_string(), "--stdio".to_owned()],
            env: BTreeMap::from([
                ("PATH".to_owned(), node_parent.display().to_string()),
                (
                    "TMPDIR".to_owned(),
                    Path::new(&settings.cache_namespace)
                        .join("tmp")
                        .display()
                        .to_string(),
                ),
            ]),
            reads: vec![settings.binary],
        };
        Ok((ProviderSettings::new(profile), plan))
    }

    fn diagnostics(
        &self,
        snapshot: &mut agent_ide_core::intelligence::session::DiagnosticSnapshot,
    ) {
        if self
            .no_interpreter
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            crate::backend::summarize_missing_environment(snapshot);
        }
    }
}

/// Serves `agent-ide module python <role>` over this process's stdin and stdout until the core
/// closes it.
pub async fn serve(role: Role) -> Result<(), ServeError> {
    let support = SupportServer::new(crate::LANGUAGE, env!("CARGO_PKG_VERSION"));
    match role {
        Role::Analyzer => serve_stdio(ProviderServer::new(support, Pyright::default()), role).await,
        Role::Checker => serve_stdio(CheckerServer { support }, role).await,
    }
}

/// Install prefixes a project interpreter may live under beyond the worktree and the user's home
/// (Homebrew, MacPorts and conda under `/opt`, `/usr/local`, the system and python.org
/// frameworks, developer tools): the roots the Pyright recipe admits an interpreter and its
/// installation prefix from. An interpreter anywhere else is refused, never granted.
const INTERPRETER_PREFIXES: [&str; 6] = [
    "/opt",
    "/usr/local",
    "/usr",
    "/Library/Frameworks",
    "/Library/Developer",
    "/Applications",
];

/// Roles of a project interpreter and of the installation prefixes Pyright reads.
const INTERPRETER_ROLES: &[PathRole] = &[
    PathRole::Worktree,
    PathRole::HomeRelative,
    PathRole::LauncherRoot,
    PathRole::DeveloperDir,
];

/// The effect recipes of the Python module, declared by the root.
pub const RECIPES: &[EffectRecipe] = &[EffectRecipe {
    id: "pyright",
    program: "node",
    args: &[
        Arg::Param("pyright_cli"),
        Arg::Literal("--outputjson"),
        Arg::Literal("--project"),
        Arg::Param("project"),
        Arg::Literal("--pythonpath"),
        Arg::Param("interpreter"),
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
    ],
    paths: &[
        PathRule {
            param: "worktree",
            roles: &[PathRole::WorktreeRoot],
            existing_only: false,
            read_root: true,
        },
        PathRule {
            param: "project_root",
            roles: &[PathRole::Worktree],
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
            param: "pyright_root",
            roles: &[PathRole::LauncherRoot],
            existing_only: false,
            read_root: true,
        },
        PathRule {
            param: "venv_root",
            roles: INTERPRETER_ROLES,
            existing_only: false,
            read_root: true,
        },
        PathRule {
            param: "base_prefix",
            roles: INTERPRETER_ROLES,
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
            param: "pyright_cli",
            roles: &[PathRole::LauncherRoot],
            existing_only: false,
            read_root: false,
        },
        PathRule {
            param: "project",
            roles: &[PathRole::Worktree],
            existing_only: false,
            read_root: false,
        },
        PathRule {
            param: "interpreter",
            roles: INTERPRETER_ROLES,
            existing_only: false,
            read_root: false,
        },
        PathRule {
            param: "node_bin",
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
    ],
    executables: &[ExecutableSlot {
        name: "node",
        source: SlotSource::Launcher("node"),
    }],
    stdin: Stdin::Null,
    class: RunClass::Background,
    timeout_ceiling_ms: 900_000,
    capture_bytes: 64 << 20,
}];

/// The `pyright` recipe request that reproduces `spec`, a run [`PythonChecker`] built for
/// `request` ([`PythonChecker::pyright_spec_for_root`]): the module names its inputs, the core
/// admits each one and rebuilds the run itself.
///
/// [`PythonChecker`]: crate::checks::PythonChecker
/// [`PythonChecker::pyright_spec_for_root`]: crate::checks::PythonChecker::pyright_spec_for_root
pub fn pyright_effect(spec: &RunSpec) -> io::Result<EffectRequest> {
    let malformed = || io::Error::new(io::ErrorKind::InvalidData, "not a pyright run");
    let roots = &spec.read_roots;
    let (worktree, project_root, rest) = match roots.len() {
        6 => (&roots[0], None, &roots[1..]),
        7 => (&roots[0], Some(&roots[1]), &roots[2..]),
        _ => return Err(malformed()),
    };
    let [node_root, pyright_root, venv_root, base_prefix, etc] = rest else {
        return Err(malformed());
    };
    let [cli, _, _, project, _, interpreter] = spec.args.as_slice() else {
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
    let path = |path: &Path| Param::Path(path.to_path_buf());
    let mut params = BTreeMap::from([
        ("node".to_owned(), Param::Executable("node".to_owned())),
        ("worktree".to_owned(), path(worktree)),
        ("node_root".to_owned(), path(node_root)),
        ("pyright_root".to_owned(), path(pyright_root)),
        ("venv_root".to_owned(), path(venv_root)),
        ("base_prefix".to_owned(), path(base_prefix)),
        ("etc".to_owned(), path(etc)),
        ("pyright_cli".to_owned(), Param::Path(PathBuf::from(cli))),
        ("project".to_owned(), Param::Path(PathBuf::from(project))),
        (
            "interpreter".to_owned(),
            Param::Path(PathBuf::from(interpreter)),
        ),
        ("node_bin".to_owned(), Param::Paths(vec![node_bin])),
        ("tmp".to_owned(), Param::Path(PathBuf::from(env("TMPDIR")?))),
    ]);
    if let Some(project_root) = project_root {
        params.insert("project_root".to_owned(), path(project_root));
    }
    Ok(EffectRequest {
        recipe: "pyright".to_owned(),
        params,
    })
}

/// The describe answer for a `project_checks` python section: Node and the Pyright CLI as named
/// programs, their install roots and the interpreter prefixes as launcher roots.
pub fn describe_checks(section: Value) -> Result<ChecksDescription, String> {
    use agent_ide_core::checks::LanguageChecks;
    let config = crate::checks::PythonChecks
        .parse_config(section)
        .map_err(|error| error.to_string())?;
    let config = config
        .downcast_ref::<crate::checks::ProjectPythonChecksConfig>()
        .ok_or("python section")?;
    let parent = |path: &Path| path.parent().unwrap_or(path).to_path_buf();
    Ok(ChecksDescription {
        valid: agent_ide_core::checks::CheckConfig::validate(config),
        programs: vec![
            NamedProgram {
                name: "node".to_owned(),
                path: config.node().to_path_buf(),
                interpreter: None,
            },
            NamedProgram {
                name: "pyright".to_owned(),
                path: config.pyright_cli().to_path_buf(),
                interpreter: Some(config.node().to_path_buf()),
            },
        ],
        launcher_roots: [parent(&parent(config.node())), parent(config.pyright_cli())]
            .into_iter()
            .chain(INTERPRETER_PREFIXES.iter().map(PathBuf::from))
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

/// The checker role: the language's own [`PythonChecker`] plans and parses each check; every
/// pyright run it needs goes to the core as a `pyright` recipe request.
///
/// [`PythonChecker`]: crate::checks::PythonChecker
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
        use agent_ide_core::checks::LanguageChecks;
        let config = crate::checks::PythonChecks
            .parse_config(query.config)
            .map_err(|error| ServeError::Protocol(error.to_string()))?;
        let (runs, mut pending) = tokio::sync::mpsc::unbounded_channel();
        let checker = config.checker(
            std::sync::Arc::new(Bridge(runs)),
            std::time::Duration::from_millis(query.timeout_ms),
        );
        let check = checker.check(query.request);
        tokio::pin!(check);
        let snapshot = loop {
            tokio::select! {
                snapshot = &mut check => break snapshot,
                Some((spec, reply)) = pending.recv() => {
                    let output = match pyright_effect(&spec) {
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
            Capability::Describe => match decode::<DescribeQuery>(request.payload) {
                Ok(DescribeQuery::Checks { section }) => {
                    Ok(Answer::result(encode(&describe_checks(section))))
                }
                Ok(DescribeQuery::Presence { worktree }) => Ok(Answer::result(encode(
                    &crate::support::is_python_project(&worktree),
                ))),
                _ => Ok(Answer::error(ErrorCode::Unsupported, "describe")),
            },
            _ => self.support.call(request, effects).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use agent_ide_core::{
        checks::CheckRequest,
        modules::recipe::{Described, Refusal, expand},
    };

    use super::*;

    /// A scratch layout: a worktree whose `.venv` interpreter links into a home-managed Python,
    /// and a Node install with the Pyright CLI.
    struct Layout {
        /// Removed on drop.
        base: PathBuf,
    }

    impl Drop for Layout {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// The layout, the check request, the section and the venv interpreter.
    fn layout(tag: &str) -> (Layout, CheckRequest, Value, PathBuf) {
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("python-recipe-{tag}-{}", std::process::id()));
        let worktree = base.join("ws");
        let python = base.join("home/.local/share/uv/python/cpython-3.14/bin/python3.14");
        let node = base.join("tools/bin/node");
        let cli = base.join("tools/lib/node_modules/pyright/index.js");
        for dir in [
            worktree.join(".venv/bin"),
            python.parent().unwrap().to_path_buf(),
            node.parent().unwrap().to_path_buf(),
            cli.parent().unwrap().to_path_buf(),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        for file in [&python, &node, &cli] {
            std::fs::write(file, "").unwrap();
        }
        std::fs::write(worktree.join("pyrightconfig.json"), "{}").unwrap();
        let interpreter = worktree.join(".venv/bin/python");
        std::os::unix::fs::symlink(&python, &interpreter).unwrap();
        let request = CheckRequest {
            worktree: worktree.clone(),
            cache_dir: base.join("cache"),
            input_generation: 1,
            read_denies: Vec::new(),
        };
        let section = serde_json::json!({"node": node, "pyright_cli": cli});
        (Layout { base }, request, section, interpreter)
    }

    /// The run the core rebuilds from the module's `pyright` request — admitted through the
    /// module's describe answer (Describe → Admission → expansion) — equals exactly the run the
    /// in-process checker builds for the same inputs, for the worktree root and a nested root.
    #[test]
    fn pyright_recipe_reproduces_the_in_process_run() {
        let (layout, request, section, interpreter) = layout("equal");
        let checker = crate::checks::PythonChecker::new(
            std::sync::Arc::new(agent_ide_core::checks::runner::FakeRunner::default()),
            PathBuf::from(section["node"].as_str().unwrap()),
            PathBuf::from(section["pyright_cli"].as_str().unwrap()),
            Duration::from_secs(600),
        );
        let described = Described::new(&describe_checks(section).unwrap());
        let home = layout.base.join("home");
        let admission = described.admission(
            &request.worktree,
            &request.cache_dir,
            &[],
            Some(&home),
            Duration::from_secs(600),
        );
        let nested = request.worktree.join("pkg");
        std::fs::create_dir_all(&nested).unwrap();
        for root in [request.worktree.clone(), nested] {
            let mut spec = checker.pyright_spec_for_root(&request, &root, &interpreter);
            // The in-process run names the real user home; the admission names the fixture's.
            for (key, value) in &mut spec.env {
                if key == "HOME" {
                    *value = home.display().to_string();
                }
            }
            let rebuilt = expand(RECIPES, &pyright_effect(&spec).unwrap(), &admission).unwrap();
            assert_eq!(rebuilt, spec, "root {}", root.display());
        }
    }

    /// A request widened beyond the recipe is refused before anything runs: an interpreter
    /// outside every interpreter root, a smuggled extra read root, a program the section does not
    /// name.
    #[test]
    fn widened_pyright_requests_are_refused() {
        let (layout, request, section, interpreter) = layout("widen");
        let checker = crate::checks::PythonChecker::new(
            std::sync::Arc::new(agent_ide_core::checks::runner::FakeRunner::default()),
            PathBuf::from(section["node"].as_str().unwrap()),
            PathBuf::from(section["pyright_cli"].as_str().unwrap()),
            Duration::from_secs(600),
        );
        let described = Described::new(&describe_checks(section).unwrap());
        let home = layout.base.join("home");
        let admission = described.admission(
            &request.worktree,
            &request.cache_dir,
            &[],
            Some(&home),
            Duration::from_secs(600),
        );
        let spec = checker.pyright_spec_for_root(&request, &request.worktree, &interpreter);
        let effect = pyright_effect(&spec).unwrap();
        let mut outside = effect.clone();
        outside.params.insert(
            "interpreter".into(),
            Param::Path(PathBuf::from("/private/var/root/python")),
        );
        assert!(matches!(
            expand(RECIPES, &outside, &admission),
            Err(Refusal::OutOfRule(name, _)) if name == "interpreter"
        ));
        let mut smuggled = effect.clone();
        smuggled.params.insert(
            "extra".into(),
            Param::Path(PathBuf::from("/private/var/root")),
        );
        assert!(matches!(
            expand(RECIPES, &smuggled, &admission),
            Err(Refusal::Undeclared(name)) if name == "extra"
        ));
        let mut program = effect;
        program
            .params
            .insert("node".into(), Param::Executable("pyright".into()));
        assert!(matches!(
            expand(RECIPES, &program, &admission),
            Err(Refusal::UnknownSlot(_))
        ));
    }
}
