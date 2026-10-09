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
    launch::{MODULE_ENV, ModuleExecutable},
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
    let mut env: BTreeMap<OsString, OsString> = MODULE_ENV
        .iter()
        .filter_map(|key| Some((OsString::from(key), std::env::var_os(key)?)))
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

/// The offer an analyzer module receives: the declaration's accepted files as its provider grant
/// and the language's settings value.
pub fn analyzer_offer(
    executable: &ModuleExecutable,
    language: &str,
    instance: u64,
    accepted: Vec<(PathBuf, String)>,
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
            provider: Some(json!({
                "grant": ProviderGrant {
                    accepted,
                    request_timeout_ms: request_timeout.as_millis() as u64,
                },
                "settings": settings,
            })),
            checks: None,
            env: MODULE_ENV
                .iter()
                .filter_map(|name| Some(((*name).to_owned(), std::env::var(name).ok()?)))
                .collect(),
            home: crate::userhome::user_home(),
        },
    }
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
        .map_err(|failure| io::Error::other(failure.to_string()))?;
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
