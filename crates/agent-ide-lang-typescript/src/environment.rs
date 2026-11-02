//! The TypeScript project environment of one worktree: which TypeScript compiler and Node the
//! configured consumers actually run, and the native inputs that decide whether an answer
//! about them is still current.
//!
//! The compiler decision is the checker's own (`compiler_fact`, shared branch-for-branch), so
//! the card, the confined check, command activation and effect planning can never name
//! different tools: the operator's accepted compiler when no manifest declares TypeScript, the
//! project's prepared compiler only when its bytes match the qualified TypeScript 5.9.3
//! closure, and the exact refusal cause otherwise — missing, denied and unqualified project
//! tools never fall back to a global one. The declared Node pin (`.nvmrc`,
//! `.node-version`, `engines.node`) is honored against the actual selected checker Node when
//! the pin states full plain versions (a numeric dot-prefix of the admitted version proves it,
//! the way an exact pin resolves); a pin the admitted version does not satisfy refuses every
//! claimed command before admission with [`LangError::Environment`] — a preparation refusal
//! with native recovery, distinct from an unsupported request shape. Range expressions are
//! not interpreted here — they are disclosed as unverifiable by this seam and validated
//! against the actual runtime by the adapter.
//!
//! The [`ResolvedEnv::identity`] carries the accepted facts themselves, not just paths: the
//! whole configured launcher sections (`provider`, `checks` — their accepted digests travel
//! with them, so a same-path re-provisioned operator tool moves the identity), the root
//! manifest, the root TypeScript/JavaScript configs, every lockfile of the manager order
//! (conservative: the active one is whichever exists, and all are hashed), the installed
//! compiler's package manifest and library bytes, and the Node pin files. Config `extends`
//! chains and workspace/member manifests beyond the root are explicitly unsupported here and
//! are not hashed; their scope stays with the checker's own reads until a seam carries it.
//! Inputs hash under the shared bounded streaming gate with the request's own
//! `ModuleConfig::read_denies` — the same exclusions the checker enforces, never a hard-coded
//! empty policy. Nothing is installed, nothing is rewritten, and the environment is not
//! agent-selectable (the default selection refusal stands).

use std::path::{Path, PathBuf};

use serde_json::Value;

use agent_ide_core::execution::seatbelt::ReadDeny;
use agent_ide_core::lang::LangError;
use agent_ide_core::lang::environment::{EnvCandidate, EnvSource, ResolvedEnv};
use agent_ide_core::modules::contract::ModuleConfig;

use crate::checks::{CompilerFact, ProjectTypeScriptChecksConfig, compiler_fact};

/// Fixed prelude, shared with static recipes; it leaves the runner's main-module argv intact.
pub(crate) const RUNNER_PRELUDE: &str = concat!(
    "data:text/javascript,",
    include_str!("version_range.js"),
    include_str!("runner_guard.js")
);

/// The operator-accepted tools of this request, from whichever launcher section carries them:
/// the confined check's Node and TypeScript CLI, or the provider OPTION `node` when no check
/// section configures tools. The measured identity and digest travel only when they belong to
/// the actually selected Node executable.
pub struct ConfiguredTools {
    /// The selected Node executable.
    pub node: PathBuf,
    /// The accepted TypeScript CLI module, when the checks section configures one.
    pub tsc_cli: Option<PathBuf>,
    /// The measured Node identity, when the accepted provider declaration states it for the
    /// selected Node.
    pub node_identity: Option<String>,
    /// The accepted Node binary digest, bound like [`Self::node_identity`].
    pub node_digest: Option<String>,
}

/// Parses [`ConfiguredTools`] from the request's launcher sections. The Node comes from the
/// checks section when one configures tools, else from the provider OPTION `node` (a
/// provider-only semantic launch is still a valid Node source for raw consumers). The
/// TypeScript CLI requires the checks section; `None` tsc means the environment cannot prove
/// a compiler for `tsc` claims.
fn configured_tools(config: &ModuleConfig) -> Option<ConfiguredTools> {
    let checks = config.checks.as_ref().and_then(|section| {
        serde_json::from_value::<ProjectTypeScriptChecksConfig>(section.clone()).ok()
    });
    let provider = config.provider.as_ref();
    // The accepted Node is the provider OPTION `node` (the common `executable` is the bridge);
    // its facts travel only when it is the same file this request's checks configure (or when
    // no checks section selects a different one).
    let accepted_node = provider.and_then(|provider| provider.get("node"));
    let (node, same_node) = match &checks {
        Some(checks) => {
            let node = checks.node().to_path_buf();
            let same = accepted_node
                .and_then(|n| n.get("path"))
                .and_then(Value::as_str)
                .is_some_and(|path| Path::new(path) == checks.node());
            (node, same)
        }
        None => {
            let path = accepted_node
                .and_then(|n| n.get("path"))
                .and_then(Value::as_str)?;
            (PathBuf::from(path), true)
        }
    };
    let fact = |field: &str| {
        same_node
            .then(|| {
                accepted_node
                    .and_then(|n| n.get(field))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .flatten()
    };
    Some(ConfiguredTools {
        node,
        tsc_cli: checks.as_ref().map(|checks| checks.tsc_cli().to_path_buf()),
        node_identity: fact("identity"),
        node_digest: fact("blake3"),
    })
}

/// Whether `worktree` is a TypeScript project by the same membership rule `detect` applies: a
/// root manifest/config (`package.json`, `tsconfig.json`, `tsconfig.*.json`, `jsconfig.json`)
/// or TypeScript-ish root files. `detect` remains the authority; this repeats its rule so the
/// environment view never answers for a worktree the card does not list.
fn is_typescript_project(worktree: &Path) -> bool {
    let Ok(names) = std::fs::read_dir(worktree) else {
        return false;
    };
    let names: Vec<String> = names
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    let has = |name: &str| names.iter().any(|entry| entry == name);
    let manifest = has("package.json")
        || has("tsconfig.json")
        || has("jsconfig.json")
        || names
            .iter()
            .any(|name| name.starts_with("tsconfig.") && name.ends_with(".json"));
    manifest
        || names.iter().any(|name| {
            [".ts", ".tsx", ".mts", ".cts"]
                .iter()
                .any(|extension| name.ends_with(extension))
        })
}

/// `ino.mtime.mtime_nsec.size` of `path` without following a final link, empty when it cannot
/// be read: the same-path mutation signal for selected tools.
fn stamp(path: &Path) -> String {
    std::fs::symlink_metadata(path)
        .map(|metadata| {
            use std::os::unix::fs::MetadataExt;
            format!(
                "{}.{}.{}.{}",
                metadata.ino(),
                metadata.mtime(),
                metadata.mtime_nsec(),
                metadata.len()
            )
        })
        .unwrap_or_default()
}

/// The declared Node pin of `worktree`: `.nvmrc`, then `.node-version`, then `engines.node` —
/// the same lookup order `detect` uses.
fn node_pin(worktree: &Path, denies: &[ReadDeny]) -> Option<String> {
    for name in [".nvmrc", ".node-version"] {
        if let Some(text) =
            agent_ide_core::lang::environment::read_input(&worktree.join(name), denies)
        {
            let version = text.trim();
            if !version.is_empty() {
                return Some(version.to_owned());
            }
        }
    }
    let package: Value = serde_json::from_str(
        &agent_ide_core::lang::environment::read_input(&worktree.join("package.json"), denies)
            .unwrap_or_default(),
    )
    .unwrap_or(Value::Null);
    package
        .get("engines")
        .and_then(|engines| engines.get("node"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// A plain `major[.minor[.patch]]` version, `v` prefix tolerated: `None` for range
/// expressions (`>=20`, `^20`, `*`, `22.x`), which this seam never interprets — the adapter
/// validates ranges against the actual runtime.
fn plain_version(value: &str) -> Option<Vec<u64>> {
    let value = value.trim().trim_start_matches('v');
    let components: Vec<&str> = value.split('.').collect();
    let numeric = !components.is_empty()
        && components.iter().all(|component| {
            !component.is_empty() && component.bytes().all(|b| b.is_ascii_digit())
        });
    if !numeric {
        return None;
    }
    // One atomic parse: an oversized all-digit component refuses the whole version instead of
    // silently dropping it (a dropped component could make any pin look satisfied).
    components
        .iter()
        .map(|component| component.parse::<u64>())
        .collect::<Result<Vec<u64>, _>>()
        .ok()
}

/// The first plain version of a measured identity (`v22.11.0`, `node 22.11.0 (…)`).
fn measured_version(identity: &str) -> Option<Vec<u64>> {
    identity
        .split(|character: char| !character.is_ascii_digit() && character != '.')
        .find_map(plain_version)
}

/// Why the worktree's Node pin obstructs the configured tools, or `None` when it does not: no
/// pin, a pin the admitted Node satisfies (every stated numeric component equals the
/// admitted version's), or a range expression this seam leaves to the adapter's runtime
/// validation (disclosed as unverified, never refused).
fn node_pin_conflict(
    worktree: &Path,
    tools: &ConfiguredTools,
    denies: &[ReadDeny],
) -> Option<String> {
    let admitted = tools.node_identity.as_deref().and_then(measured_version)?;
    let declared = node_pin(worktree, denies)?;
    let components = plain_version(&declared)?;
    let satisfied = components.len() <= admitted.len()
        && components
            .iter()
            .zip(admitted.iter())
            .all(|(declared, admitted)| declared == admitted);
    (!satisfied).then(|| {
        let shown = admitted
            .iter()
            .map(|component| component.to_string())
            .collect::<Vec<_>>()
            .join(".");
        format!(
            "node pin {declared} does not match the configured Node ({shown}); IDE commands \
             refuse the mismatch; use the project's own runtime or align the launcher Node"
        )
    })
}

/// Why the configured consumers must not run for `worktree` under this request's denies, or
/// `None` when they may: the compiler fact's refusal, then the Node pin's. Compiler failures
/// retain their unavailable reason; a Node-pin mismatch is a fatal refusal.
fn obstruction(
    worktree: &Path,
    tools: &ConfiguredTools,
    denies: &[ReadDeny],
) -> Option<(agent_ide_core::checks::UnavailableReason, String)> {
    // Without a configured TypeScript CLI (provider-only launch) the compiler fact cannot be
    // evaluated: no compiler obstruction, but the Node pin still applies.
    let compiler_cause = tools
        .tsc_cli
        .as_ref()
        .and_then(|tsc_cli| compiler_fact(worktree, tsc_cli, denies).err());
    compiler_cause.or_else(|| {
        node_pin_conflict(worktree, tools, denies)
            .map(|cause| (agent_ide_core::checks::UnavailableReason::Fatal, cause))
    })
}

/// The worktree's environment as a [`ResolvedEnv`]: the compiler fact (`CompilerFact`) names, with
/// the exact refusal cause when the declaration cannot be honored or the Node pin obstructs —
/// an apparently valid winner the configured consumers would refuse is never returned. The
/// configured operator compiler when no manifest declares TypeScript; `None` tools (no
/// configured section) resolve nothing with the disclosure that the environment view was
/// given no configured tools. A Node pin this seam cannot verify (a range expression) keeps
/// the winner and discloses the limit.
pub fn environment(
    worktree: &Path,
    tools: Option<&ConfiguredTools>,
    config: &ModuleConfig,
    denies: &[ReadDeny],
) -> ResolvedEnv {
    let cause = tools.and_then(|tools| obstruction(worktree, tools, denies));
    let fact = tools.filter(|_| cause.is_none()).and_then(|tools| {
        tools
            .tsc_cli
            .as_ref()
            .map(|tsc_cli| match compiler_fact(worktree, tsc_cli, denies) {
                Ok(fact) => Ok(fact),
                Err((_, cause)) => Err(cause),
            })
    });
    let chosen = match &fact {
        Some(Ok(CompilerFact::Operator(cli))) => Some(EnvCandidate {
            label: "operator tsc".to_owned(),
            path: cli.clone(),
            version: None,
            broken: false,
        }),
        Some(Ok(CompilerFact::Project { cli, version })) => Some(EnvCandidate {
            label: "project typescript".to_owned(),
            path: cli.clone(),
            version: version.clone(),
            broken: false,
        }),
        _ => None,
    };
    let source = match &fact {
        Some(Ok(CompilerFact::Project { .. })) if chosen.is_some() => Some(EnvSource::Discovered),
        Some(Ok(CompilerFact::Operator(_))) if chosen.is_some() => Some(EnvSource::Launcher),
        _ => None,
    };
    // A pin this seam cannot verify is disclosed, never silently healthy: a range expression
    // is the adapter's runtime validation, and a configured Node without a measured identity
    // proves nothing about the pin. Borrowed before `cause` moves into the
    // missing-environment disclosure below.
    let unverified = match (cause.is_none(), tools, node_pin(worktree, denies)) {
        (true, Some(tools), Some(declared)) => {
            if plain_version(&declared).is_none() {
                Some(format!(
                    "node pin {declared} is a range this environment view cannot verify; the \
                     adapter validates it against the actual runtime"
                ))
            } else if tools.node_identity.is_none() {
                Some(format!(
                    "node pin {declared} cannot be verified: the configured Node has no \
                     measured identity in this request"
                ))
            } else {
                None
            }
        }
        _ => None,
    };
    let warnings = unverified.into_iter().collect();
    let candidates = chosen.iter().cloned().collect();
    let missing_next_step = cause.map(|(_, cause)| cause).or_else(|| {
        (tools.is_none()).then(|| {
            "no configured TypeScript tools were given to the environment view; checks \
                 follow their configured tools and report their own availability"
                .to_owned()
        })
    });
    ResolvedEnv {
        root: PathBuf::new(),
        chosen,
        source,
        // The chosen compiler is the one candidate: the card shows no `choose:` hint, and
        // the language keeps its default selection refusal.
        candidates,
        warnings,
        missing_next_step,
        identity: identity_string(worktree, tools, config, denies),
    }
}

/// Refuses effect planning when this request's configured tools are obstructed — a compiler
/// fact refusal, a known Node-pin conflict, or an authoritative pin that cannot be verified —
/// with the same cause [`command_env`] refuses `tsc`/`node` with, before any recipe is
/// generated or admitted. `Ok(())` when no configured tools exist (nothing to obstruct) or
/// the facts are compatible.
pub fn admit_effects(worktree: &Path, config: &ModuleConfig) -> Result<(), LangError> {
    let Some(tools) = configured_tools(config) else {
        return Ok(());
    };
    // One shared guard for every claimed consumer: a known compiler/pin obstruction, an
    // authoritative pin whose compatibility cannot be verified (no measured Node identity, or
    // a range/alias expression no accepted version establishes), or configured Node bytes
    // that no longer match the accepted declaration — a card warning or a stale digest is not
    // compatibility proof for a raw Node run.
    let cause = obstruction(worktree, &tools, &config.read_denies)
        .map(|(_, cause)| cause)
        .or_else(|| {
            let pin = node_pin(worktree, &config.read_denies)?;
            let unverifiable = || {
                format!(
                    "node pin {pin} cannot be verified against the configured Node in this \
                 request — IDE commands refuse an unverifiable pin; align the launcher Node \
                 or adjust the pin"
                )
            };
            let Some(identity) = tools.node_identity.as_deref() else {
                return Some(unverifiable());
            };
            let Some(admitted) = measured_version(identity) else {
                return Some(unverifiable());
            };
            let Some(components) = plain_version(&pin) else {
                // A range or alias expression no accepted version establishes: raw commands
                // refuse rather than running a Node the pin may reject.
                return Some(format!(
                    "node pin {pin} is a range/alias this request cannot prove against the \
                 configured Node — IDE commands refuse an unproven pin; use native tooling \
                 (the adapter validates ranges for confined checks) or align the pin"
                ));
            };
            let satisfied = components.len() <= admitted.len()
                && components
                    .iter()
                    .zip(admitted.iter())
                    .all(|(declared, admitted)| declared == admitted);
            (!satisfied).then(|| {
                format!(
                    "node pin {pin} does not match the configured Node — IDE commands refuse the \
                 mismatch; use the project's own runtime or align the launcher Node"
                )
            })
        });
    if let Some(cause) = cause {
        return Err(LangError::Environment(cause));
    }
    verify_configured_node(&tools.node, config, &config.read_denies)
        .map_err(|(_, cause)| LangError::Environment(cause))
}

/// Admission of the stdin syntax and outline probes. `Ok(None)` when [`admit_effects`] admits
/// them as raw Node runs; `Ok(Some(vars))` when its only obstruction is a range/alias Node pin
/// and the runner guard prepares the runtime proof instead: the fixed prelude tests every pin
/// with the qualified compiler's VersionRange on the actual Node before the probe loads, and
/// exits without output when one does not hold. Every other refusal stands, and the guard
/// binds only a probe on the configured Node (`program` is `node` or its exact path).
///
/// # Errors
///
/// The [`admit_effects`] refusal when the pin is plain or absent or `program` is another
/// Node, else the runner guard's.
pub fn probe_guard(
    worktree: &Path,
    config: &ModuleConfig,
    program: &str,
) -> Result<Option<Vec<(std::ffi::OsString, std::ffi::OsString)>>, LangError> {
    let refusal = match admit_effects(worktree, config) {
        Ok(()) => return Ok(None),
        Err(refusal) => refusal,
    };
    let ranged =
        node_pin(worktree, &config.read_denies).is_some_and(|pin| plain_version(&pin).is_none());
    let Some(tools) = configured_tools(config)
        .filter(|tools| ranged && (program == "node" || Path::new(program) == tools.node))
    else {
        return Err(refusal);
    };
    let vars = runner_guard(worktree, &tools, config)?;
    // Only the range path prepares requirements, the verifier and the identity; a guard
    // without them would run the probe unproven.
    if vars.len() < 3 {
        return Err(refusal);
    }
    Ok(Some(vars))
}

/// Activation of `program` under this request's configured tools, resolved before any
/// admission: `tsc` runs the winning compiler through the configured Node, `node` runs the
/// configured Node itself. A compiler fact refusal, a known Node-pin mismatch, or an
/// authoritative pin whose compatibility cannot be verified (no measured Node identity) is
/// `Environment` with its cause — never an ambient fallback. Other programs are unclaimed
/// (`Ok(None)`): project scripts run through the package manager as today.
pub fn command_env(
    worktree: &Path,
    program: &str,
    config: &ModuleConfig,
) -> Result<Option<agent_ide_core::lang::environment::CommandEnv>, LangError> {
    // Ownership first: an unclaimed program is never refused by this environment, and an
    // explicitly pathed executable stays the caller's own tool (basename `tsc` alone is the
    // unresolved project compiler this environment claims).
    let claimed = matches!(
        Path::new(program)
            .file_name()
            .and_then(|name| name.to_str()),
        Some("tsc" | "node")
    ) && !program.contains('/');
    if !claimed {
        return Ok(None);
    }
    let Some(tools) = configured_tools(config) else {
        return Ok(None);
    };
    admit_effects(worktree, config)?;
    if program == "tsc" {
        let Some(tsc_cli) = &tools.tsc_cli else {
            return Err(LangError::Environment(
                "no configured TypeScript CLI in this request; the tsc compiler cannot be \
                 proven — configure the checks section or use native tsc"
                    .to_owned(),
            ));
        };
        let compiler = match compiler_fact(worktree, tsc_cli, &config.read_denies) {
            Ok(fact) => fact,
            Err((_, cause)) => return Err(LangError::Environment(cause)),
        };
        let cli = match &compiler {
            CompilerFact::Project { cli, .. } | CompilerFact::Operator(cli) => cli.clone(),
        };
        return Ok(Some(agent_ide_core::lang::environment::CommandEnv {
            argv_prefix: vec![tools.node.clone().into_os_string(), cli.into_os_string()],
            // The configured Node's directory leads PATH so child processes spawned via
            // bare `node` resolve the same runtime — the module slot's SlotDir parity.
            path_prefix: tools.node.parent().map(Path::to_path_buf),
            ..Default::default()
        }));
    }
    // program == "node"
    Ok(Some(agent_ide_core::lang::environment::CommandEnv {
        argv_prefix: vec![tools.node.clone().into_os_string()],
        // Same SlotDir parity: child `node`/`npx` resolve from the configured Node's directory.
        path_prefix: tools.node.parent().map(Path::to_path_buf),
        ..Default::default()
    }))
}

/// Activation for the full argument vector under this request's configured tools. The
/// slot-resolution call (argv is exactly one slot name) answers the Launcher `node` slot with
/// the configured Node alone — one prepared executable, no interpreter prefix, variables or
/// `PATH` that a program slot cannot carry. A full argv binds `tsc`/`node` and prepared
/// project-local Vitest/Jest bins to the configured Node. Unbound `npx` runners refuse;
/// The private `runner_node` slot resolves only the executable after preparing its mandatory
/// recipe guard; raw `node`/`tsc` keep their stricter pre-admission pin checks. A stdin
/// probe whose only obstruction is a range/alias pin runs behind the runner prelude
/// ([`probe_guard`]); any other probe obstruction refuses, whichever Node it names.
pub fn command_argv(
    worktree: &Path,
    argv: &[String],
    config: &ModuleConfig,
) -> Result<Option<agent_ide_core::lang::environment::CommandEnv>, LangError> {
    if argv == ["runner_node"] {
        let tools = configured_tools(config)
            .ok_or_else(|| LangError::Environment("no configured runner Node".into()))?;
        runner_guard(worktree, &tools, config)?;
        return Ok(Some(agent_ide_core::lang::environment::CommandEnv {
            argv_prefix: vec![tools.node.into_os_string()],
            ..Default::default()
        }));
    }
    if let [name] = argv
        && name == "node"
    {
        let Some(tools) = configured_tools(config) else {
            return Ok(None);
        };
        admit_effects(worktree, config)?;
        return Ok(Some(agent_ide_core::lang::environment::CommandEnv {
            argv_prefix: vec![tools.node.clone().into_os_string()],
            ..Default::default()
        }));
    }
    // A stdin probe on a range/alias pin runs behind the same runner prelude as the module's
    // guarded probe recipes; otherwise it keeps the raw-run admission below.
    if let [program, flag, script, module, _] = argv
        && flag == "-e"
        && (script == crate::support::TS_PROBE || script == crate::support::TS_OUTLINE_PROBE)
        && module.starts_with('/')
        && let Some(vars) = probe_guard(worktree, config, program)?
    {
        let node = configured_tools(config)
            .expect("a prepared guard has configured tools")
            .node;
        return Ok(Some(agent_ide_core::lang::environment::CommandEnv {
            argv_prefix: vec![
                node.clone().into_os_string(),
                "--import".into(),
                RUNNER_PRELUDE.into(),
            ],
            path_prefix: node.parent().map(Path::to_path_buf),
            vars,
        }));
    }
    // Bare "tsc" and any other slot name falls through to the full-argv path below.
    // One shared runner-shape refusal for both routes: an `npx vitest`/`npx jest` test run
    // executes the project's Node runtime through an operator `npx` this request does not
    // configure, so it refuses instead of silently running an ambient Node the project's pin
    // does not cover. `command_env` enforces the same refusal for the native route.
    if let Some(cause) = runner_shape_refusal(argv) {
        return Err(LangError::Environment(cause));
    }
    let Some(program) = argv.first() else {
        return Ok(None);
    };
    if matches!(program.as_str(), "vitest" | "jest") {
        let unavailable = || {
            LangError::Environment(format!(
                "project-local {program} runner is missing, read restricted or untrusted; prepare its package and configured Node, or run tests natively; no ambient fallback"
            ))
        };
        let tools = configured_tools(config).ok_or_else(unavailable)?;
        let vars = runner_guard(worktree, &tools, config)?;
        let package = worktree.join("node_modules").join(program);
        let manifest = runner_input(worktree, &package.join("package.json"), &config.read_denies)
            .ok_or_else(unavailable)?;
        let manifest: Value =
            agent_ide_core::lang::environment::read_input(&manifest, &config.read_denies)
                .and_then(|text| serde_json::from_str(&text).ok())
                .ok_or_else(unavailable)?;
        if manifest.get("name").and_then(Value::as_str) != Some(program.as_str()) {
            return Err(unavailable());
        }
        let bin = manifest
            .get("bin")
            .and_then(|bin| {
                bin.as_str()
                    .or_else(|| bin.get(program).and_then(Value::as_str))
            })
            .ok_or_else(unavailable)?;
        let bin = Path::new(bin);
        if bin.as_os_str().is_empty()
            || bin.is_absolute()
            || bin.components().any(|part| {
                !matches!(
                    part,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            })
            || !matches!(
                bin.extension().and_then(|ext| ext.to_str()),
                Some("js" | "mjs" | "cjs")
            )
        {
            return Err(unavailable());
        }
        let cli = runner_input(worktree, &package.join(bin), &config.read_denies)
            .ok_or_else(unavailable)?;
        return Ok(Some(agent_ide_core::lang::environment::CommandEnv {
            argv_prefix: vec![
                tools.node.clone().into_os_string(),
                "--import".into(),
                RUNNER_PRELUDE.into(),
                cli.into_os_string(),
            ],
            path_prefix: tools.node.parent().map(Path::to_path_buf),
            vars,
        }));
    }
    command_env(worktree, program, config)
}

/// Snapshots Node constraints under the read policy and prepares the fixed runtime proof.
/// Ranges use the qualified compiler's VersionRange on the actual Node before the runner loads.
fn runner_guard(
    worktree: &Path,
    tools: &ConfiguredTools,
    config: &ModuleConfig,
) -> Result<Vec<(std::ffi::OsString, std::ffi::OsString)>, LangError> {
    let unavailable = || {
        LangError::Environment("runner Node requirements or qualified TypeScript verifier are missing, malformed or read restricted; use native tooling".into())
    };
    if let Some((_, cause)) = obstruction(worktree, tools, &config.read_denies) {
        return Err(LangError::Environment(cause));
    }
    verify_configured_node(&tools.node, config, &config.read_denies)
        .map_err(|(_, cause)| LangError::Environment(cause))?;
    let mut requirements = Vec::new();
    let manifest = worktree.join("package.json");
    if manifest.symlink_metadata().is_ok() {
        let path =
            runner_input(worktree, &manifest, &config.read_denies).ok_or_else(unavailable)?;
        let package: Value =
            agent_ide_core::lang::environment::read_input(&path, &config.read_denies)
                .and_then(|text| serde_json::from_str(&text).ok())
                .ok_or_else(unavailable)?;
        if let Some(pin) = package
            .get("engines")
            .and_then(|engines| engines.get("node"))
        {
            requirements.push(pin.as_str().ok_or_else(unavailable)?.to_owned());
        }
    }
    for name in [".nvmrc", ".node-version"] {
        let path = worktree.join(name);
        match path.symlink_metadata() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(unavailable()),
            Ok(_) => {}
        }
        let path = runner_input(worktree, &path, &config.read_denies).ok_or_else(unavailable)?;
        let pin = agent_ide_core::lang::environment::read_input(&path, &config.read_denies)
            .ok_or_else(unavailable)?;
        requirements.push(pin.trim().trim_start_matches('v').to_owned());
        break;
    }
    let mut vars = vec![("AGENT_IDE_RUNNER_REQUIREMENTS".into(), "[]".into())];
    if requirements.iter().all(|pin| plain_version(pin).is_some()) {
        admit_effects(worktree, config)?;
        if !requirements.is_empty() {
            let admitted = tools
                .node_identity
                .as_deref()
                .and_then(measured_version)
                .ok_or_else(unavailable)?;
            if requirements.iter().any(|pin| {
                let pin = plain_version(pin).expect("plain requirements checked above");
                !admitted.starts_with(&pin)
            }) {
                return Err(unavailable());
            }
        }
        return Ok(vars);
    }
    let identity = tools
        .node_identity
        .as_deref()
        .and_then(measured_version)
        .filter(|version| version.len() == 3)
        .ok_or_else(unavailable)?;
    let cli = tools.tsc_cli.as_ref().ok_or_else(unavailable)?;
    let cli = match compiler_fact(worktree, cli, &config.read_denies)
        .map_err(|(_, cause)| LangError::Environment(cause))?
    {
        CompilerFact::Operator(cli) | CompilerFact::Project { cli, .. } => cli,
    };
    let library = cli.with_file_name("typescript.js");
    crate::checks::qualify_library(&library, &config.read_denies)
        .map_err(|(_, cause)| LangError::Environment(cause))?;
    vars[0].1 = serde_json::to_string(&requirements)
        .expect("Node requirements are strings")
        .into();
    vars.push((
        "AGENT_IDE_RUNNER_TYPESCRIPT".into(),
        library.into_os_string(),
    ));
    vars.push((
        "AGENT_IDE_RUNNER_NODE_IDENTITY".into(),
        identity
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(".")
            .into(),
    ));
    Ok(vars)
}

/// Resolves a prepared runner file within the worktree, allowing contained pnpm aliases.
/// Denies apply to both spellings and their ancestors before any content is read.
fn runner_input(worktree: &Path, path: &Path, denies: &[ReadDeny]) -> Option<PathBuf> {
    let denied = |path: &Path| {
        path.ancestors()
            .any(|prefix| denies.iter().any(|deny| deny.matches(prefix)))
    };
    if denied(path) {
        return None;
    }
    let resolved = path.canonicalize().ok()?;
    (resolved.starts_with(worktree.canonicalize().ok()?)
        && !denied(&resolved)
        && agent_ide_core::lang::environment::readable(&resolved, denies))
    .then_some(resolved)
}

/// Host-owned recipe roots from the same accepted runner activation, even with a relocated
/// IDE home. Refused environments grant nothing; module-supplied paths never establish roots.
pub fn accepted_runner_roots(worktree: &Path, config: &ModuleConfig) -> Vec<PathBuf> {
    let Some(tools) = configured_tools(config) else {
        return Vec::new();
    };
    let Ok(vars) = runner_guard(worktree, &tools, config) else {
        return Vec::new();
    };
    let mut roots: Vec<_> = tools
        .node
        .parent()
        .map(Path::to_path_buf)
        .into_iter()
        .collect();
    for (name, value) in vars {
        if name == "AGENT_IDE_RUNNER_TYPESCRIPT" {
            roots.push(PathBuf::from(value));
        }
    }
    roots
}

/// The exact runner-shape refusal shared by the native argv route and the module's effect
/// planner: an `npx vitest`/`npx jest` test run executes the project's Node runtime through an
/// operator `npx` this request does not configure. `None` for every other shape — `npx
/// prettier` stays the operator's accepted formatting tool, and unclaimed programs stay
/// unclaimed.
pub fn runner_shape_refusal(argv: &[String]) -> Option<String> {
    (argv.first().map(String::as_str) == Some("npx")
        && matches!(argv.get(1).map(String::as_str), Some("vitest" | "jest")))
    .then(|| {
        "npx-based vitest/jest runs are not bound to the configured Node; prepare the \
         package manager's tool or run them natively"
            .to_owned()
    })
}

/// Remeasures only accepted provider facts bound to the actual `node` a consumer will run.
/// A different/unbound operator Node keeps its existing checker qualification. Matching facts
/// must be well formed and current; denied reads yield ReadRestricted, missing/changed bytes
/// yield EnvMissing with native recovery. This bytes-only guard leaves range validation to
/// the confined adapter and semantic provider instead of imposing raw-command pin rules.
pub(crate) fn verify_configured_node(
    node: &Path,
    config: &ModuleConfig,
    denies: &[ReadDeny],
) -> Result<(), (agent_ide_core::checks::UnavailableReason, String)> {
    use agent_ide_core::{assistance::launcher::AcceptedExecutable, checks::UnavailableReason};
    let Some(fact) = config
        .provider
        .as_ref()
        .and_then(|provider| provider.get("node"))
    else {
        return Ok(());
    };
    if fact.get("path").and_then(Value::as_str).map(Path::new) != Some(node) {
        return Ok(());
    }
    if node
        .ancestors()
        .any(|path| denies.iter().any(|deny| deny.matches(path)))
    {
        return Err((
            UnavailableReason::ReadRestricted,
            "the configured Node is read restricted; use native tooling".into(),
        ));
    }
    let accepted = serde_json::from_value::<AcceptedExecutable>(fact.clone()).ok();
    if accepted.as_ref().is_some_and(|accepted| {
        accepted.validate().is_ok()
            && agent_ide_core::lang::environment::hash_input(node, denies)
                .is_some_and(|hash| hash.to_hex().eq_ignore_ascii_case(&accepted.blake3))
    }) {
        return Ok(());
    }
    Err((UnavailableReason::EnvMissing, "the configured Node's bytes changed since acceptance, are missing or unqualified; restore the accepted Node or re-provision the launcher, or use native tooling".into()))
}

/// The environments of `worktree` under this request's configured tools and read excludes:
/// one environment for a TypeScript project, none otherwise (`detect` remains the listing
/// authority).
pub fn environments(worktree: &Path, config: &ModuleConfig) -> Vec<ResolvedEnv> {
    if !is_typescript_project(worktree) {
        return Vec::new();
    }
    let tools = configured_tools(config);
    vec![environment(
        worktree,
        tools.as_ref(),
        config,
        &config.read_denies,
    )]
}

/// The opaque identity of the worktree's environment resolution: the accepted launcher
/// sections themselves (provider and checks — their accepted digests travel with them, so a
/// same-path re-provisioned operator tool moves the identity), the root manifest, the root
/// TypeScript/JavaScript configs, every lockfile of the manager order, the installed
/// compiler's package manifest and library bytes, and the Node pin files.
fn identity_string(
    worktree: &Path,
    tools: Option<&ConfiguredTools>,
    config: &ModuleConfig,
    denies: &[ReadDeny],
) -> String {
    let hash = |path: &Path| {
        agent_ide_core::lang::environment::hash_input(path, denies).map(|hash| hash.to_string())
    };
    let mut parts: Vec<String> = Vec::new();
    // The accepted launcher sections themselves: a same-path re-provisioned operator tool
    // arrives as a changed declaration and moves the identity here, beside the on-disk
    // stamps below.
    parts.push(format!(
        "provider={}",
        config
            .provider
            .as_ref()
            .map_or_else(String::new, |value| value.to_string())
    ));
    parts.push(format!(
        "checks={}",
        config
            .checks
            .as_ref()
            .map_or_else(String::new, |value| value.to_string())
    ));
    match tools {
        Some(tools) => {
            parts.push(format!("node={}", tools.node.display()));
            parts.push(format!(
                "tsc={}",
                tools
                    .tsc_cli
                    .as_deref()
                    .map_or_else(String::new, |path| path.display().to_string())
            ));
            parts.push(format!(
                "node-identity={}",
                tools.node_identity.as_deref().unwrap_or_default()
            ));
            parts.push(format!(
                "node-digest={}",
                tools.node_digest.as_deref().unwrap_or_default()
            ));
            // Current measured content, not only the declaration: a same-path replacement
            // moves the identity even before any consumer refuses on it.
            parts.push(format!(
                "node-content={}",
                agent_ide_core::lang::environment::hash_input(&tools.node, denies)
                    .map_or_else(String::new, |hash| hash.to_string())
            ));
            // On-disk identity of the selected tools: a same-path mutated executable or
            // compiler module moves the stamp and the measured content hash even when the
            // declaration is unchanged — the guard refuses on the digest comparison, and the
            // identity moves with the measured bytes.
            parts.push(format!("node-stamp={}", stamp(&tools.node)));
            parts.push(format!(
                "tsc-stamp={}",
                tools.tsc_cli.as_deref().map(stamp).unwrap_or_default()
            ));
            // Current measured content of the selected TypeScript CLI: same-path replacement
            // moves the identity even when the declaration is unchanged.
            parts.push(format!(
                "tsc-content={}",
                tools
                    .tsc_cli
                    .as_deref()
                    .and_then(|path| agent_ide_core::lang::environment::hash_input(path, denies))
                    .map_or_else(String::new, |hash| hash.to_string())
            ));
        }
        None => parts.push("configured=-".to_owned()),
    }
    let mut configs: Vec<String> = std::fs::read_dir(worktree)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            name != "tsconfig.json"
                && ((name.starts_with("tsconfig.") && name.ends_with(".json"))
                    || name == "jsconfig.json")
        })
        .collect();
    configs.sort();
    for name in ["package.json", "tsconfig.json"] {
        parts.push(format!(
            "{name}={}",
            hash(&worktree.join(name)).unwrap_or_default()
        ));
    }
    for name in &configs {
        parts.push(format!(
            "{name}={}",
            hash(&worktree.join(name)).unwrap_or_default()
        ));
    }
    for lock in [
        "pnpm-lock.yaml",
        "yarn.lock",
        "bun.lockb",
        "bun.lock",
        "package-lock.json",
    ] {
        parts.push(format!(
            "{lock}={}",
            hash(&worktree.join(lock)).unwrap_or_default()
        ));
    }
    for relative in [
        "node_modules/typescript/package.json",
        "node_modules/typescript/lib/tsc.js",
        "node_modules/typescript/lib/typescript.js",
        ".nvmrc",
        ".node-version",
    ] {
        parts.push(format!(
            "{relative}={}",
            hash(&worktree.join(relative)).unwrap_or_default()
        ));
    }
    blake3::hash(parts.join("\n").as_bytes()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ModuleConfig whose `checks` section configures the given tools and whose accepted
    /// provider declares the measured Node identity and digest for that same executable.
    fn config_with(node: &Path, tsc_cli: &Path) -> ModuleConfig {
        ModuleConfig {
            worktree: None,
            test_workers: None,
            // Shape projection of the accepted declaration: the provider OPTION `node` is
            // what the binding reads, and its digest is the ACTUAL hash of the fixture file
            // (a fake constant would make the bytes guard accept anything). This is a
            // unit-fixture shape, not a validated declaration — the real one is the root-run
            // qualified fixture.
            provider: Some(serde_json::json!({
                "node": {
                    "path": node.display().to_string(),
                    "identity": "v22.11.0",
                    "blake3": blake3::hash(std::fs::read(node).unwrap().as_slice()).to_hex().to_string(),
                },
                "toolchain": "v22.11.0",
            })),
            checks: Some(serde_json::json!({
                "node": node.display().to_string(),
                "tsc_cli": tsc_cli.display().to_string(),
            })),
            read_denies: Vec::new(),
            env: Default::default(),
            home: None,
        }
    }

    /// Fresh scratch directory for one fixture.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agent-ide-ts-env-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes `text` to `root/relative`, creating parents.
    fn put(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// Creates Node/CLI stub files below fixture `root` and returns their paths. These are
    /// selection-policy fixtures with real, distinct content — the bytes guard measures them,
    /// but they are not executable qualification or admission evidence.
    fn tools(root: &Path) -> (PathBuf, PathBuf) {
        let node = root.join("tools/node");
        let tsc = root.join("tools/tsc.js");
        put(root, "tools/node", "node-stub-v1\n");
        put(root, "tools/tsc.js", "tsc-stub-v1\n");
        (node, tsc)
    }

    /// A non-TypeScript worktree has no environment, and a manifest-less tree of `.ts` files
    /// is still a project the environment view answers for.
    #[test]
    fn environment_exists_only_for_typescript_projects() {
        let root = scratch("not-ts");
        put(&root, "main.rs", "fn main() {}\n");
        let config = {
            let (node, tsc) = tools(&root);
            config_with(&node, &tsc)
        };
        assert!(environments(&root, &config).is_empty());
        std::fs::remove_file(root.join("main.rs")).unwrap();
        put(&root, "app.ts", "export {};\n");
        assert_eq!(environments(&root, &config).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Without configured tools nothing resolves and the disclosure says the seam's scope;
    /// the checker's own facts govern checks.
    #[test]
    fn unconfigured_tools_disclose_scope() {
        let root = scratch("unconfigured");
        put(&root, "package.json", "{\"dependencies\":{}}");
        let config = ModuleConfig::default();
        let env = environment(&root, None, &config, &[]);
        assert_eq!(env.chosen, None);
        assert_eq!(env.source, None);
        let step = env
            .missing_next_step
            .expect("the disclosure names the scope");
        assert!(step.contains("no configured TypeScript tools"), "{step}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// No manifest declaring TypeScript: the operator's accepted compiler is the winner, from
    /// the launcher section, never a global fallback; `tsc`/`node` activate the configured
    /// tools.
    #[test]
    fn operator_compiler_wins_without_a_typescript_declaration() {
        let root = scratch("operator");
        let (node, tsc) = tools(&root);
        put(
            &root,
            "package.json",
            "{\"dependencies\":{\"left-pad\":\"1\"}}",
        );
        put(&root, "tsconfig.json", "{}");
        let config = config_with(&node, &tsc);
        let env = environment(&root, configured_tools(&config).as_ref(), &config, &[]);
        let chosen = env.chosen.expect("the operator compiler resolves");
        assert_eq!(chosen.label, "operator tsc");
        assert_eq!(chosen.path, tsc);
        // The operator compiler's version is honestly unknown: it is accepted, not
        // byte-qualified against the closure.
        assert_eq!(chosen.version, None);
        assert_eq!(env.source, Some(EnvSource::Launcher));
        assert_eq!(env.missing_next_step, None);
        // `tsc` activates the configured Node with the operator compiler; `node` activates
        // the configured Node; other programs stay unclaimed.
        let tsc_env = command_env(&root, "tsc", &config).unwrap().unwrap();
        assert_eq!(
            tsc_env.argv_prefix,
            [node.clone().into_os_string(), tsc.clone().into_os_string()]
        );
        let node_env = command_env(&root, "node", &config).unwrap().unwrap();
        assert_eq!(node_env.argv_prefix, [node.into_os_string()]);
        assert_eq!(command_env(&root, "npm", &config).unwrap(), None);
        // An explicitly pathed compiler stays the caller's own tool: no claim, never a bare
        // Node substitution that would drop the compiler argument.
        assert_eq!(
            command_env(&root, "/usr/local/bin/tsc", &config).unwrap(),
            None
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A declared-but-unprepared project compiler resolves to nothing with the checker's
    /// exact cause; a wrong-byte compiler discloses the unqualified cause — never a global
    /// fallback — and claimed commands refuse with the same cause before admission.
    #[test]
    fn declared_compiler_missing_or_unqualified_resolves_nothing() {
        let root = scratch("declared");
        let (node, tsc) = tools(&root);
        put(
            &root,
            "package.json",
            "{\"devDependencies\":{\"typescript\":\"5.9.3\"}}",
        );
        let config = config_with(&node, &tsc);
        let tools = configured_tools(&config).unwrap();
        let env = environment(&root, Some(&tools), &config, &[]);
        assert_eq!(env.chosen, None);
        let step = env.missing_next_step.expect("the cause is the way out");
        assert!(step.contains("not prepared; no global fallback"), "{step}");
        let refusal = command_env(&root, "tsc", &config).unwrap_err();
        assert!(refusal.to_string().contains("not prepared"), "{refusal}");

        // Prepared but not the qualified bytes: the checker's unqualified cause, byte-for-byte.
        put(
            &root,
            "node_modules/typescript/lib/tsc.js",
            "module.exports = {};",
        );
        put(&root, "node_modules/typescript/lib/typescript.js", "short");
        let env = environment(&root, Some(&tools), &config, &[]);
        assert_eq!(env.chosen, None);
        let step = env.missing_next_step.expect("the cause is the way out");
        assert!(
            step.contains("not the qualified TypeScript 5.9.3"),
            "{step}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A Node pin that plain versions prove unsatisfied refuses claimed commands with its
    /// cause and yields no apparently usable winner; a prefix pin of the admitted version is
    /// proven; a range is disclosed as unverified, never refused.
    #[test]
    fn node_pin_semantics() {
        let root = scratch("node-pin");
        let (node, tsc) = tools(&root);
        put(&root, "package.json", "{\"dependencies\":{}}");
        put(&root, "tsconfig.json", "{}");
        let config = config_with(&node, &tsc);
        let tools = configured_tools(&config).unwrap();

        // Plain mismatch: refuses, and the card yields nothing.
        put(&root, ".nvmrc", "20.18.0\n");
        let env = environment(&root, Some(&tools), &config, &[]);
        assert_eq!(env.chosen, None, "incompatible pin is no winner");
        let step = env.missing_next_step.expect("the mismatch is the way out");
        assert!(step.contains("node pin 20.18.0"), "{step}");
        assert!(command_env(&root, "node", &config).is_err());

        // Partial-major pin satisfied by the admitted version: proven, no warning.
        put(&root, ".nvmrc", "22\n");
        let env = environment(&root, Some(&tools), &config, &[]);
        assert!(env.chosen.is_some());
        assert!(env.warnings.is_empty(), "{:?}", env.warnings);
        assert!(command_env(&root, "node", &config).is_ok());

        // Range expression: the card keeps the winner with a disclosure, but raw claimed
        // commands refuse — a range no accepted version proves (root's 24 vs >=999
        // counterexample) must never silently run an unmatched runtime.
        put(&root, ".nvmrc", ">=20\n");
        let env = environment(&root, Some(&tools), &config, &[]);
        assert!(env.chosen.is_some());
        let warning = env.warnings.first().expect("the range is disclosed");
        assert!(warning.contains("cannot verify"), "{warning}");
        let refusal = command_env(&root, "node", &config).unwrap_err();
        assert!(refusal.to_string().contains("range/alias"), "{refusal}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The identity moves with each input independently (immediate baseline per mutation):
    /// lockfile, manifest, Node pin files, and the accepted launcher sections themselves — a
    /// re-provisioned operator tool with the same path moves it.
    #[test]
    fn identity_covers_each_input() {
        let root = scratch("identity");
        let (node, tsc) = tools(&root);
        put(&root, "package.json", "{\"dependencies\":{}}");
        put(&root, "tsconfig.json", "{}");
        put(&root, "pnpm-lock.yaml", "lock v1\n");
        put(&root, ".nvmrc", "22.11.0\n");
        let config = config_with(&node, &tsc);
        let id = |config: &ModuleConfig| {
            environment(&root, configured_tools(config).as_ref(), config, &[]).identity
        };
        let before = id(&config);
        assert_eq!(id(&config), before);
        put(&root, "pnpm-lock.yaml", "lock v2\n");
        assert_ne!(id(&config), before, "lockfile");
        let after_lock = id(&config);
        put(&root, "package.json", "{\"dependencies\":{},\"x\":1}");
        assert_ne!(id(&config), after_lock, "manifest");
        let after_manifest = id(&config);
        put(&root, ".nvmrc", "22.11.1\n");
        assert_ne!(id(&config), after_manifest, "pin file");
        let after_pin = id(&config);
        // The same path with different accepted bytes moves the identity through the sections.
        let mut reprovisioned = config_with(&node, &tsc);
        reprovisioned.provider = Some(serde_json::json!({
            "node": {
                "path": node.display().to_string(),
                "identity": "v22.11.1",
                "blake3": "cc5551dd9b4b41b1a6977e7f9f22a5721e1ea6c94a92d7f7eea7fea3b3d7e1c9",
            },
            "toolchain": "v22.11.1",
        }));
        assert_ne!(
            id(&reprovisioned),
            after_pin,
            "re-provisioned operator tool"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A host-denied root manifest answers no project from `configured_detect` — no
    /// unguarded detector fallthrough — while unrestricted detection lists the same tree.
    #[test]
    fn configured_detect_refuses_denied_manifests() {
        use crate::support::TypeScript;
        use agent_ide_core::lang::LanguageSupport;
        let root = scratch("detect-denied");
        let (node, tsc) = tools(&root);
        put(&root, "package.json", "{\"dependencies\":{}}");
        put(&root, "tsconfig.json", "{}");
        let mut config = config_with(&node, &tsc);
        assert!(TypeScript.configured_detect(&root, &config).is_some());
        // Absent optional pin files never block.
        assert!(!root.join(".nvmrc").exists());
        assert!(TypeScript.configured_detect(&root, &config).is_some());
        // A denied manifest blocks.
        config.read_denies = vec![agent_ide_core::execution::seatbelt::ReadDeny::Path(
            root.join("package.json"),
        )];
        assert_eq!(TypeScript.configured_detect(&root, &config), None);
        config.read_denies.clear();
        // A linked manifest blocks without following: the checker's link-free policy.
        std::fs::rename(root.join("package.json"), root.join("real-package.json")).unwrap();
        std::os::unix::fs::symlink(root.join("real-package.json"), root.join("package.json"))
            .unwrap();
        assert!(
            root.join("package.json")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(TypeScript.configured_detect(&root, &config), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A same-path replacement of the configured Node (same name, unchanged accepted
    /// declaration, same file length) refuses claimed commands and effect planning: the
    /// shared guard re-measures the actual bytes against the accepted digest. The card keeps
    /// the winner visible; the refusal is the truthful action state.
    #[test]
    fn same_path_node_replacement_refuses() {
        let root = scratch("node-replaced");
        let (node, tsc) = tools(&root);
        put(&root, "package.json", "{\"dependencies\":{}}");
        put(&root, "tsconfig.json", "{}");
        let config = config_with(&node, &tsc);
        let tools = configured_tools(&config).unwrap();
        // The baseline identity is captured with the SAME config object before any mutation.
        let before = environment(&root, Some(&tools), &config, &[]).identity;
        // Same path, same length, different bytes, declaration unchanged.
        let original = std::fs::read(&node).unwrap();
        let mut replaced = original.clone();
        replaced[0] ^= b'x';
        std::fs::write(&node, &replaced).unwrap();
        assert_ne!(std::fs::read(&node).unwrap(), original);
        let cause = admit_effects(&root, &config).unwrap_err();
        assert!(
            cause.to_string().contains("bytes changed since acceptance"),
            "{cause}"
        );
        assert!(command_env(&root, "node", &config).is_err());
        assert!(command_env(&root, "tsc", &config).is_err());
        // Current measured content participates in the identity. Restoring accepted bytes
        // restores usability, while the file's changed object stamp still invalidates reuse.
        let replaced_identity = environment(&root, Some(&tools), &config, &[]).identity;
        assert_ne!(
            replaced_identity, before,
            "replaced bytes move the identity"
        );
        std::fs::write(&node, &original).unwrap();
        assert!(admit_effects(&root, &config).is_ok());
        assert_ne!(
            environment(&root, Some(&tools), &config, &[]).identity,
            replaced_identity
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The identity's Node facts are bound to the actually selected checker Node: a provider
    /// declaration for a DIFFERENT executable contributes no identity or version to this
    /// request's tools.
    #[test]
    fn provider_facts_bind_to_the_selected_node() {
        let root = scratch("binding");
        let (node, tsc) = tools(&root);
        put(&root, "tools/other-node", "");
        let other = root.join("tools/other-node");
        put(&root, "package.json", "{\"dependencies\":{}}");
        put(&root, "tsconfig.json", "{}");
        let mut config = config_with(&node, &tsc);
        config.provider = Some(serde_json::json!({
            "node": {
                "path": other.display().to_string(),
                "identity": "v18.0.0",
                "blake3": "bb5551dd9b4b41b1a6977e7f9f22a5721e1ea6c94a92d7f7eea7fea3b3d7e1c9",
            },
            "toolchain": "v18.0.0",
        }));
        let tools = configured_tools(&config).unwrap();
        assert_eq!(tools.node_identity, None, "foreign provider facts unbound");
        assert_eq!(tools.node_digest, None);
        // A .nvmrc pin with no bound measured identity is disclosed on the card as
        // unverifiable, and refused for raw claimed commands and effect planning: facts this
        // request does not have never become compatibility proof.
        put(&root, ".nvmrc", "99.0.0\n");
        let env = environment(&root, Some(&tools), &config, &[]);
        assert!(env.chosen.is_some(), "unverified is disclosed, not refused");
        let warning = env
            .warnings
            .first()
            .expect("the unverified pin is disclosed");
        assert!(warning.contains("cannot be verified"), "{warning}");
        // Raw claimed commands and effect planning refuse the unverifiable pin: a warning is
        // not compatibility proof for a run that never reaches the adapter's validation.
        assert!(command_env(&root, "node", &config).is_err());
        assert!(command_env(&root, "tsc", &config).is_err());
        assert!(admit_effects(&root, &config).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Root's counterexample: a bound configured Node (22.x measured) with a `>=999` range
    /// pin must refuse raw claimed commands — the range can never be satisfied by the
    /// admitted major, and a warning alone would let an unmatched runtime run.
    #[test]
    fn unsatisfiable_range_pin_refuses_raw_commands() {
        let root = scratch("range-999");
        let (node, tsc) = tools(&root);
        put(&root, "package.json", "{\"dependencies\":{}}");
        put(&root, "tsconfig.json", "{}");
        put(&root, ".nvmrc", ">=999\n");
        let config = config_with(&node, &tsc);
        let refusal = command_env(&root, "node", &config).unwrap_err();
        assert!(refusal.to_string().contains("range/alias"), "{refusal}");
        assert!(refusal.to_string().contains(">=999"), "{refusal}");
        assert!(admit_effects(&root, &config).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The native argv route and the module effect planner enforce the SAME runner-shape
    /// refusal: `npx vitest`/`npx jest` refuse in both, while the operator formatting tool
    /// and unclaimed programs stay unclaimed — no ambient fallback either way.
    #[test]
    fn runner_shape_refusal_is_shared_by_both_routes() {
        let root = scratch("runner-shapes");
        let (node, tsc) = tools(&root);
        put(&root, "package.json", "{\"dependencies\":{}}");
        put(&root, "tsconfig.json", "{}");
        let config = config_with(&node, &tsc);
        for runner_args in [
            vec!["npx".to_owned(), "vitest".to_owned(), "run".to_owned()],
            vec!["npx".to_owned(), "jest".to_owned(), "a.test.ts".to_owned()],
        ] {
            assert!(
                command_argv(&root, &runner_args, &config).is_err(),
                "native route refuses: {runner_args:?}"
            );
            assert!(
                crate::module::configured_effect(&runner_args, &config, &root).is_err(),
                "module planner refuses: {runner_args:?}"
            );
        }
        // An unsupported raw Node pin cannot veto unrelated commands or the separately
        // selected operator formatter in either route.
        put(&root, ".nvmrc", ">=999\n");
        let prettier = vec![
            "npx".into(),
            "prettier".into(),
            "--stdin-filepath".into(),
            "a.ts".into(),
        ];
        assert!(
            crate::module::configured_effect(&prettier, &config, &root)
                .unwrap()
                .is_some()
        );
        assert!(
            crate::module::configured_effect(&["npm".into(), "test".into()], &config, &root)
                .unwrap()
                .is_none()
        );
        // The operator formatting tool and unclaimed programs stay unclaimed in the native
        // route (the module planner formats through the accepted shape).
        assert_eq!(
            command_argv(
                &root,
                &[
                    "npx".to_owned(),
                    "prettier".to_owned(),
                    "--stdin-filepath".to_owned(),
                    "a.ts".to_owned()
                ],
                &config
            )
            .unwrap(),
            None
        );
        assert_eq!(
            command_argv(
                &root,
                &["npm".to_owned(), "run".to_owned(), "test".to_owned()],
                &config
            )
            .unwrap(),
            None
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A detected runner's prepared package bin binds to the configured Node in both routes.
    #[test]
    fn prepared_test_runner_binds_both_routes() {
        use agent_ide_core::lang::{LanguageSupport, TestTarget};
        let root = scratch("prepared-runner");
        let (node, tsc) = tools(&root);
        let config = config_with(&node, &tsc);
        for runner in ["vitest", "jest"] {
            put(
                &root,
                "package.json",
                &format!(r#"{{"devDependencies":{{"{runner}":"1"}}}}"#),
            );
            put(
                &root,
                &format!("node_modules/{runner}/package.json"),
                &format!(r#"{{"name":"{runner}","bin":{{"{runner}":"./cli.cjs"}}}}"#),
            );
            put(
                &root,
                &format!("node_modules/{runner}/cli.cjs"),
                "console.log('prepared');",
            );
            let project = crate::support::TypeScript.detect(&root).unwrap();
            let selection = crate::support::TypeScript
                .test_selection(&project, &TestTarget::Pattern("-exact pattern 🦀".into()))
                .unwrap();
            let activation = command_argv(&root, &selection.command, &config)
                .unwrap()
                .expect("prepared runner activation");
            assert_eq!(
                activation.argv_prefix,
                [
                    node.clone().into_os_string(),
                    "--import".into(),
                    RUNNER_PRELUDE.into(),
                    root.join(format!("node_modules/{runner}/cli.cjs"))
                        .into_os_string()
                ]
            );
            assert_eq!(activation.path_prefix, node.parent().map(Path::to_path_buf));
            let effect = crate::module::configured_effect(&selection.command, &config, &root)
                .unwrap()
                .expect("prepared runner recipe");
            assert_eq!(effect.recipe, format!("test-{runner}-named"));
            let programs = [("runner_node".into(), node.clone())];
            let admission = agent_ide_core::modules::recipe::Admission {
                worktree: &root,
                cache_dir: &root,
                read_denies: &[],
                home: Some(&root),
                launcher_roots: &[],
                developer_dirs: &[],
                programs: &programs,
                timeout: std::time::Duration::from_secs(30),
            };
            let spec = agent_ide_core::modules::recipe::expand(
                crate::module::RECIPES,
                &effect,
                &admission,
            )
            .unwrap();
            assert_eq!(spec.program, node);
            let expected: Vec<std::ffi::OsString> = activation.argv_prefix[1..]
                .iter()
                .cloned()
                .chain(selection.command[1..].iter().map(Into::into))
                .collect();
            assert_eq!(spec.args, expected);
            let mut escaping = effect;
            escaping.params.insert(
                "runner".into(),
                agent_ide_core::modules::payload::Param::Path(
                    node.parent()
                        .unwrap()
                        .parent()
                        .unwrap()
                        .with_file_name("outside")
                        .join("cli.mjs"),
                ),
            );
            assert!(
                agent_ide_core::modules::recipe::expand(
                    crate::module::RECIPES,
                    &escaping,
                    &admission
                )
                .is_err()
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Invalid package metadata, denied inputs and escaping aliases refuse in both routes.
    #[test]
    fn prepared_test_runner_rejects_untrusted_or_unreadable_inputs() {
        let root = scratch("runner-refusals");
        let (node, tsc) = tools(&root);
        let baseline = config_with(&node, &tsc);
        let argv = vec!["vitest".into(), "run".into(), "a.test.ts".into()];
        let package = "node_modules/vitest/package.json";
        let cli = "node_modules/vitest/cli.mjs";
        let valid = r#"{"name":"vitest","bin":"./cli.mjs"}"#;
        put(&root, "package.json", "{}");
        for case in [
            "missing",
            "malformed",
            "wrong-name",
            "wrong-bin",
            "escape-bin",
            "shell-bin",
            "missing-cli",
            "denied-manifest",
            "denied-cli",
            "denied-parent",
            "unconfigured",
            "changed-node",
            "escaping-link",
        ] {
            put(&root, package, valid);
            put(&root, cli, "console.log('unused');");
            let mut config = baseline.clone();
            match case {
                "missing" => std::fs::remove_file(root.join(package)).unwrap(),
                "malformed" => put(&root, package, "{"),
                "wrong-name" => put(&root, package, r#"{"name":"other","bin":"cli.mjs"}"#),
                "wrong-bin" => put(
                    &root,
                    package,
                    r#"{"name":"vitest","bin":{"other":"cli.mjs"}}"#,
                ),
                "escape-bin" => put(&root, package, r#"{"name":"vitest","bin":"../other.mjs"}"#),
                "shell-bin" => put(&root, package, r#"{"name":"vitest","bin":"cli.sh"}"#),
                "missing-cli" => std::fs::remove_file(root.join(cli)).unwrap(),
                "denied-manifest" | "denied-cli" | "denied-parent" => {
                    config.read_denies = vec![ReadDeny::Path(root.join(match case {
                        "denied-manifest" => package,
                        "denied-cli" => cli,
                        _ => "node_modules",
                    }))]
                }
                "unconfigured" => config = ModuleConfig::default(),
                "changed-node" => std::fs::write(&node, "changed-node").unwrap(),
                "escaping-link" => {
                    std::fs::remove_file(root.join(cli)).unwrap();
                    std::os::unix::fs::symlink("/usr/bin/true", root.join(cli)).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                matches!(
                    command_argv(&root, &argv, &config),
                    Err(LangError::Environment(_))
                ),
                "native {case}"
            );
            assert!(
                matches!(
                    crate::module::configured_effect(&argv, &config, &root),
                    Err(LangError::Environment(_))
                ),
                "module {case}"
            );
            std::fs::write(&node, "node-stub-v1\n").unwrap();
            if case == "escaping-link" {
                std::fs::remove_file(root.join(cli)).unwrap();
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The Launcher `node` slot resolves to the request's CONFIGURED Node — the fixture's
    /// `nodeA` — never to whatever a conflicting ambient `PATH` would name: the returned
    /// activation is exactly that one absolute executable, with no variables or `PATH`
    /// prefix a program slot cannot carry.
    #[test]
    fn node_slot_resolves_the_configured_node_alone() {
        let root = scratch("slot-node");
        let (node_a, tsc) = tools(&root);
        put(&root, "package.json", "{\"dependencies\":{}}");
        put(&root, "tsconfig.json", "{}");
        let config = config_with(&node_a, &tsc);
        let activation = command_argv(&root, &["node".to_owned()], &config)
            .unwrap()
            .expect("the configured Node slot resolves");
        assert_eq!(
            activation.argv_prefix,
            [node_a.clone().into_os_string()],
            "the configured Node, not an ambient lookup"
        );
        assert!(activation.vars.is_empty());
        assert!(activation.path_prefix.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Root's Hono counterexample: a measured 24.4.0 Node with an `engines` `>=16.9.0` pin.
    /// Both stdin probes leave the range to the runner guard, never to a raw run, in the
    /// module planner and the native argv route alike: an unqualified verifier, an unmeasured
    /// Node or another Node still refuses, a plain mismatch keeps its raw-run refusal, and an
    /// absent pin keeps the unguarded recipes and activations.
    #[test]
    fn range_pinned_probes_refuse_without_the_runner_guard() {
        let root = scratch("probe-range");
        let (node, tsc) = tools(&root);
        put(&root, "tsconfig.json", "{}");
        put(&root, "package.json", r#"{"engines":{"node":">=16.9.0"}}"#);
        let mut config = config_with(&node, &tsc);
        config.provider.as_mut().unwrap()["node"]["identity"] = "24.4.0".into();
        let module = root.join("tools/typescript.js").display().to_string();
        let argv = |program: &str, script: &str| {
            [program, "-e", script, module.as_str(), "a.test.ts"].map(str::to_owned)
        };
        let scripts = [crate::support::TS_PROBE, crate::support::TS_OUTLINE_PROBE];
        // Every probe argv on `programs` in both routes, as refusal texts.
        let refusals = |config: &ModuleConfig, programs: &[&str]| {
            let mut refusals = Vec::new();
            for program in programs {
                for script in scripts {
                    let argv = argv(program, script);
                    let module = crate::module::configured_effect(&argv, config, &root);
                    let native = command_argv(&root, &argv, config);
                    refusals.push(module.unwrap_err().to_string());
                    refusals.push(native.unwrap_err().to_string());
                }
            }
            refusals
        };
        let node_text = node.display().to_string();
        let configured = [node_text.as_str(), "node"];
        // The stub compiler is no qualified verifier: the range stays unproven.
        assert!(admit_effects(&root, &config).is_err());
        for refusal in refusals(&config, &configured) {
            assert!(refusal.contains("project TypeScript compiler"), "{refusal}");
        }
        // The guard binds only the configured Node: another one keeps the raw-run refusal.
        for refusal in refusals(&config, &["/usr/local/bin/node"]) {
            assert!(refusal.contains("range/alias"), "{refusal}");
        }
        let mut unmeasured = config.clone();
        unmeasured.provider.as_mut().unwrap()["node"]["identity"] = "node".into();
        for refusal in refusals(&unmeasured, &configured) {
            assert!(refusal.contains("runner Node requirements"), "{refusal}");
        }
        put(&root, ".nvmrc", "22.11.0\n");
        for refusal in refusals(&config, &configured) {
            assert!(refusal.contains("does not match"), "{refusal}");
        }
        std::fs::remove_file(root.join(".nvmrc")).unwrap();
        put(&root, "package.json", "{\"dependencies\":{}}");
        let recipes = scripts.map(|script| {
            crate::module::configured_effect(&argv(&node_text, script), &config, &root)
                .unwrap()
                .unwrap()
                .recipe
        });
        assert_eq!(recipes, ["ts-probe", "ts-outline"]);
        for script in scripts {
            assert_eq!(
                command_argv(&root, &argv(&node_text, script), &config).unwrap(),
                None,
                "an explicitly pathed Node stays the caller's tool"
            );
            let activation = command_argv(&root, &argv("node", script), &config)
                .unwrap()
                .expect("bare node binds the configured Node");
            assert_eq!(activation.argv_prefix, [node.clone().into_os_string()]);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The guarded probe recipes run the stdin probes behind the fixed runner prelude, with
    /// the guard's three inputs as their environment, on an explicitly pathed Node or the
    /// guarded runner slot; a request without the guard inputs does not expand.
    #[test]
    fn guarded_probe_recipes_expand_behind_the_runner_prelude() {
        use agent_ide_core::modules::payload::{EffectRequest, Param};
        let root = scratch("probe-guarded");
        let (node, _) = tools(&root);
        put(&root, "tools/typescript.js", "typescript-stub\n");
        let module = root.join("tools/typescript.js");
        let programs = [("runner_node".to_owned(), node.clone())];
        let admission = agent_ide_core::modules::recipe::Admission {
            worktree: &root,
            cache_dir: &root,
            read_denies: &[],
            home: Some(&root),
            launcher_roots: &[],
            developer_dirs: &[],
            programs: &programs,
            timeout: std::time::Duration::from_secs(10),
        };
        for (recipe, script) in [
            ("ts-probe-guarded", crate::support::TS_PROBE),
            ("ts-outline-guarded", crate::support::TS_OUTLINE_PROBE),
            ("ts-probe-tool-guarded", crate::support::TS_PROBE),
            ("ts-outline-tool-guarded", crate::support::TS_OUTLINE_PROBE),
        ] {
            let program = if recipe.contains("-tool-") {
                ("node", Param::Executable("node".into()))
            } else {
                ("node_path", Param::Path(node.clone()))
            };
            let mut effect = EffectRequest {
                recipe: recipe.into(),
                params: [
                    program,
                    ("module", Param::Path(module.clone())),
                    ("file", Param::Token("a.test.ts".into())),
                    ("verifier", Param::Path(module.clone())),
                    ("node_identity", Param::Token("24.4.0".into())),
                ]
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value))
                .collect(),
            };
            assert!(
                agent_ide_core::modules::recipe::expand(
                    crate::module::RECIPES,
                    &effect,
                    &admission
                )
                .is_err(),
                "{recipe} needs the guard requirements"
            );
            effect.params.insert(
                "requirements".into(),
                Param::Token(r#"[">=16.9.0"]"#.into()),
            );
            let spec = agent_ide_core::modules::recipe::expand(
                crate::module::RECIPES,
                &effect,
                &admission,
            )
            .unwrap_or_else(|refusal| panic!("{recipe}: {refusal:?}"));
            assert_eq!(spec.program, node, "{recipe}");
            let expected: Vec<std::ffi::OsString> = [
                "--import",
                RUNNER_PRELUDE,
                "-e",
                script,
                module.to_str().unwrap(),
                "a.test.ts",
            ]
            .map(Into::into)
            .into();
            assert_eq!(spec.args, expected, "{recipe}");
            for (name, value) in [
                ("AGENT_IDE_RUNNER_REQUIREMENTS", r#"[">=16.9.0"]"#),
                ("AGENT_IDE_RUNNER_TYPESCRIPT", module.to_str().unwrap()),
                ("AGENT_IDE_RUNNER_NODE_IDENTITY", "24.4.0"),
            ] {
                assert!(
                    spec.env
                        .iter()
                        .any(|(key, actual)| key == name && actual == value),
                    "{recipe}: {name} in {:?}",
                    spec.env
                );
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The guarded probes on the real accepted Node and qualified compiler
    /// (`AGENT_IDE_PROBE_LAUNCHER`: a launcher whose first provider declares the Node;
    /// `AGENT_IDE_PROBE_COMPILER`: a directory holding the qualified `tsc.js`/`typescript.js`).
    /// Both routes run the fixed scripts on exactly the configured Node: an `engines` range
    /// the Node satisfies yields the probe's output, while an unsatisfied range or an alias
    /// VersionRange cannot parse exits 78 with no output.
    #[test]
    #[ignore = "runs the real accepted Node and qualified TypeScript compiler"]
    fn guarded_probes_prove_the_range_on_the_actual_node() {
        use agent_ide_core::lang::LanguageSupport;
        let launcher: Value = serde_json::from_str(
            &std::fs::read_to_string(std::env::var("AGENT_IDE_PROBE_LAUNCHER").unwrap()).unwrap(),
        )
        .unwrap();
        let compiler = PathBuf::from(std::env::var("AGENT_IDE_PROBE_COMPILER").unwrap());
        let provider = launcher["targets"][0]["providers"][0].clone();
        let node = PathBuf::from(provider["node"]["path"].as_str().unwrap());
        let root = scratch("probe-actual");
        for name in ["tsc.js", "typescript.js"] {
            let target = root.join("node_modules/typescript/lib").join(name);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::copy(compiler.join(name), target).unwrap();
        }
        let library = root.join("node_modules/typescript/lib/typescript.js");
        put(&root, "tsconfig.json", "{}");
        let config = ModuleConfig {
            worktree: Some(root.clone()),
            test_workers: None,
            provider: Some(provider),
            checks: Some(serde_json::json!({
                "node": node.display().to_string(),
                "tsc_cli": root.join("node_modules/typescript/lib/tsc.js").display().to_string(),
            })),
            read_denies: Vec::new(),
            env: Default::default(),
            home: std::env::var_os("HOME").map(PathBuf::from),
        };
        let source = "describe('suite', () => {\n  it('case', () => {})\n})\n";
        let run = |program: &Path, args: &[std::ffi::OsString], env: Vec<(String, String)>| {
            use std::io::Write as _;
            let mut child = std::process::Command::new(program)
                .args(args)
                .current_dir(&root)
                .env_clear()
                .envs(env)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(source.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            (output.status.code(), output.stdout)
        };
        let home = config.home.clone().unwrap();
        let roots = vec![node.parent().unwrap().parent().unwrap().to_path_buf()];
        let admission = agent_ide_core::modules::recipe::Admission {
            worktree: &root,
            cache_dir: &root,
            read_denies: &[],
            home: Some(&home),
            launcher_roots: &roots,
            developer_dirs: &[],
            programs: &[],
            timeout: std::time::Duration::from_secs(10),
        };
        for (pin, accepted) in [(">=16.9.0", true), (">=99", false), ("lts/iron", false)] {
            put(
                &root,
                "package.json",
                &format!(
                    r#"{{"devDependencies":{{"typescript":"5.9.3"}},"engines":{{"node":"{pin}"}}}}"#
                ),
            );
            for (script, outline) in [
                (crate::support::TS_PROBE, false),
                (crate::support::TS_OUTLINE_PROBE, true),
            ] {
                let argv = [
                    node.display().to_string(),
                    "-e".into(),
                    script.into(),
                    library.display().to_string(),
                    "a.test.ts".into(),
                ];
                let effect = crate::module::configured_effect(&argv, &config, &root)
                    .unwrap()
                    .unwrap();
                let spec = agent_ide_core::modules::recipe::expand(
                    crate::module::RECIPES,
                    &effect,
                    &admission,
                )
                .unwrap();
                let activation = command_argv(&root, &argv, &config).unwrap().unwrap();
                let native: Vec<std::ffi::OsString> = activation.argv_prefix[1..]
                    .iter()
                    .cloned()
                    .chain(argv[1..].iter().map(Into::into))
                    .collect();
                // Identical guarded command on exactly the verified configured Node.
                assert_eq!(spec.program, node, "{pin}");
                assert_eq!(activation.argv_prefix[0], node.as_os_str(), "{pin}");
                assert_eq!(spec.args, native, "{pin}");
                let native_env = activation
                    .vars
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.to_string_lossy().into_owned(),
                            value.to_string_lossy().into_owned(),
                        )
                    })
                    .collect::<Vec<_>>();
                for (route, (status, stdout)) in [
                    ("module", run(&spec.program, &spec.args, spec.env.clone())),
                    ("native", run(&node, &native, native_env)),
                ] {
                    if !accepted {
                        assert_eq!((status, stdout.len()), (Some(78), 0), "{pin} {route}");
                    } else if outline {
                        assert_eq!(status, Some(0), "{pin} {route}");
                        let text = String::from_utf8(stdout).unwrap();
                        let parsed = crate::support::TypeScript.source_outline_probe_result(
                            Path::new("a.test.ts"),
                            source,
                            &text,
                        );
                        assert!(parsed.is_some(), "{pin} {route}: {text}");
                    } else {
                        assert_eq!((status, stdout.len()), (Some(0), 0), "{pin} {route}");
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
