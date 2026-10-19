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

/// The operator-accepted tools of this request, from the launcher sections the core carries,
/// with the accepted facts bound to the actually selected checker Node: the measured identity
/// and digest of the provider declaration travel only when the provider's accepted executable
/// is the same file this request's checks section configures.
pub struct ConfiguredTools {
    /// The accepted confined-check Node executable.
    pub node: PathBuf,
    /// The accepted confined-check TypeScript CLI module.
    pub tsc_cli: PathBuf,
    /// The measured Node identity, when the accepted provider declaration names the same Node
    /// executable this request's checks configure; otherwise unknown.
    pub node_identity: Option<String>,
    /// The accepted Node binary digest, bound like [`Self::node_identity`].
    pub node_digest: Option<String>,
}

/// Parses [`ConfiguredTools`] from the request's launcher sections; `None` when no
/// `project_checks` section configures this language's tools.
fn configured_tools(config: &ModuleConfig) -> Option<ConfiguredTools> {
    let checks: ProjectTypeScriptChecksConfig =
        serde_json::from_value(config.checks.clone()?).ok()?;
    let provider = config.provider.as_ref();
    // The accepted Node is the provider OPTION `node` (the common `executable` is the bridge);
    // its facts travel only when it is the same file this request's checks configure.
    let accepted_node = provider.and_then(|provider| provider.get("node"));
    let same_node = accepted_node
        .and_then(|node| node.get("path"))
        .and_then(Value::as_str)
        .is_some_and(|path| Path::new(path) == checks.node());
    let fact = |field: &str| {
        same_node
            .then(|| {
                accepted_node
                    .and_then(|node| node.get(field))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .flatten()
    };
    Some(ConfiguredTools {
        node: checks.node().to_path_buf(),
        tsc_cli: checks.tsc_cli().to_path_buf(),
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
    let Some(admitted) = tools.node_identity.as_deref().and_then(measured_version) else {
        return None;
    };
    let Some(declared) = node_pin(worktree, denies) else {
        return None;
    };
    let Some(components) = plain_version(&declared) else {
        return None;
    };
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
/// `None` when they may: the compiler fact's refusal, then the Node pin's.
fn obstruction(worktree: &Path, tools: &ConfiguredTools, denies: &[ReadDeny]) -> Option<String> {
    match compiler_fact(worktree, &tools.tsc_cli, denies) {
        Ok(_) => node_pin_conflict(worktree, tools, denies),
        Err((_, cause)) => Some(cause),
    }
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
    let fact = tools.filter(|_| cause.is_none()).map(|tools| {
        match compiler_fact(worktree, &tools.tsc_cli, denies) {
            Ok(fact) => Ok(fact),
            Err((_, cause)) => Err(cause),
        }
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
    let missing_next_step = cause.or_else(|| {
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
    match obstruction(worktree, &tools, &config.read_denies) {
        Some(cause) => Err(LangError::Environment(cause)),
        // An unverifiable authoritative pin refuses effect planning too: a recipe would run
        // raw Node the pin does not cover.
        None if node_pin(worktree, &config.read_denies).is_some()
            && tools.node_identity.is_none() =>
        {
            Err(LangError::Environment(
                "node pin cannot be verified: the configured Node has no measured identity in \
                 this request — IDE commands refuse an unverifiable pin; align the launcher \
                 Node or adjust the pin"
                    .to_owned(),
            ))
        }
        None => Ok(()),
    }
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
    // An authoritative pin this seam cannot verify (no measured Node identity) refuses raw
    // claimed commands exactly like a known mismatch: a card warning alone is not
    // compatibility proof for a run that never reaches the adapter's runtime validation.
    if node_pin(worktree, &config.read_denies).is_some() && tools.node_identity.is_none() {
        return Err(LangError::Environment(
            "node pin cannot be verified: the configured Node has no measured identity in \
             this request — IDE commands refuse an unverifiable pin; align the launcher Node \
             or adjust the pin"
                .to_owned(),
        ));
    }
    let compiler = match compiler_fact(worktree, &tools.tsc_cli, &config.read_denies) {
        Ok(fact) => fact,
        Err((_, cause)) => return Err(LangError::Environment(cause)),
    };
    let environment = match &compiler {
        CompilerFact::Project { cli, .. } | CompilerFact::Operator(cli) if program == "tsc" => {
            agent_ide_core::lang::environment::CommandEnv {
                argv_prefix: vec![
                    tools.node.clone().into_os_string(),
                    cli.clone().into_os_string(),
                ],
                ..Default::default()
            }
        }
        _ => agent_ide_core::lang::environment::CommandEnv {
            argv_prefix: vec![tools.node.clone().into_os_string()],
            ..Default::default()
        },
    };
    Ok(Some(environment))
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
            parts.push(format!("tsc={}", tools.tsc_cli.display()));
            parts.push(format!(
                "node-identity={}",
                tools.node_identity.as_deref().unwrap_or_default()
            ));
            parts.push(format!(
                "node-digest={}",
                tools.node_digest.as_deref().unwrap_or_default()
            ));
            // On-disk identity of the selected tools: a same-path mutated executable or
            // compiler module moves the stamp even when the declaration is unchanged. The
            // node's content identity is the admitted digest above; a swap preserving every
            // stamp field is outside this seam and stays with admission-time measurement.
            parts.push(format!("node-stamp={}", stamp(&tools.node)));
            parts.push(format!("tsc-stamp={}", stamp(&tools.tsc_cli)));
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
            // Shape projection of the accepted declaration: the provider OPTION `node` is
            // what the binding reads. This is a unit-fixture shape, not a validated
            // declaration — the real one is the root-run qualified fixture.
            provider: Some(serde_json::json!({
                "node": {
                    "path": node.display().to_string(),
                    "identity": "v22.11.0",
                    "blake3": "aa5551dd9b4b41b1a6977e7f9f22a5721e1ea6c94a92d7f7eea7fea3b3d7e1c9",
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

    /// Creates empty Node/CLI files below fixture `root` and returns their paths. These are
    /// selection-policy stubs, not executable qualification or admission evidence.
    fn tools(root: &Path) -> (PathBuf, PathBuf) {
        let node = root.join("tools/node");
        let tsc = root.join("tools/tsc.js");
        put(root, "tools/node", "");
        put(root, "tools/tsc.js", "");
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

        // Range expression: disclosed as unverified, never refused or interpreted.
        put(&root, ".nvmrc", ">=20\n");
        let env = environment(&root, Some(&tools), &config, &[]);
        assert!(env.chosen.is_some());
        let warning = env.warnings.first().expect("the range is disclosed");
        assert!(warning.contains("cannot verify"), "{warning}");
        assert!(command_env(&root, "node", &config).is_ok());
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
        config.read_denies = vec![agent_ide_core::execution::seatbelt::ReadDeny::Path(
            root.join("package.json"),
        )];
        assert_eq!(TypeScript.configured_detect(&root, &config), None);
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
}
