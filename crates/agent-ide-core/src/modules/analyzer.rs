//! Core-side start of a module-hosted provider: the language backend admits a provider view as
//! today, spawns the daemon's pinned executable in hidden `module <language> analyzer` mode
//! through that view's one-time provider capability, and opens a [`LiveSession`] on it.
//!
//! The module receives only the declaration's accepted executables and files ([`ProviderGrant`])
//! and the language's settings value; it plans and verifies its own provider launch.

use std::{collections::BTreeMap, ffi::OsString, io, path::PathBuf, time::Duration};

use serde_json::{Value, json};

use super::{
    contract::{
        Capability, HelloOffer, Limits, ModuleConfig, ModuleId, PROTOCOL, Role, Support, VERSION,
    },
    host::HostChannel,
    launch::{ModuleExecutable, module_env},
    provider::ProviderGrant,
};
use crate::{
    execution::{CommandKind, ControlledCommand},
    intelligence::{
        freshness::ViewGeneration,
        session::{LiveSession, ProviderSettings},
    },
    workspace::authority::WorktreeRef,
};

/// Spawn-to-`hello` ceiling of an analyzer module.
const HELLO_BUDGET: Duration = Duration::from_secs(30);

/// The provider command that starts `language`'s analyzer module in `worktree`, refused when the
/// executable changed since it was pinned. Its environment is only the module environment
/// (plus the module fault seam in test builds).
pub fn analyzer_command(
    executable: &ModuleExecutable,
    language: &str,
    worktree: &WorktreeRef,
) -> Option<ControlledCommand> {
    let mut env: BTreeMap<OsString, OsString> = module_env(language)
        .into_iter()
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect();
    if let Some(seam) = crate::test_seams::var(super::serve::FAULT_SEAM) {
        env.insert(super::serve::FAULT_SEAM.into(), seam.into());
    }
    let command = ControlledCommand::from_validated_peer(
        CommandKind::Provider,
        executable.path.clone(),
        vec![
            "module".into(),
            language.into(),
            Role::Analyzer.name().into(),
        ],
        worktree.worktree_path().to_path_buf(),
        env,
    )
    .ok()?;
    command
        .has_program_digest(&executable.digest)
        .then_some(command)
}

/// The offer an analyzer module receives: the declaration's accepted files and admitted `roots`
/// as its provider grant ([`ProviderGrant`]) and the language's settings value.
#[allow(clippy::too_many_arguments)]
pub fn analyzer_offer(
    executable: &ModuleExecutable,
    language: &str,
    instance: u64,
    worktree: &WorktreeRef,
    accepted: Vec<(PathBuf, String)>,
    roots: Vec<PathBuf>,
    request_timeout: Duration,
    settings: Value,
) -> HelloOffer {
    HelloOffer {
        protocol: PROTOCOL.to_owned(),
        versions: vec![VERSION],
        module_id: ModuleId::bundled(language),
        package_version: env!("CARGO_PKG_VERSION").to_owned(),
        executable_digest: executable.digest.to_hex().to_string(),
        instance,
        role: Role::Analyzer,
        limits: Limits::default(),
        requested_caps: vec![Capability::Semantic, Capability::Outline],
        config: ModuleConfig {
            worktree: Some(worktree.worktree_path().to_path_buf()),
            provider: Some(json!({
                "grant": ProviderGrant {
                    accepted,
                    request_timeout_ms: request_timeout.as_millis() as u64,
                    roots,
                },
                "settings": settings,
            })),
            checks: None,
            env: module_env(language),
            home: crate::userhome::user_home(),
        },
    }
}

/// Restart history and deterministic quarantine of one provider-hosted analyzer scope.
#[derive(Default)]
struct Policy {
    /// The same budget a supervised slot keeps.
    budget: super::runtime::RestartBudget,
    /// A deterministic refusal and the inputs it was observed with.
    blocked: Option<(String, super::contract::Stage, super::contract::Cause)>,
}

/// Policies by `(language, worktree)`.
static POLICIES: std::sync::Mutex<Vec<((String, PathBuf), Policy)>> =
    std::sync::Mutex::new(Vec::new());

/// Runs `f` on the policy of `language` in `worktree`.
fn with_policy<T>(
    language: &str,
    worktree: &std::path::Path,
    f: impl FnOnce(&mut Policy) -> T,
) -> T {
    let mut policies = POLICIES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (language.to_owned(), worktree.to_path_buf());
    let index = match policies.iter().position(|(known, _)| *known == key) {
        Some(index) => index,
        None => {
            policies.push((key, Policy::default()));
            policies.len() - 1
        }
    };
    f(&mut policies[index].1)
}

/// The typed failure of `language`'s analyzer at `stage` with `cause`.
fn refusal(
    language: &str,
    stage: super::contract::Stage,
    cause: super::contract::Cause,
    retry: Option<tokio::time::Instant>,
) -> super::contract::ModuleUnavailable {
    super::contract::ModuleUnavailable {
        module_id: ModuleId::bundled(language),
        module_version: env!("CARGO_PKG_VERSION").to_owned(),
        role: Role::Analyzer,
        stage,
        cause,
        instance: None,
        retry_after_ms: retry.map(|at| {
            at.saturating_duration_since(tokio::time::Instant::now())
                .as_millis() as u64
        }),
    }
}

/// Whether a provider-hosted analyzer of `language` in `worktree` may start now with accepted
/// `inputs` (the executable digest and settings): the same restart budget and backoff as a
/// supervised slot (waiting out a backoff that ends before `deadline`), and a deterministic
/// refusal repeats until the inputs change.
pub async fn start_permit(
    language: &str,
    worktree: &std::path::Path,
    inputs: &str,
    deadline: tokio::time::Instant,
) -> Result<(), super::contract::ModuleUnavailable> {
    use super::runtime::Permit;
    let permit = with_policy(language, worktree, |policy| {
        if let Some((blocked, stage, cause)) = &policy.blocked {
            if blocked == inputs {
                return Err(refusal(language, *stage, *cause, None));
            }
            policy.blocked = None;
        }
        Ok(policy.budget.permit(tokio::time::Instant::now()))
    })?;
    match permit {
        Permit::Now => Ok(()),
        Permit::After(at) if at <= deadline => {
            tokio::time::sleep_until(at).await;
            Ok(())
        }
        Permit::After(at) | Permit::Exhausted(at) => Err(refusal(
            language,
            super::contract::Stage::Spawn,
            super::contract::Cause::RestartExhausted,
            Some(at),
        )),
    }
}

/// Records that a provider-hosted analyzer of `language` in `worktree` started with `inputs`
/// failed with `failure`: a deterministic cause quarantines those inputs, any other counts
/// against the restart budget. An admission refusal is not a crash.
pub fn record_failure(
    language: &str,
    worktree: &std::path::Path,
    inputs: &str,
    failure: &super::contract::ModuleUnavailable,
) {
    if failure.stage == super::contract::Stage::Admission {
        return;
    }
    with_policy(language, worktree, |policy| {
        if super::runtime::deterministic(failure.cause) {
            policy.blocked = Some((inputs.to_owned(), failure.stage, failure.cause));
        } else {
            policy.budget.record(tokio::time::Instant::now());
        }
    });
}

/// Opens the session of an analyzer module on its protocol pipes (its stdout, its stdin): `hello`
/// within 30 s, then a [`LiveSession`] whose requests go to the module.
#[allow(clippy::too_many_arguments)]
pub async fn open_session<R, W>(
    stdout: R,
    stdin: W,
    offer: HelloOffer,
    worktree: WorktreeRef,
    epoch: u64,
    generation: ViewGeneration,
    settings: ProviderSettings,
    request_timeout: Duration,
) -> io::Result<LiveSession>
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
    W: tokio::io::AsyncWrite + Send + Sync + Unpin + 'static,
{
    let (channel, reply) = HostChannel::open(stdout, stdin, offer, HELLO_BUDGET)
        .await
        .map_err(io::Error::other)?;
    let calls = reply
        .capabilities
        .iter()
        .any(|decl| decl.capability == Capability::Calls && decl.support == Support::Supported);
    LiveSession::open_module(
        channel,
        calls,
        worktree,
        epoch,
        generation,
        settings,
        request_timeout,
    )
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    use crate::modules::contract::{Cause, Stage};

    /// A provider-hosted analyzer scope spends the same restart budget as a supervised slot, and
    /// a deterministic refusal repeats until its inputs change; an admission refusal is no crash.
    #[tokio::test(start_paused = true)]
    async fn provider_analyzers_share_the_restart_policy() {
        let worktree = std::path::Path::new("/policy-test");
        let soon = || tokio::time::Instant::now() + Duration::from_secs(10);
        let failed = |stage, cause| refusal("alpha", stage, cause, None);
        for _ in 0..4 {
            start_permit("alpha", worktree, "inputs-1", soon())
                .await
                .unwrap();
            record_failure(
                "alpha",
                worktree,
                "inputs-1",
                &failed(Stage::Request, Cause::Exited),
            );
        }
        let exhausted = start_permit("alpha", worktree, "inputs-1", soon())
            .await
            .unwrap_err();
        assert_eq!(exhausted.cause, Cause::RestartExhausted);
        assert!(exhausted.retry_after_ms.is_some());
        let other = std::path::Path::new("/policy-test-2");
        record_failure(
            "alpha",
            other,
            "inputs-1",
            &failed(Stage::Admission, Cause::ResourceLimit),
        );
        record_failure(
            "alpha",
            other,
            "inputs-1",
            &failed(Stage::Hello, Cause::Incompatible),
        );
        assert_eq!(
            start_permit("alpha", other, "inputs-1", soon())
                .await
                .unwrap_err()
                .cause,
            Cause::Incompatible
        );
        start_permit("alpha", other, "inputs-2", soon())
            .await
            .unwrap();
    }
}
