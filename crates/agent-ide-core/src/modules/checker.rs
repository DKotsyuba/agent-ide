//! A project checker whose language computes in its module. The module plans each check and
//! interprets its output; every process the check needs is an effect recipe of the language's
//! descriptor that the core admits ([`recipe::expand`]) and runs through the same confined
//! runner an in-process checker uses, with the same capture ceilings.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde_json::Value;

use super::{
    contract::{Capability, Cause, Fence, ModuleUnavailable, Role},
    host::{Call, EffectRunner},
    payload::{
        CheckPlanRequest, ChecksDescription, DescribeQuery, EffectOutcome, EffectRequest, encode,
    },
    recipe::{self, Described},
    router::ModuleHost,
    wire::Attachment,
};
use crate::{
    checks::{
        BoxFuture, CheckRequest, Checker, ProblemSnapshot, UnavailableReason,
        runner::{ConfinedRunner, RunOutput},
    },
    lang::Language,
};

/// Time a module gets beyond the check's own timeout to start, plan and interpret.
const PLAN_MARGIN: Duration = Duration::from_secs(60);

/// The [`Checker`] of a language in module mode.
pub struct ModuleChecker {
    /// The checked language.
    language: Language,
    /// The daemon's module routing.
    host: Arc<ModuleHost>,
    /// The language's raw `project_checks` section.
    section: Value,
    /// The confined runner every check process goes through.
    runner: Arc<dyn ConfinedRunner>,
    /// Total wall-clock ceiling of one check.
    timeout: Duration,
    /// Immutable accepted factory facts; each run fills its own actual worktree before sending
    /// them. Unknown provider evidence remains absent, never replaced with ambient paths.
    configuration: super::contract::ModuleConfig,
    /// The module's `describe` answer for the section, asked once.
    described: tokio::sync::OnceCell<Described>,
}

impl ModuleChecker {
    /// The checker of `language` with its launcher `section`.
    pub fn new(
        language: Language,
        host: Arc<ModuleHost>,
        section: Value,
        runner: Arc<dyn ConfinedRunner>,
        timeout: Duration,
    ) -> Self {
        Self {
            configuration: super::contract::ModuleConfig {
                checks: Some(section.clone()),
                ..Default::default()
            },
            language,
            host,
            section,
            runner,
            timeout,
            described: tokio::sync::OnceCell::new(),
        }
    }

    /// Supplies accepted immutable launcher facts before sharing this checker. Run requests clone
    /// them and set the admitted worktree; no cached module hello becomes current authority.
    pub fn with_config(mut self, config: super::contract::ModuleConfig) -> Self {
        self.configuration = config;
        self
    }

    /// The module's `describe` answer for the section as admission inputs: named programs,
    /// launcher roots and developer directories. A refused section describes nothing.
    async fn described(&self, worktree: &Path) -> Result<&Described, ModuleUnavailable> {
        self.described
            .get_or_try_init(|| async {
                let description: Result<ChecksDescription, String> = self
                    .host
                    .call(
                        self.language,
                        worktree,
                        Role::Checker,
                        call(
                            Capability::Describe,
                            worktree,
                            encode(&DescribeQuery::Checks {
                                section: self.section.clone(),
                            }),
                        ),
                        super::router::budget_or(PLAN_MARGIN),
                        &mut super::host::NoEffects,
                    )
                    .await?;
                let mut described = description
                    .map(|description| Described::new(&description))
                    .unwrap_or_default();
                // The section's overrides first, then the core's own platform resolution.
                for dir in recipe::platform_developer_dirs() {
                    if !described.developer_dirs.contains(&dir) {
                        described.developer_dirs.push(dir);
                    }
                }
                Ok(described)
            })
            .await
    }

    /// Runs one check, or returns its typed module failure. Builds admission roots from this
    /// request's current module configuration and read denies before the module plans effects.
    async fn run(&self, request: &CheckRequest) -> Result<ProblemSnapshot, ModuleUnavailable> {
        let mut configuration = self.configuration.clone();
        configuration.worktree = Some(request.worktree.clone());
        configuration.read_denies = request.read_denies.clone();
        let mut described = self.described(&request.worktree).await?.clone();
        described.launcher_roots.extend(recipe::environment_roots(
            self.language.name(),
            &request.worktree,
            &configuration,
        ));
        described.launcher_roots.extend(
            self.language
                .support()
                .configured_environments(&request.worktree, &configuration)
                .into_iter()
                .filter(|environment| environment.missing_next_step.is_none())
                .filter_map(|environment| environment.chosen.map(|candidate| candidate.path)),
        );
        let mut effects = CheckEffects {
            recipes: recipe::declared(self.language.name()),
            request,
            home: crate::userhome::user_home(),
            described: &described,
            timeout: self.timeout,
            runner: self.runner.as_ref(),
        };
        self.host
            .call(
                self.language,
                &request.worktree,
                Role::Checker,
                call(
                    Capability::CheckPlan,
                    &request.worktree,
                    encode(&CheckPlanRequest {
                        module_config: configuration,
                        request: request.clone(),
                        config: self.section.clone(),
                        timeout_ms: self.timeout.as_millis() as u64,
                        // The module plans in the current environment: its own copy of the
                        // selections may predate the last `ide.start environment` change.
                        selections: crate::lang::environment::selections(
                            &request.worktree,
                            self.language,
                        ),
                    }),
                ),
                self.timeout + super::router::budget_or(PLAN_MARGIN),
                &mut effects,
            )
            .await
    }
}

/// One request to the checker instance of `worktree`.
fn call(capability: Capability, worktree: &Path, payload: Value) -> Call {
    Call {
        capability,
        scope_key: worktree.display().to_string(),
        revision_key: String::new(),
        payload,
        attachments: Vec::new(),
    }
}

impl Checker for ModuleChecker {
    fn language(&self) -> Language {
        self.language
    }

    fn check(&self, request: CheckRequest) -> BoxFuture<'_, ProblemSnapshot> {
        Box::pin(async move {
            match self.run(&request).await {
                Ok(snapshot) => snapshot,
                Err(failure) => ProblemSnapshot::unavailable_with_detail(
                    self.language,
                    if failure.cause == Cause::ToolMissing {
                        UnavailableReason::ToolMissing
                    } else {
                        UnavailableReason::Fatal
                    },
                    request.input_generation,
                    0,
                    Some(failure.to_string()),
                ),
            }
        })
    }
}

/// Serves the effects of one check: expands each recipe under the core's own admission and runs
/// it through the confined runner.
struct CheckEffects<'a> {
    /// The language's declared recipes.
    recipes: &'static [crate::modules::payload::EffectRecipe],
    /// The check being served.
    request: &'a CheckRequest,
    /// The real user home.
    home: Option<PathBuf>,
    /// The module's description of the section.
    described: &'a Described,
    /// The check's own timeout.
    timeout: Duration,
    /// The confined runner.
    runner: &'a dyn ConfinedRunner,
}

/// Effect ids minted by this daemon.
static EFFECTS: AtomicU64 = AtomicU64::new(0);

impl EffectRunner for CheckEffects<'_> {
    fn run<'a>(
        &'a mut self,
        _fence: &'a Fence,
        effect: EffectRequest,
    ) -> BoxFuture<'a, (EffectOutcome, Vec<Attachment>)> {
        Box::pin(async move {
            let admission = self.described.admission(
                &self.request.worktree,
                &self.request.cache_dir,
                &self.request.read_denies,
                self.home.as_deref(),
                self.timeout,
            );
            let spec = match recipe::expand_staged(self.recipes, &effect, &admission) {
                Ok((spec, staged)) => {
                    if let Err(error) = recipe::stage(&self.request.cache_dir, &staged) {
                        return (
                            EffectOutcome::Refused {
                                cause: Cause::Exited,
                                message: format!("staging recipe assets: {}", error.kind()),
                            },
                            Vec::new(),
                        );
                    }
                    spec
                }
                Err(refusal) => {
                    return (
                        EffectOutcome::Refused {
                            cause: Cause::PolicyRefused,
                            message: format!("{refusal:?}"),
                        },
                        Vec::new(),
                    );
                }
            };
            let failed_run = (spec.program.clone(), spec.args.clone(), spec.env.clone());
            match self.runner.run(spec).await {
                Ok(output) => {
                    if output.status != Some(0) {
                        record_failed_run(&self.request.cache_dir, &failed_run, &output);
                    }
                    completed(output)
                }
                Err(error) => (
                    EffectOutcome::Refused {
                        cause: if error.kind() == std::io::ErrorKind::NotFound {
                            Cause::ToolMissing
                        } else {
                            Cause::Exited
                        },
                        message: error.kind().to_string(),
                    },
                    Vec::new(),
                ),
            }
        })
    }
}

/// File in a check's private cache that holds the operator diagnostic of its last failed run.
pub const FAILED_RUN_LOG: &str = "last-failed-run.log";

/// Writes the operator diagnostic of one failed check run to [`FAILED_RUN_LOG`] in `cache_dir`:
/// program, arguments, the complete (allowlisted) environment, exit status and the whole captured
/// stderr. The agent's reply keeps only its bounded cause line; this file lets an operator see
/// the underlying error (for example a build script's own output). Best effort, owner-only, and
/// replaced by the next failed run; a write failure never changes the check result.
fn record_failed_run(
    cache_dir: &std::path::Path,
    (program, args, env): &(PathBuf, Vec<std::ffi::OsString>, Vec<(String, String)>),
    output: &RunOutput,
) {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut text = format!(
        "status: {:?}\nprogram: {}\nargs:",
        output.status,
        program.display()
    );
    for arg in args {
        text.push(' ');
        text.push_str(&arg.to_string_lossy());
    }
    text.push_str("\nenv:\n");
    for (name, value) in env {
        text.push_str(&format!("  {name}={value}\n"));
    }
    text.push_str("stderr:\n");
    let path = cache_dir.join(FAILED_RUN_LOG);
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut file| {
            file.write_all(text.as_bytes())?;
            file.write_all(&output.stderr)
        });
}

/// The outcome and output attachments (stdout 1, stderr 2) of a completed run.
fn completed(output: RunOutput) -> (EffectOutcome, Vec<Attachment>) {
    let outcome = EffectOutcome::Completed {
        effect_id: format!("e{}", EFFECTS.fetch_add(1, Ordering::Relaxed) + 1),
        status: output.status,
        timed_out: output.timed_out,
        truncated: output.truncated,
        stdout_bytes: output.stdout.len() as u64,
        stderr_bytes: output.stderr.len() as u64,
    };
    (
        outcome,
        vec![
            Attachment::octets(1, output.stdout),
            Attachment::octets(2, output.stderr),
        ],
    )
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Mutex};

    use super::*;
    use crate::{
        checks::runner::RunSpec,
        modules::payload::{Arg, EffectRecipe, ExecutableSlot, Param, RunClass, SlotSource, Stdin},
    };

    /// Runs the launcher program `tool` with one literal argument.
    const RECIPES: &[EffectRecipe] = &[EffectRecipe {
        id: "run",
        program: "tool",
        args: &[Arg::Literal("--json")],
        env: &[],
        paths: &[],
        executables: &[ExecutableSlot {
            name: "tool",
            source: SlotSource::Launcher("tool"),
        }],
        stdin: Stdin::Null,
        class: RunClass::Background,
        timeout_ceiling_ms: 900_000,
        capture_bytes: 64 << 20,
        assets: &[],
    }];

    /// A failed run leaves its whole diagnostic in the cache, owner-only, replacing the last.
    #[test]
    fn a_failed_run_records_its_diagnostic_in_the_cache() {
        let cache = crate::scratch::ScratchDir::new("failed-run-log");
        let run = (
            PathBuf::from("/bin/tool"),
            vec![std::ffi::OsString::from("check")],
            vec![("CC".to_owned(), "/x/clang".to_owned())],
        );
        let output = |stderr: &[u8]| RunOutput {
            status: Some(101),
            stderr: stderr.to_vec(),
            ..RunOutput::default()
        };
        record_failed_run(&cache, &run, &output(b"first"));
        record_failed_run(
            &cache,
            &run,
            &output(b"--- stderr\nfatal: header not found\n"),
        );
        let path = cache.join(FAILED_RUN_LOG);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("status: Some(101)\nprogram: /bin/tool\nargs: check\n"));
        assert!(text.contains("  CC=/x/clang\n"));
        assert!(text.ends_with("stderr:\n--- stderr\nfatal: header not found\n"));
        assert!(!text.contains("first"));
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    /// Records every spec and answers a fixed failed run.
    #[derive(Default)]
    struct Recording(Mutex<Vec<RunSpec>>);

    impl ConfinedRunner for Recording {
        fn run(&self, spec: RunSpec) -> BoxFuture<'_, std::io::Result<RunOutput>> {
            self.0.lock().unwrap().push(spec);
            Box::pin(async {
                Ok(RunOutput {
                    status: Some(1),
                    stdout: b"out".to_vec(),
                    stderr: b"err".to_vec(),
                    timed_out: false,
                    truncated: false,
                })
            })
        }
    }

    /// An admitted recipe runs through the runner with the core's working directory and returns
    /// its outcome with stdout and stderr attachments; an unknown recipe is refused unrun.
    #[tokio::test]
    async fn effects_run_admitted_recipes_and_refuse_the_rest() {
        let request = CheckRequest {
            worktree: PathBuf::from("/work/tree"),
            cache_dir: PathBuf::from("/cache/alpha"),
            input_generation: 7,
            read_denies: Vec::new(),
        };
        let runner = Recording::default();
        let described = Described {
            programs: vec![("tool".to_owned(), PathBuf::from("/bin/sh"))],
            launcher_roots: vec![PathBuf::from("/bin")],
            developer_dirs: Vec::new(),
        };
        let mut effects = CheckEffects {
            recipes: RECIPES,
            request: &request,
            home: None,
            described: &described,
            timeout: Duration::from_secs(60),
            runner: &runner,
        };
        let fence = Fence {
            instance: 1,
            request_id: 1,
            scope_key: String::new(),
            revision_key: String::new(),
        };
        let effect = |recipe: &str| EffectRequest {
            recipe: recipe.to_owned(),
            params: BTreeMap::from([("tool".to_owned(), Param::Executable("tool".to_owned()))]),
        };
        let (outcome, output) = effects.run(&fence, effect("run")).await;
        assert!(matches!(
            outcome,
            EffectOutcome::Completed {
                status: Some(1),
                stdout_bytes: 3,
                stderr_bytes: 3,
                ..
            }
        ));
        assert_eq!(
            output
                .iter()
                .map(|attachment| (attachment.id, attachment.bytes.as_slice()))
                .collect::<Vec<_>>(),
            [(1, &b"out"[..]), (2, &b"err"[..])]
        );
        {
            let specs = runner.0.lock().unwrap();
            assert_eq!(specs[0].program, PathBuf::from("/bin/sh"));
            assert_eq!(specs[0].args, ["--json"]);
            assert_eq!(specs[0].cwd, request.worktree);
        }
        let (outcome, output) = effects.run(&fence, effect("other")).await;
        assert!(matches!(
            outcome,
            EffectOutcome::Refused {
                cause: Cause::PolicyRefused,
                ..
            }
        ));
        assert!(output.is_empty());
        assert_eq!(runner.0.lock().unwrap().len(), 1, "nothing ran");
    }
}
