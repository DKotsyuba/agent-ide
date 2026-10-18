//! Optional protected Bridge producer boundary. Routes carry delivery identity only;
//! Workspace authority and every local binding/source guard remain IDE-owned.

use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use super::{
    host_binding::BindingRef,
    outbox::{FactDraft, FactKind},
};
use crate::app::store::{
    DomainMigration, DomainName, MigrationAdmission, MigrationDigest, MigrationKey, OperationId,
    Store, StoreError, TrustedUpSql,
};
use bridge_sdk::{Client, Config, bridge_protocol as wire};
use serde::Deserialize;
use serde_json::{Map, Value};

/// Opaque test completion kind owned by IDE, with producer-composed bounded result text.
pub(crate) const TEST_KIND: &str = "ide.test.finished";
/// Opaque edit-check completion kind owned by IDE; Bridge never interprets diagnostics.
pub(crate) const EDIT_KIND: &str = "ide.edit.checked";
/// Private frontend-to-daemon route association; never a model argument.
pub(crate) const ROUTE_META: &str = "agent-ide/bridge-route";
/// One Bridge SDK exchange budget. Five seconds is the standalone SDK's documented baseline;
/// native-owner handoff keeps its separate Bridge-owned deadline after custody is committed.
const BRIDGE_SDK_DEADLINE: Duration = Duration::from_secs(5);

/// Explicit restart-only deployment configuration; absent means legacy delivery.
/// Endpoint parents and credential must be private, current-user-owned, real paths.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeConfig {
    /// Absolute protected native-registration endpoint selected by the operator.
    pub registration_socket: PathBuf,
    /// Absolute protected publication/lifetime endpoint selected by the operator.
    pub core_socket: PathBuf,
    /// Absolute regular owner-only credential file, read once; never an inline secret.
    pub credential_file: PathBuf,
    /// Operator-selected native stores; actor ids still come only from validated IDE bindings.
    /// An empty list retains historical route compatibility during migration, never guesses a store.
    #[serde(default)]
    pub native_stores: Vec<NativeStore>,
}

/// One configured native store prefix for direct-address delivery, never caller authority.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeStore {
    /// Supported native harness whose independently validated actor owns the result.
    pub harness: wire::Host,
    /// Explicit native OS host namespace, not derived from a worktree or process.
    pub host: String,
    /// Explicit native home/store namespace admitted by the operator’s Bridge profile.
    pub store: String,
}

impl BridgeConfig {
    /// Checks normalized absolute paths without I/O or granting host/workspace authority.
    pub(crate) fn validate(&self) -> bool {
        if self.native_stores.len() > 2
            || self
                .native_stores
                .iter()
                .enumerate()
                .any(|(index, native)| {
                    wire::validate_id("host", &native.host, wire::MAX_ID_BYTES).is_err()
                        || wire::validate_id("store", &native.store, wire::MAX_ID_BYTES).is_err()
                        || self.native_stores[..index]
                            .iter()
                            .any(|prior| prior.harness == native.harness)
                })
        {
            return false;
        }
        [
            &self.registration_socket,
            &self.core_socket,
            &self.credential_file,
        ]
        .into_iter()
        .all(|path| {
            path.is_absolute()
                && path.components().all(|component| {
                    matches!(
                        component,
                        std::path::Component::RootDir | std::path::Component::Normal(_)
                    )
                })
        })
    }

    /// Reads only the configured bounded no-follow credential, validating private parent
    /// ownership and modes first. Invalid deployment disables Bridge; no fallback discovery.
    pub(crate) fn open(&self) -> Option<Arc<BridgeClient>> {
        if !self.validate() {
            return None;
        }
        for path in [
            &self.registration_socket,
            &self.core_socket,
            &self.credential_file,
        ] {
            let parent = path.parent()?;
            let metadata = std::fs::symlink_metadata(parent).ok()?;
            if !metadata.is_dir()
                || !private_owner(&metadata)
                || parent.canonicalize().ok()? != parent
            {
                return None;
            }
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&self.credential_file)
            .ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || !private_owner(&metadata) || metadata.len() > 16 * 1024 {
            return None;
        }
        let mut credential = String::new();
        file.take(16 * 1024 + 1)
            .read_to_string(&mut credential)
            .ok()?;
        if credential.len() > 16 * 1024 {
            return None;
        }
        let owner = identity(
            "sender-owner",
            &[
                &self.registration_socket.to_string_lossy(),
                &self.core_socket.to_string_lossy(),
                blake3::hash(credential.trim_end_matches(['\r', '\n']).as_bytes())
                    .to_hex()
                    .as_str(),
            ],
        );
        let config = Config::new(
            &self.registration_socket,
            &self.core_socket,
            wire::Secret::new(credential.trim_end_matches(['\r', '\n'])),
            BRIDGE_SDK_DEADLINE,
        )
        .ok()?;
        Some(Arc::new(BridgeClient {
            client: Client::new(config).ok()?,
            owner,
            capabilities: Default::default(),
            registration_slots: Arc::new(tokio::sync::Semaphore::new(8)),
        }))
    }
}

/// Verifies current effective uid and owner-only access, never changing permissions.
fn private_owner(metadata: &std::fs::Metadata) -> bool {
    // SAFETY: geteuid has no arguments and does not mutate process state.
    metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0
}

/// Shared synchronous SDK behind finite blocking tasks; no metadata or credential Debug.
pub(crate) struct BridgeClient {
    /// Approved standalone SDK; owns its protected credential and finite socket budget.
    client: Client,
    /// Opaque endpoint/credential-owner pin for immutable handoff evidence; never printed.
    owner: String,
    /// Sixty-second supported gate, five-second failure gate; original calls remain fresh.
    capabilities: std::sync::Mutex<Option<(std::time::Instant, Option<wire::Capabilities>)>>,
    /// Bounds still-running blocking registrations after an inline wait expires.
    registration_slots: Arc<tokio::sync::Semaphore>,
}

/// Current same-call route association. It is routing data, never admission evidence.
#[derive(Clone)]
pub(crate) struct RouteTicket {
    /// Opaque Bridge route reference, never printed or persisted as authority.
    pub(crate) route_key: String,
    /// Exact original native actor, checked against the local admitted binding.
    pub(crate) actor: String,
    /// Exact native call id, checked against the private RPC correlation.
    pub(crate) call: String,
}

impl RouteTicket {
    /// Decodes only the private exact-call association and verifies the independently
    /// admitted local actor. The ticket never grants authority or replaces admission.
    pub(crate) fn from_private(value: &Value, binding: &BindingRef, call: &str) -> Option<Self> {
        let object = value.as_object()?;
        if object.len() != 3 {
            return None;
        }
        let route_key = object.get("route_key")?.as_str()?.to_owned();
        let actor = object.get("actor")?.as_str()?.to_owned();
        let ticket_call = object.get("call")?.as_str()?.to_owned();
        wire::validate_id("route", &route_key, wire::MAX_ID_BYTES).ok()?;
        if binding.host() != super::host_binding::HostKind::Codex
            || !binding.actor_is(&actor)
            || ticket_call != call
        {
            return None;
        }
        Some(Self {
            route_key,
            actor,
            call: ticket_call,
        })
    }
}

/// One frozen delivery destination; neither variant grants caller or workspace authority.
#[derive(Clone)]
pub(crate) enum DeliveryTarget {
    /// Historical same-call route, retained temporarily for old deployment compatibility.
    Route(RouteTicket),
    /// Exact native address built from operator store configuration and validated local actor.
    Address(String),
}

impl From<RouteTicket> for DeliveryTarget {
    /// Preserves historical private route callers during the direct-address migration.
    fn from(route: RouteTicket) -> Self {
        Self::Route(route)
    }
}

impl DeliveryTarget {
    /// Produces the frozen SDK destination without inferring any local execution permission.
    fn destination(&self) -> wire::Destination {
        match self {
            Self::Route(route) => wire::Destination::Route {
                route_key: route.route_key.clone(),
            },
            Self::Address(address) => wire::Destination::Address {
                address: address.clone(),
            },
        }
    }
}

/// One optional daemon delivery owner, using the existing Application SQLite store.
/// Immutable destination/body/sender linkage is retained beside domain outcome metadata before
/// publication. Authorized result content is carried by Bridge; this class owns no sender loop.
pub(crate) struct BridgeDelivery {
    /// Protected producer client shared across finite blocking exchanges.
    client: Arc<BridgeClient>,
    /// Existing daemon store; retains only opaque ids and custody dispositions.
    store: Arc<Store>,
    /// At most 64 current local generations with same-actor delivery routes.
    routes: std::sync::Mutex<std::collections::BTreeMap<BindingRef, DeliveryTarget>>,
    /// Explicit configured store prefixes, independent of caller/operation admission.
    native_stores: Vec<NativeStore>,
    /// Boot fence prevents restarted generation numbers from reusing delivery scopes.
    boot: String,
    /// Eight concurrent exchanges maximum; saturation keeps legacy delivery.
    permits: Arc<tokio::sync::Semaphore>,
}

impl BridgeDelivery {
    /// Installs bounded domain intent storage and retires every retained old scope before
    /// enabling this boot. Any unavailable schema/recovery disables Bridge, preserving IDE.
    pub(crate) async fn open(
        config: &BridgeConfig,
        store: Arc<Store>,
        nonce: [u8; 32],
    ) -> Option<Arc<Self>> {
        let client = config.open()?;
        let sql = TrustedUpSql::new("CREATE TABLE ide_bridge_delivery(event_id TEXT PRIMARY KEY,scope_key TEXT NOT NULL,state TEXT NOT NULL,expires_at_ms INTEGER NOT NULL) STRICT; CREATE INDEX ide_bridge_scope ON ide_bridge_delivery(scope_key);").ok()?;
        let admission = store
            .admit_migration(DomainMigration {
                domain: DomainName::new("ide_bridge").ok()?,
                key: MigrationKey::new("delivery_intent_v1").ok()?,
                expected_digest: MigrationDigest::from_sql(&sql),
                up_sql: sql,
            })
            .await
            .ok()?;
        if !matches!(
            admission,
            MigrationAdmission::Applied { .. } | MigrationAdmission::AlreadyApplied { .. }
        ) {
            return None;
        }
        let sql = TrustedUpSql::new("CREATE TABLE ide_bridge_intention(event_id TEXT PRIMARY KEY REFERENCES ide_bridge_delivery(event_id) ON DELETE CASCADE,destination TEXT NOT NULL,payload TEXT NOT NULL,sender_owner TEXT NOT NULL) STRICT;").ok()?;
        if !matches!(
            store
                .admit_migration(DomainMigration {
                    domain: DomainName::new("ide_bridge").ok()?,
                    key: MigrationKey::new("delivery_intention_v2").ok()?,
                    expected_digest: MigrationDigest::from_sql(&sql),
                    up_sql: sql
                })
                .await
                .ok()?,
            MigrationAdmission::Applied { .. } | MigrationAdmission::AlreadyApplied { .. }
        ) {
            return None;
        }
        // Expired evidence follows the existing seventy-minute retention even if no new
        // publication can start; foreign nonexpired effects are never silently rehomed.
        let expired_before = now_ms()?.saturating_sub(60 * 60 * 1000);
        store
            .execute_untracked_settled(move |tx| {
                tx.execute(
                    "DELETE FROM ide_bridge_delivery WHERE expires_at_ms<?1",
                    [expired_before],
                )?;
                Ok(())
            })
            .await
            .ok()?;
        let scopes: Vec<(String, Option<String>)> = store.read_many(
            "SELECT DISTINCT d.scope_key,i.sender_owner FROM ide_bridge_delivery d LEFT JOIN ide_bridge_intention i ON i.event_id=d.event_id WHERE d.state!='retired' LIMIT 1001",
            vec![], 1001, |row| Ok((row.get(0)?, row.get(1)?)),
        ).await.ok()?;
        // Old/foreign handoff ownership is never silently rebound to a new credential/service.
        if scopes
            .iter()
            .any(|(_, owner)| owner.as_deref() != Some(client.owner.as_str()))
        {
            return None;
        }
        if scopes.len() > 1000 {
            return None;
        }
        let delivery = Arc::new(Self {
            client,
            store: Arc::clone(&store),
            routes: Default::default(),
            native_stores: config.native_stores.clone(),
            boot: blake3::Hash::from_bytes(nonce).to_hex().to_string(),
            permits: Arc::new(tokio::sync::Semaphore::new(8)),
        });
        for (scope, _) in scopes {
            if !delivery.retire(scope).await {
                return None;
            }
        }
        Some(delivery)
    }

    /// Remembers a route only for an admitted current generation; capacity never evicts
    /// another actor's existing route. Caller already checked exact RPC actor/call linkage.
    pub(crate) fn note_route(&self, binding: &BindingRef, ticket: RouteTicket) {
        if let Ok(mut routes) = self.routes.lock()
            && (routes.contains_key(binding) || routes.len() < 64)
        {
            routes.insert(binding.clone(), DeliveryTarget::Route(ticket));
        }
    }

    /// Checks the direct-address/auto/result-kind grant without blocking the async worker on IPC.
    /// This is a bounded admission observation, never proof of later native arrival.
    pub(crate) async fn direct_capable(&self) -> bool {
        let client = Arc::clone(&self.client);
        tokio::task::spawn_blocking(move || {
            client
                .capability_snapshot("ide-direct-capabilities")
                .is_some_and(|caps| usable_direct(&caps))
        })
        .await
        .unwrap_or(false)
    }

    /// Checks whether this boot has an explicit address for a validated local actor. This is
    /// recipient selection only, not a claim of Bridge health, native arrival or source authority.
    pub(crate) fn can_target(&self, binding: &BindingRef) -> bool {
        self.known_address(binding).is_some()
    }

    /// Resolves only the configured prefix plus the positively validated binding actor.
    /// Missing stores refuse rather than guessing a home, parent actor or other recipient.
    fn known_address(&self, binding: &BindingRef) -> Option<String> {
        let harness = match binding.host() {
            super::host_binding::HostKind::Codex => wire::Host::Codex,
            super::host_binding::HostKind::Claude => wire::Host::ClaudeCode,
        };
        let native = self
            .native_stores
            .iter()
            .find(|native| native.harness == harness)?;
        wire::validate_id("session", binding.native_actor_id(), wire::MAX_ID_BYTES).ok()?;
        Some(
            wire::NativeAddress {
                host: native.host.clone(),
                harness,
                store: native.store.clone(),
                session: binding.native_actor_id().to_owned(),
            }
            .canonical(),
        )
    }

    /// Creates one boot-and-generation-scoped lifetime identity, independent of actor text.
    fn scope(&self, binding: &BindingRef) -> String {
        identity(
            "scope",
            &[
                &self.boot,
                blake3::Hash::from_bytes(binding.fingerprint())
                    .to_hex()
                    .as_ref(),
            ],
        )
    }

    /// Reserves the sole delivery owner synchronously before any legacy push. Returns
    /// `None` when no eligible route/capacity exists; otherwise the caller must await the
    /// owned attempt and fall back only for a positive no-publication result.
    /// Success, failure and source-bearing drafts use the same transport; the producer must
    /// prove source access/currency before calling. Configured exact destinations take priority.
    pub(crate) fn reserve(
        &self,
        binding: &BindingRef,
        draft: &FactDraft,
    ) -> Option<(DeliveryTarget, tokio::sync::OwnedSemaphorePermit)> {
        draft.validate().ok()?;
        if self.native_stores.is_empty()
            && (draft.source_bearing
                || draft.failed
                || !matches!(draft.kind, FactKind::TestFinished | FactKind::EditChecked))
        {
            return None;
        }
        let permit = Arc::clone(&self.permits).try_acquire_owned().ok()?;
        let address = self.known_address(binding);
        let mut routes = self.routes.lock().ok()?;
        if let Some(address) = address {
            if !routes.contains_key(binding) && routes.len() >= 64 {
                return None;
            }
            routes.insert(binding.clone(), DeliveryTarget::Address(address));
        }
        let destination = routes.get(binding)?.clone();
        Some((destination, permit))
    }

    /// Durably records the stable intent, then publishes a closed completion notice.
    /// `true` means custody OR uncertainty: never send through a second carrier. `false`
    /// is positive no-write evidence, including a route retired after preparation but before
    /// socket I/O. No native/model observation is inferred. Cancelled generation retirement
    /// fences new custody; callers retain domain work instead of adding a fallback carrier.
    pub(crate) async fn publish(
        &self,
        binding: BindingRef,
        draft: &FactDraft,
        target: impl Into<DeliveryTarget>,
    ) -> bool {
        let target = target.into();
        let direct = matches!(target, DeliveryTarget::Address(_));
        let destination = target.destination();
        let scope = self.scope(&binding);
        let event = event_identity(&scope, draft);
        let Some(expires) = now_ms().and_then(|now| now.checked_add(10 * 60 * 1000)) else {
            return false;
        };
        let payload = if direct {
            format!(
                "{} {}: {}{}",
                draft.kind.label(),
                draft.subject,
                draft.summary,
                draft
                    .detail
                    .as_ref()
                    .map(|detail| format!("\n{detail}"))
                    .unwrap_or_default()
            )
        } else {
            format!(
                "{}: completion recorded{}",
                draft.kind.label(),
                draft
                    .detail_ref
                    .as_ref()
                    .filter(
                        |reference| wire::validate_id("reference", reference, 128).is_ok()
                            && !reference.contains(['<', '>'])
                    )
                    .map(|reference| format!(" [ref {reference}]"))
                    .unwrap_or_default()
            )
        };
        let stored_destination =
            serde_json::to_string(&destination).expect("typed SDK destination");
        let stored_payload = payload.clone();
        let sender_owner = self.client.owner.clone();
        let operation = OperationId::new(format!("bridge-prepare-{event}"))
            .expect("bounded internal operation");
        let insert_event = event.clone();
        let insert_scope = scope.clone();
        let prepared = self
            .store
            .execute(operation, move |transaction| {
                transaction.execute(
                    "DELETE FROM ide_bridge_delivery WHERE expires_at_ms<?1",
                    [expires.saturating_sub(70 * 60 * 1000)],
                )?;
                let count: i64 = transaction.query_row(
                    "SELECT count(*) FROM ide_bridge_delivery",
                    [],
                    |row| row.get(0),
                )?;
                if count >= 4096 {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                transaction.execute(
                    "INSERT INTO ide_bridge_delivery VALUES(?1,?2,'intent',?3)",
                    rusqlite::params![insert_event, insert_scope, expires],
                )?;
                transaction.execute(
                    "INSERT INTO ide_bridge_intention VALUES(?1,?2,?3,?4)",
                    rusqlite::params![
                        insert_event,
                        stored_destination,
                        stored_payload,
                        sender_owner
                    ],
                )?;
                Ok(())
            })
            .await;
        if let Err(error) = prepared {
            // Duplicate/unknown preparation is conserved. A committed old intent may
            // already have reached Bridge; never invent a new event or legacy replay.
            return matches!(
                error,
                StoreError::DuplicateOperation { .. } | StoreError::ReceiptExpired
            );
        }
        if !self
            .routes
            .lock()
            .ok()
            .is_some_and(|routes| routes.contains_key(&binding))
        {
            let _ = self.set_state(event, "retired").await;
            return false;
        }
        let kind = match draft.kind {
            FactKind::TestFinished => TEST_KIND,
            FactKind::EditChecked => EDIT_KIND,
            FactKind::JobReady => "ide.job.finished",
            FactKind::LostAfterRestart => "ide.job.lost",
        };
        let client = Arc::clone(&self.client);
        let sent_event = event.clone();
        let result = tokio::task::spawn_blocking(move || {
            client.client.publish(
                sent_event.clone(),
                wire::PublishParams {
                    destination,
                    event_id: sent_event,
                    kind: kind.into(),
                    payload: wire::Payload::new(payload).expect("closed bounded completion notice"),
                    mode: if direct {
                        wire::Mode::Auto
                    } else {
                        wire::Mode::Message
                    },
                    policy: wire::Policy::KeepAll,
                    metadata: Default::default(),
                    expires_at_ms: Some(expires),
                    delivery_scope_key: Some(scope),
                },
            )
        })
        .await;
        let state = match &result {
            Ok(Ok(result)) if result.receipt.event_id == event => "custody",
            Ok(Err(error)) if definitely_unsent(error) => {
                crate::errorlog::record(
                    crate::errorlog::Method::Daemon,
                    crate::errorlog::Outcome::Failed,
                    crate::errorlog::Fields {
                        detail: Some(refusal_detail(error)),
                        ..Default::default()
                    },
                );
                "refused"
            }
            _ => "unknown",
        };
        let _ = self.set_state(event, state).await;
        state != "refused"
    }

    /// Settles only an open intent; a late publication answer can never reopen retirement.
    /// The idempotent domain update reuses the existing receipt-free serialized store writer.
    async fn set_state(&self, event: String, state: &'static str) -> bool {
        self.store
            .execute_untracked_settled(move |transaction| {
                transaction.execute(
                    "UPDATE ide_bridge_delivery SET state=?1 WHERE event_id=?2 AND state='intent'",
                    rusqlite::params![state, event],
                )?;
                Ok(())
            })
            .await
            .is_ok()
    }

    /// Drops the local route immediately and installs the permanent Bridge scope fence.
    /// Failure retains durable uncertainty; accepted possible native effects are preserved.
    pub(crate) fn end_scope(self: &Arc<Self>, binding: &BindingRef) {
        if let Ok(mut routes) = self.routes.lock() {
            routes.remove(binding);
        }
        let scope = self.scope(binding);
        let delivery = Arc::clone(self);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = delivery.retire(scope).await;
            });
        }
    }

    /// Removes every owned route during daemon shutdown and schedules its permanent fence.
    /// It never discovers foreign scopes or recalls already possible native effects.
    pub(crate) fn end_all(self: &Arc<Self>) {
        let bindings = self
            .routes
            .lock()
            .ok()
            .map(|routes| routes.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for binding in bindings {
            self.end_scope(&binding);
        }
    }

    /// Idempotently retires an opaque scope without replaying publications or recalling effects.
    async fn retire(&self, scope: String) -> bool {
        let client = Arc::clone(&self.client);
        let sent_scope = scope.clone();
        if !matches!(
            tokio::task::spawn_blocking(move || client.client.retire_delivery_scope(
                identity("retire", &[&sent_scope]),
                wire::RetireDeliveryScopeParams {
                    scope_key: sent_scope,
                    action: wire::RetirementAction::RetireUnhanded
                }
            ))
            .await,
            Ok(Ok(ref result)) if result.receipt.scope_key == scope
                && result.receipt.action == wire::RetirementAction::RetireUnhanded
                && result.receipt.request_id == identity("retire", &[&scope])
        ) {
            return false;
        }
        self.store
            .execute_untracked_settled(move |transaction| {
                transaction.execute(
                    "UPDATE ide_bridge_delivery SET state='retired' WHERE scope_key=?1",
                    [scope],
                )?;
                Ok(())
            })
            .await
            .is_ok()
    }
}

/// Positive no-effect classification; conflicts/unknown receipts never permit legacy replay.
fn definitely_unsent(error: &bridge_sdk::ClientError) -> bool {
    match error {
        bridge_sdk::ClientError::InvalidConfig(_) | bridge_sdk::ClientError::InvalidRequest(_) => {
            true
        }
        bridge_sdk::ClientError::Transport {
            outcome: bridge_sdk::TransportOutcome::NotSent,
            ..
        } => true,
        bridge_sdk::ClientError::Service(error) => {
            error.effect == Some(wire::MutationEffect::NotCommitted)
                && !matches!(
                    error.class,
                    wire::ErrorClass::Conflict | wire::ErrorClass::OutcomeUnknown
                )
        }
        _ => false,
    }
}

/// Returns one content-free diagnostic for a proven no-effect publication refusal. Transport
/// reasons are SDK-owned static strings; service bodies, addresses and reconciliation ids stay out.
fn refusal_detail(error: &bridge_sdk::ClientError) -> &'static str {
    match error {
        bridge_sdk::ClientError::InvalidConfig(reason) => reason,
        bridge_sdk::ClientError::InvalidRequest(_) => "bridge:publish_refused:invalid_request",
        bridge_sdk::ClientError::Service(_) => "bridge:publish_refused:service_not_committed",
        bridge_sdk::ClientError::Transport { reason, .. } => reason,
    }
}

impl std::fmt::Debug for RouteTicket {
    /// Prints no native actor, call or route identifier.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RouteTicket(<redacted>)")
    }
}

impl BridgeClient {
    /// Caches only service/grant capabilities for60s (failed reads5s), never native actor proof.
    fn capability_snapshot(&self, id: &str) -> Option<wire::Capabilities> {
        if let Some((at, caps)) = self
            .capabilities
            .lock()
            .ok()
            .and_then(|cache| cache.clone())
            && at.elapsed() < Duration::from_secs(if caps.is_some() { 60 } else { 5 })
        {
            return caps;
        }
        let caps = self.client.capabilities(format!("cap-{id}")).ok();
        if let Ok(mut cache) = self.capabilities.lock() {
            *cache = Some((std::time::Instant::now(), caps.clone()));
        }
        caps
    }

    /// Keeps the historical registration gate separate from direct-address publication support.
    fn usable(&self, id: &str) -> bool {
        self.capability_snapshot(id)
            .is_some_and(|caps| usable(&caps))
    }

    /// Registers original Codex direct-call metadata once before local dispatch. Exact
    /// native-call-derived ids survive reconnect without new operations; historical replay
    /// never attaches a route. Unavailable/unsupported Bridge leaves legacy delivery intact.
    /// The inline wait is at most 250 ms; eight blocking registrations may continue
    /// behind it. Late tickets are discarded. Capability support is cached for 60 seconds
    /// and failure for five; original native evidence is never cached or reused.
    pub(crate) async fn register(
        self: &Arc<Self>,
        meta: &Map<String, Value>,
    ) -> Option<RouteTicket> {
        let evidence =
            bridge_sdk::extract_native_evidence(wire::Host::Codex, &Value::Object(meta.clone()))
                .ok()?;
        let actor = meta.get("threadId")?.as_str()?.to_owned();
        let call = meta.get("callId")?.as_str()?.to_owned();
        let identity = identity("registration", &[&actor, &call]);
        let permit = Arc::clone(&self.registration_slots)
            .try_acquire_owned()
            .ok()?;
        let client = Arc::clone(self);
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            if !client.usable(&identity) {
                return None;
            }
            let result = client
                .client
                .register_call(
                    identity.clone(),
                    wire::RegisterCallParams {
                        operation_key: identity.clone(),
                        evidence: Some(evidence),
                        reply_policy: wire::ReplyPolicy::SelfActor,
                        allowed_modes: Some(vec![wire::Mode::Message]),
                        allowed_kinds: Some(vec![TEST_KIND.into(), EDIT_KIND.into()]),
                        max_payload_bytes: Some(1024),
                        route_expires_at_ms: Some(now_ms()?.checked_add(15 * 60 * 1000)?),
                        assertion_audience: None,
                        invocation_key: None,
                    },
                )
                .ok()?;
            let receipt = result.receipt;
            if result.evidence_state != wire::CallerEvidenceState::VerifiedAtRegistration
                || result.assertion.is_some()
                || receipt.request_id != identity
                || receipt.operation_key != identity
                || receipt.state != wire::RouteState::Open
                || receipt.reply_policy != wire::ReplyPolicy::SelfActor
                || receipt.constraints.reply_policy != wire::ReplyPolicy::SelfActor
                || !receipt
                    .constraints
                    .allowed_modes
                    .contains(&wire::Mode::Message)
                || ![TEST_KIND, EDIT_KIND]
                    .into_iter()
                    .all(|kind| allowed(&receipt.constraints.allowed_kinds, kind))
                || receipt.constraints.max_payload_bytes < 1024
            {
                return None;
            }
            Some(RouteTicket {
                route_key: receipt.route_key,
                actor,
                call,
            })
        });
        // Expired frontend wait leaves at most an expiring unused route. It never attaches
        // a late ticket to a newer call, publishes a fact, or admits work through Bridge.
        tokio::time::timeout(Duration::from_millis(250), task)
            .await
            .ok()?
            .ok()
            .flatten()
    }
}

/// Requires actual configured Codex direct-MCP support and scoped message custody;
/// this does not claim live qualification, native arrival or model observation.
fn usable(capabilities: &wire::Capabilities) -> bool {
    capabilities.api_version == wire::API_VERSION
        && capabilities.features.delivery_scopes
        && ["register_call", "publish", "retire_delivery_scope"]
            .into_iter()
            .all(|operation| capabilities.operations.iter().any(|item| item == operation))
        && capabilities.native_contracts.iter().any(|contract| {
            contract.host == wire::Host::Codex
                && contract
                    .paths
                    .iter()
                    .any(|path| path.path == wire::HostPath::DirectMcp && path.supported)
        })
        && capabilities
            .grant
            .allowed_modes
            .contains(&wire::Mode::Message)
        && capabilities
            .grant
            .allowed_policies
            .contains(&wire::PolicyKind::KeepAll)
        && capabilities
            .grant
            .reply_policies
            .contains(&wire::ReplyPolicy::SelfActor)
        && capabilities.limits.payload_bytes >= 1024
        && [TEST_KIND, EDIT_KIND]
            .into_iter()
            .all(|kind| allowed(&capabilities.grant.allowed_kinds, kind))
}

/// Requires direct address custody, explicit auto mode, all result kinds and the lifetime fence
/// used by this consumer. Generic caller-registration paths do not gate address delivery.
fn usable_direct(caps: &wire::Capabilities) -> bool {
    caps.api_version == wire::API_VERSION
        && caps.grant.direct_address
        && caps.features.delivery_scopes
        && ["publish", "retire_delivery_scope"]
            .iter()
            .all(|op| caps.operations.iter().any(|item| item == op))
        && caps.modes.contains(&wire::Mode::Auto)
        && caps.grant.allowed_modes.contains(&wire::Mode::Auto)
        && caps.policies.contains(&wire::PolicyKind::KeepAll)
        && caps
            .grant
            .allowed_policies
            .contains(&wire::PolicyKind::KeepAll)
        && caps.limits.payload_bytes >= 8 * 1024
        && [TEST_KIND, EDIT_KIND, "ide.job.finished", "ide.job.lost"]
            .iter()
            .all(|kind| allowed(&caps.grant.allowed_kinds, kind))
}

/// Stable facts deduplicate; independent graph jobs on the same symbol keep their own result id.
fn event_identity(scope: &str, draft: &FactDraft) -> String {
    let job_reference = if draft.kind == FactKind::JobReady {
        draft.detail_ref.as_deref().unwrap_or("")
    } else {
        ""
    };
    identity(
        "event",
        &[scope, draft.kind.label(), &draft.subject, job_reference],
    )
}

/// Tests the authenticated closed kind grant, accepting only exact names or its wildcard.
fn allowed(kinds: &[String], kind: &str) -> bool {
    kinds.iter().any(|item| item == kind || item == "*")
}

/// Builds a domain-separated length-framed identity without exposing native identifiers.
pub(crate) fn identity(domain: &str, parts: &[&str]) -> String {
    let mut hash = blake3::Hasher::new();
    for part in std::iter::once(domain).chain(parts.iter().copied()) {
        hash.update(&(part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    format!("ide-{}", hash.finalize().to_hex())
}

/// Returns bounded Unix milliseconds; clock failure disables new custody rather than guessing.
pub(crate) fn now_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Direct-address admission needs explicit Auto, KeepAll, all result kinds and lifetime
    /// retirement; missing generic registration support does not disable this independent path.
    #[test]
    fn direct_capabilities_require_exact_delivery_contract() {
        let mut caps: wire::Capabilities = serde_json::from_value(serde_json::json!({
            "api_version":wire::API_VERSION, "supported_versions":[wire::API_VERSION],
            "operations":["publish","retire_delivery_scope"], "evidence_variants":[],
            "native_contracts":[], "modes":["auto"], "policies":["keep_all"], "reply_policies":[],
            "limits":{"registration_request_bytes":65536,"core_request_bytes":65536,"response_bytes":65536,
                "payload_bytes":32768,"metadata_entries":0,"rendered_manifest_bytes":65536,"open_routes":0,"assertions":0},
            "grant":{"allowed_modes":["auto"],"allowed_policies":["keep_all"],"allowed_kinds":["*"],
                "reply_policies":[],"direct_address":true,"native_scopes":["*"]},
            "features":{"consume_assertion":false,"delivery_scopes":true,"host_observations":false},
            "retention":{"durable_keys":true,"closed_tombstones":true,"credential_independent_identity":true}
        })).unwrap();
        assert!(usable_direct(&caps));
        for mutation in 0..6 {
            let mut changed = caps.clone();
            match mutation {
                0 => changed.grant.allowed_modes = vec![wire::Mode::Message, wire::Mode::Inject],
                1 => changed.grant.direct_address = false,
                2 => changed.features.delivery_scopes = false,
                3 => changed.grant.allowed_kinds = vec![TEST_KIND.into()],
                4 => changed
                    .operations
                    .retain(|op| op != "retire_delivery_scope"),
                _ => changed.policies.clear(),
            }
            assert!(!usable_direct(&changed), "mutation {mutation}");
        }
        caps.native_contracts.clear();
        assert!(
            usable_direct(&caps),
            "generic registration contracts are not direct delivery authority"
        );
    }

    /// Graph jobs for the same root have distinct immutable identities, while replaying one
    /// completion or a test result keeps its existing identity.
    #[test]
    fn graph_event_identity_distinguishes_jobs_on_same_symbol() {
        let mut fact = FactDraft {
            kind: FactKind::JobReady,
            subject: "a.rs#f".into(),
            summary: "done".into(),
            detail: None,
            detail_ref: Some("job-one".into()),
            failed: false,
            source_bearing: false,
            fence_paths: vec![],
        };
        let first = event_identity("scope", &fact);
        assert_eq!(first, event_identity("scope", &fact));
        fact.detail_ref = Some("job-two".into());
        assert_ne!(first, event_identity("scope", &fact));
        fact.kind = FactKind::TestFinished;
        let first = event_identity("scope", &fact);
        fact.detail_ref = Some("page-next".into());
        assert_eq!(first, event_identity("scope", &fact));
    }

    /// Restart never retires or adopts possible effects owned by a different credential/service.
    /// The foreign intent remains intact even when the currently configured endpoint is available.
    #[tokio::test]
    async fn recovery_refuses_foreign_sender_before_socket_io() {
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};
        let scratch = crate::scratch::ScratchDir::new("bridge-owner-fence");
        let config = BridgeConfig {
            registration_socket: scratch.join("adapter.sock"),
            core_socket: scratch.join("core.sock"),
            credential_file: scratch.join("token"),
            native_stores: vec![],
        };
        std::fs::write(&config.credential_file, "owned-fixture-token").unwrap();
        std::fs::set_permissions(
            &config.credential_file,
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let store = Arc::new(
            Store::open_with_backup_root(
                &scratch.join("state.sqlite"),
                &scratch.join("backups"),
                crate::app::config::EffectiveConfig::defaults().store(),
            )
            .unwrap(),
        );
        let delivery = BridgeDelivery::open(&config, store.clone(), [1; 32])
            .await
            .unwrap();
        store.execute_untracked_settled(|tx| {
            tx.execute("INSERT INTO ide_bridge_delivery VALUES('old','scope-old','unknown',?1)",[now_ms().unwrap()+600000])?;
            tx.execute("INSERT INTO ide_bridge_intention VALUES('old','destination','body','foreign-owner')",[])?;
            Ok(())
        }).await.unwrap();
        let listener = UnixListener::bind(&config.core_socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        assert!(
            BridgeDelivery::open(&config, store.clone(), [2; 32])
                .await
                .is_none()
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let state: String = store
            .read_one(
                "SELECT state FROM ide_bridge_delivery WHERE event_id='old'",
                vec![],
                |row| row.get(0),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state, "unknown");
        store
            .execute_untracked_settled(|tx| {
                tx.execute(
                    "UPDATE ide_bridge_delivery SET expires_at_ms=0 WHERE event_id='old'",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(
            BridgeDelivery::open(&config, store.clone(), [3; 32])
                .await
                .is_some(),
            "bounded expired evidence cannot poison future boots forever"
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(delivery);
    }

    /// Repeated retirement remains successful after a late inserted intent, while a late
    /// publication settle cannot reopen a retired row. No publication is replayed on recovery.
    #[tokio::test]
    async fn retirement_is_idempotent_and_late_settle_cannot_reopen_it() {
        use std::{
            io::{BufRead, BufReader, Write},
            os::unix::{fs::PermissionsExt, net::UnixListener},
        };
        let scratch = crate::scratch::ScratchDir::new("br-retire");
        let config = BridgeConfig {
            registration_socket: scratch.join("adapter.sock"),
            core_socket: scratch.join("core.sock"),
            credential_file: scratch.join("token"),
            native_stores: Vec::new(),
        };
        std::fs::write(&config.credential_file, "synthetic-token").unwrap();
        std::fs::set_permissions(
            &config.credential_file,
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let store = Arc::new(
            Store::open_with_backup_root(
                &scratch.join("state.sqlite"),
                &scratch.join("backups"),
                crate::app::config::EffectiveConfig::defaults().store(),
            )
            .unwrap(),
        );
        let delivery = BridgeDelivery::open(&config, Arc::clone(&store), [3; 32])
            .await
            .unwrap();
        let listener = UnixListener::bind(&config.core_socket).unwrap();
        let server = std::thread::spawn(move || {
            for attempt in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut frame = String::new();
                BufReader::new(&stream).read_line(&mut frame).unwrap();
                let request = wire::decode_request(frame.trim_end().as_bytes()).unwrap();
                let wire::Operation::RetireDeliveryScope(params) = request.operation else {
                    panic!("recovery must only retire, never publish");
                };
                let response = wire::Response::ok(
                    request.request_id.clone(),
                    wire::OperationResult::RetireDeliveryScope(wire::RetireResult {
                        receipt: wire::RetireReceipt {
                            producer: "fixture".into(),
                            request_id: request.request_id,
                            scope_key: if attempt == 2 {
                                "wrong-scope".into()
                            } else {
                                params.scope_key
                            },
                            action: params.action,
                            retired_at_ms: 1,
                            disposed: 0,
                            already_terminal: 0,
                            preserved_possible_effect: 0,
                            preserved_unhanded: 0,
                        },
                        replayed: false,
                    }),
                );
                stream
                    .write_all(&wire::encode_response(&response).unwrap())
                    .unwrap();
                stream.write_all(b"\n").unwrap();
            }
        });
        store
            .execute_untracked_settled(|transaction| {
                transaction.execute(
                    "INSERT INTO ide_bridge_delivery VALUES('event-a','scope-a','intent',100000)",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(delivery.retire("scope-a".into()).await);
        assert!(delivery.set_state("event-a".into(), "custody").await);
        let state: String = store
            .read_one(
                "SELECT state FROM ide_bridge_delivery WHERE event_id='event-a'",
                vec![],
                |row| row.get(0),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state, "retired");
        store
            .execute_untracked_settled(|transaction| {
                transaction.execute(
                    "INSERT INTO ide_bridge_delivery VALUES('event-b','scope-a','intent',100000)",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(delivery.retire("scope-a".into()).await);
        let states: Vec<String> = store
            .read_many(
                "SELECT state FROM ide_bridge_delivery ORDER BY event_id",
                vec![],
                2,
                |row| row.get(0),
            )
            .await
            .unwrap();
        assert_eq!(states, ["retired", "retired"]);
        store
            .execute_untracked_settled(|tx| {
                tx.execute(
                    "INSERT INTO ide_bridge_delivery VALUES('event-c','scope-c','intent',100000)",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(
            !delivery.retire("scope-c".into()).await,
            "a mismatched receipt cannot retire local state"
        );
        server.join().unwrap();
        let state: String = store
            .read_one(
                "SELECT state FROM ide_bridge_delivery WHERE event_id='event-c'",
                vec![],
                |row| row.get(0),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state, "intent");
        drop(delivery);
        drop(store);
    }

    /// A route removed after reservation but before publication is definite no-effect: the
    /// prepared intent retires, no transport call occurs and the caller keeps restart-loss work.
    /// Keep the scratch label short so socket paths fit macOS SUN_LEN under isolated TMPDIRs.
    #[tokio::test]
    async fn route_retired_before_publication_is_definite_no_effect() {
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};
        let scratch = crate::scratch::ScratchDir::new("br-retired");
        let config = BridgeConfig {
            registration_socket: scratch.join("adapter.sock"),
            core_socket: scratch.join("core.sock"),
            credential_file: scratch.join("token"),
            native_stores: vec![NativeStore {
                harness: wire::Host::ClaudeCode,
                host: "local".into(),
                store: "fixture".into(),
            }],
        };
        std::fs::write(&config.credential_file, "synthetic-token").unwrap();
        std::fs::set_permissions(
            &config.credential_file,
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let store = Arc::new(
            Store::open_with_backup_root(
                &scratch.join("state.sqlite"),
                &scratch.join("backups"),
                crate::app::config::EffectiveConfig::defaults().store(),
            )
            .unwrap(),
        );
        let delivery = BridgeDelivery::open(&config, Arc::clone(&store), [5; 32])
            .await
            .unwrap();
        let binding = BindingRef::fixture("actor", "channel", 1);
        let draft = FactDraft {
            kind: FactKind::LostAfterRestart,
            subject: "restart".into(),
            summary: "results may be lost".into(),
            detail: None,
            detail_ref: None,
            failed: false,
            source_bearing: false,
            fence_paths: vec![],
        };
        let (route, _permit) = delivery
            .reserve(&binding, &draft)
            .expect("direct route reserves");
        assert!(delivery.routes.lock().unwrap().remove(&binding).is_some());
        let listener = UnixListener::bind(&config.core_socket).unwrap();
        listener.set_nonblocking(true).unwrap();

        assert!(!delivery.publish(binding, &draft, route).await);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "retirement before publication performs no transport I/O"
        );
        let state: String = store
            .read_one("SELECT state FROM ide_bridge_delivery", vec![], |row| {
                row.get(0)
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state, "retired");
    }

    /// Exercises real SDK/socket handoff: immutable authorized body precedes bytes, useful
    /// success/failure content reaches Bridge, and uncertain effects never create another send.
    #[tokio::test]
    async fn publication_intent_and_body_precede_wire_and_unknown_never_replays() {
        use std::{
            io::{BufRead, BufReader, Write},
            os::unix::{fs::PermissionsExt, net::UnixListener},
        };
        for (class, effect, transferred) in [
            (
                wire::ErrorClass::Capacity,
                wire::MutationEffect::NotCommitted,
                false,
            ),
            (
                wire::ErrorClass::OutcomeUnknown,
                wire::MutationEffect::Unknown,
                true,
            ),
            (
                wire::ErrorClass::Conflict,
                wire::MutationEffect::NotCommitted,
                true,
            ),
            (
                wire::ErrorClass::Unavailable,
                wire::MutationEffect::Committed,
                true,
            ),
        ] {
            let scratch = crate::scratch::ScratchDir::new("bridge-publication");
            let config = BridgeConfig {
                registration_socket: scratch.join("adapter.sock"),
                core_socket: scratch.join("core.sock"),
                credential_file: scratch.join("token"),
                native_stores: Vec::new(),
            };
            std::fs::write(&config.credential_file, "synthetic-token").unwrap();
            std::fs::set_permissions(
                &config.credential_file,
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            let database = scratch.join("state.sqlite");
            let store = Arc::new(
                Store::open_with_backup_root(
                    &database,
                    &scratch.join("backups"),
                    crate::app::config::EffectiveConfig::defaults().store(),
                )
                .unwrap(),
            );
            let delivery = BridgeDelivery::open(&config, Arc::clone(&store), [7; 32])
                .await
                .unwrap();
            let binding = BindingRef::fixture("actor", "channel", 1);
            let route = RouteTicket {
                route_key: "fixture-route".into(),
                actor: "actor".into(),
                call: "call".into(),
            };
            delivery.note_route(&binding, route.clone());
            let listener = UnixListener::bind(&config.core_socket).unwrap();
            let thread =
                std::thread::spawn(move || {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let mut frame = String::new();
                    BufReader::new(&stream).read_line(&mut frame).unwrap();
                    let request = wire::decode_request(frame.trim_end().as_bytes()).unwrap();
                    let wire::Operation::Publish(params) = &request.operation else {
                        panic!("publication");
                    };
                    assert_eq!(request.request_id, params.event_id);
                    assert_eq!(
                        params.payload.as_str(),
                        "test-finished tests #1: AUTHORIZED_DIAGNOSTIC\nAUTHORIZED_SOURCE"
                    );
                    assert_eq!(params.mode, wire::Mode::Auto);
                    if class == wire::ErrorClass::Capacity {
                        // A response slower than the retired 1.5-second budget must still be read
                        // as the service's definite no-effect refusal, never misclassified as an
                        // unknown transport outcome.
                        std::thread::sleep(Duration::from_millis(1700));
                    }
                    let connection = rusqlite::Connection::open_with_flags(
                        &database,
                        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                    )
                    .unwrap();
                    let state: String = connection
                        .query_row(
                            "SELECT state FROM ide_bridge_delivery WHERE event_id=?1",
                            [&params.event_id],
                            |row| row.get(0),
                        )
                        .unwrap();
                    assert_eq!(
                        state, "intent",
                        "durable intent must exist before first socket write"
                    );
                    let stored: (String, String) = connection.query_row(
                    "SELECT payload,destination FROM ide_bridge_intention WHERE event_id=?1",
                    [&params.event_id], |row| Ok((row.get(0)?, row.get(1)?)),
                ).unwrap();
                    assert_eq!(stored.0, params.payload.as_str());
                    assert_eq!(
                        stored.1,
                        serde_json::to_string(&params.destination).unwrap()
                    );
                    let response = wire::Response::error(
                        request.request_id,
                        wire::ApiError {
                            class,
                            reason: "fixture".into(),
                            retry_hint: wire::RetryHint::Never,
                            effect: Some(effect),
                            reconciliation: Some(wire::ReconciliationKey::Event {
                                event_id: params.event_id.clone(),
                            }),
                            detail: "fixture".into(),
                        },
                    );
                    stream
                        .write_all(&wire::encode_response(&response).unwrap())
                        .unwrap();
                    stream.write_all(b"\n").unwrap();
                });
            let draft = FactDraft {
                kind: FactKind::TestFinished,
                subject: "tests #1".into(),
                summary: "AUTHORIZED_DIAGNOSTIC".into(),
                detail: Some("AUTHORIZED_SOURCE".into()),
                detail_ref: Some("retained-detail".into()),
                failed: false,
                source_bearing: false,
                fence_paths: vec![],
            };
            assert_eq!(
                delivery
                    .publish(
                        binding.clone(),
                        &draft,
                        DeliveryTarget::Address("local/claude-code/fixture/actor".into())
                    )
                    .await,
                transferred
            );
            thread.join().unwrap();
            // Same completed fact never writes another socket or invents an event identity.
            assert!(
                delivery
                    .publish(
                        binding,
                        &draft,
                        DeliveryTarget::Address("local/claude-code/fixture/actor".into())
                    )
                    .await
            );
            drop(delivery);
            drop(store);
        }
    }

    /// Definite no-send alone permits fallback; possible effects and conflicts never do.
    #[test]
    fn fallback_requires_positive_no_effect() {
        use bridge_sdk::{ClientError, TransportOutcome};
        for (outcome, expected) in [
            (TransportOutcome::NotSent, true),
            (TransportOutcome::OutcomeUnknown, false),
            (TransportOutcome::ReadFailed, false),
        ] {
            let error = ClientError::Transport {
                outcome,
                reconciliation: None,
                reason: "fixture",
            };
            assert_eq!(definitely_unsent(&error), expected);
            assert_eq!(refusal_detail(&error), "fixture");
        }
        for (class, effect, expected) in [
            (
                wire::ErrorClass::Capacity,
                wire::MutationEffect::NotCommitted,
                true,
            ),
            (
                wire::ErrorClass::Conflict,
                wire::MutationEffect::NotCommitted,
                false,
            ),
            (
                wire::ErrorClass::OutcomeUnknown,
                wire::MutationEffect::NotCommitted,
                false,
            ),
            (
                wire::ErrorClass::Unavailable,
                wire::MutationEffect::Committed,
                false,
            ),
            (
                wire::ErrorClass::Unavailable,
                wire::MutationEffect::Unknown,
                false,
            ),
        ] {
            let error = ClientError::Service(wire::ApiError {
                class,
                reason: "fixture".into(),
                retry_hint: wire::RetryHint::Never,
                effect: Some(effect),
                reconciliation: None,
                detail: "fixture".into(),
            });
            assert_eq!(definitely_unsent(&error), expected);
            assert_eq!(
                refusal_detail(&error),
                "bridge:publish_refused:service_not_committed"
            );
        }
    }

    /// Same-call delivery identity cannot cross actor, native call, or host boundaries.
    #[test]
    fn route_ticket_must_match_local_admission() {
        use super::super::host_binding::{
            BindingStatus, HostBindingGuard, parse_candidate, parse_channel_session,
        };
        let mut guard = HostBindingGuard::default();
        let metadata = serde_json::json!({"threadId":"actor-a", "callId":"call-a", "x-codex-turn-metadata":{}});
        let candidate = parse_candidate(metadata.as_object().unwrap()).unwrap();
        let BindingStatus::Validated(invocation) = guard.establish_managed_codex_start(
            candidate,
            parse_channel_session(b"private-channel").unwrap(),
        ) else {
            panic!("valid fixture");
        };
        let ticket =
            serde_json::json!({"route_key":"route-private", "actor":"actor-a", "call":"call-a"});
        assert!(RouteTicket::from_private(&ticket, invocation.binding_ref(), "call-a").is_some());
        assert!(RouteTicket::from_private(&ticket, invocation.binding_ref(), "call-b").is_none());
        let wrong_actor =
            serde_json::json!({"route_key":"route-private", "actor":"actor-b", "call":"call-a"});
        assert!(
            RouteTicket::from_private(&wrong_actor, invocation.binding_ref(), "call-a").is_none()
        );
        assert!(
            RouteTicket::from_private(
                &ticket,
                &BindingRef::fixture("actor-a", "private-channel", 1),
                "call-a"
            )
            .is_none()
        );
    }

    /// Exact addresses come from configured native stores and independently established actors;
    /// failed/source-bearing results use the same reservation without caller registration.
    #[tokio::test]
    async fn direct_targets_preserve_host_actor_and_useful_failure_content() {
        use super::super::host_binding::{
            BindingStatus, HostBindingGuard, parse_candidate, parse_channel_session,
        };
        use std::os::unix::fs::PermissionsExt;
        let scratch = crate::scratch::ScratchDir::new("bridge-direct-target");
        let config = BridgeConfig {
            registration_socket: scratch.join("adapter.sock"),
            core_socket: scratch.join("core.sock"),
            credential_file: scratch.join("token"),
            native_stores: vec![
                NativeStore {
                    harness: wire::Host::Codex,
                    host: "local".into(),
                    store: "codex-owned".into(),
                },
                NativeStore {
                    harness: wire::Host::ClaudeCode,
                    host: "local".into(),
                    store: "claude-owned".into(),
                },
            ],
        };
        std::fs::write(&config.credential_file, "synthetic-token").unwrap();
        std::fs::set_permissions(
            &config.credential_file,
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let store = Arc::new(
            Store::open_with_backup_root(
                &scratch.join("state.sqlite"),
                &scratch.join("backups"),
                crate::app::config::EffectiveConfig::defaults().store(),
            )
            .unwrap(),
        );
        let delivery = BridgeDelivery::open(&config, store, [9; 32]).await.unwrap();
        let mut guard = HostBindingGuard::default();
        let metadata = serde_json::json!({"threadId":"codex-actor", "callId":"call", "x-codex-turn-metadata":{}});
        let BindingStatus::Validated(invocation) = guard.establish_managed_codex_start(
            parse_candidate(metadata.as_object().unwrap()).unwrap(),
            parse_channel_session(b"owned-channel").unwrap(),
        ) else {
            panic!("fixture correlation");
        };
        let draft = FactDraft {
            kind: FactKind::TestFinished,
            subject: "tests #1".into(),
            summary: "1 failed".into(),
            detail: Some("authorized failure details".into()),
            detail_ref: None,
            failed: true,
            source_bearing: true,
            fence_paths: Vec::new(),
        };
        for (binding, expected) in [
            (
                invocation.binding_ref().clone(),
                "local/codex/codex-owned/codex-actor",
            ),
            (
                BindingRef::fixture("claude-actor", "owned-channel", 1),
                "local/claude-code/claude-owned/claude-actor",
            ),
        ] {
            let (target, permit) = delivery.reserve(&binding, &draft).unwrap();
            assert_eq!(
                target.destination(),
                wire::Destination::Address {
                    address: expected.into()
                }
            );
            drop(permit);
        }
        let mut invalid = config.clone();
        invalid.native_stores.push(config.native_stores[0].clone());
        assert!(
            !invalid.validate(),
            "duplicate/excess host prefixes must refuse"
        );
    }

    /// Historical route deployments retain their closed notice and existing failure/source
    /// carriers; enabling direct delivery must not silently broaden an old route grant.
    #[tokio::test]
    async fn historical_routes_refuse_failed_source_and_graph_facts() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = crate::scratch::ScratchDir::new("bridge-legacy-isolation");
        let config = BridgeConfig {
            registration_socket: scratch.join("adapter.sock"),
            core_socket: scratch.join("core.sock"),
            credential_file: scratch.join("token"),
            native_stores: vec![],
        };
        std::fs::write(&config.credential_file, "fixture-token").unwrap();
        std::fs::set_permissions(
            &config.credential_file,
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let store = Arc::new(
            Store::open_with_backup_root(
                &scratch.join("state.sqlite"),
                &scratch.join("backups"),
                crate::app::config::EffectiveConfig::defaults().store(),
            )
            .unwrap(),
        );
        let delivery = BridgeDelivery::open(&config, store, [5; 32]).await.unwrap();
        let binding = BindingRef::fixture("actor", "channel", 1);
        delivery.note_route(
            &binding,
            RouteTicket {
                route_key: "route".into(),
                actor: "actor".into(),
                call: "call".into(),
            },
        );
        let fact = FactDraft {
            kind: FactKind::TestFinished,
            subject: "tests #1".into(),
            summary: "done".into(),
            detail: None,
            detail_ref: None,
            failed: false,
            source_bearing: false,
            fence_paths: vec![],
        };
        assert!(delivery.reserve(&binding, &fact).is_some());
        let mut changed = fact.clone();
        changed.failed = true;
        assert!(delivery.reserve(&binding, &changed).is_none());
        let mut changed = fact.clone();
        changed.source_bearing = true;
        assert!(delivery.reserve(&binding, &changed).is_none());
        let mut changed = fact;
        changed.kind = FactKind::JobReady;
        assert!(delivery.reserve(&binding, &changed).is_none());
    }

    /// Credential adoption rejects symlinks, public permissions, directories and oversize files.
    #[test]
    fn credential_file_is_bounded_private_and_no_follow() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let scratch = crate::scratch::ScratchDir::new("bridge-config");
        let credential = scratch.join("token");
        std::fs::write(&credential, "synthetic-test-token\n").unwrap();
        std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut config = BridgeConfig {
            registration_socket: scratch.join("adapter.sock"),
            core_socket: scratch.join("core.sock"),
            credential_file: credential.clone(),
            native_stores: Vec::new(),
        };
        assert!(config.open().is_some());
        std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(config.open().is_none());
        std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = scratch.join("link");
        symlink(&credential, &link).unwrap();
        config.credential_file = link;
        assert!(config.open().is_none());
        config.credential_file = scratch.to_path_buf();
        assert!(config.open().is_none());
        config.credential_file = credential.clone();
        std::fs::write(credential, "x".repeat(16 * 1024 + 1)).unwrap();
        assert!(config.open().is_none());
    }

    /// Routing ids distinguish actors, calls and framing; Debug never reveals route data.
    #[test]
    fn identities_and_route_debug_are_separate() {
        assert_ne!(
            identity("registration", &["ab", "c"]),
            identity("registration", &["a", "bc"])
        );
        assert_ne!(
            identity("registration", &["a", "1"]),
            identity("registration", &["b", "1"])
        );
        let ticket = RouteTicket {
            route_key: "private-route".into(),
            actor: "private-actor".into(),
            call: "private-call".into(),
        };
        assert_eq!(format!("{ticket:?}"), "RouteTicket(<redacted>)");
    }

    /// Trusted deployment rejects relative or parent-containing paths before credential I/O.
    #[test]
    fn config_paths_are_explicit_and_normalized() {
        let mut config = BridgeConfig {
            registration_socket: "/service/native.sock".into(),
            core_socket: "/service/core.sock".into(),
            credential_file: "/service/token".into(),
            native_stores: Vec::new(),
        };
        assert!(config.validate());
        config.core_socket = "/service/../core.sock".into();
        assert!(!config.validate());
        config.core_socket = "core.sock".into();
        assert!(!config.validate());
    }
}
