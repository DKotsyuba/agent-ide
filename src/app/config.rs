//! Immutable, restart-only Application configuration used by the daemon and SQLite substrate.

use std::fmt::{self, Display};
use std::num::NonZeroU64;
use std::time::Duration;

/// Names the bounded Application setting whose effective origin may be inspected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigKey {
    /// Total read/write time allowed for one private IPC connection.
    IpcConnectionDeadline,
    /// Maximum number of concurrently served private IPC connections.
    IpcMaxConnections,
    /// Maximum domain SQL submissions waiting for the dedicated SQLite owner thread.
    StoreQueueCapacity,
    /// Maximum time SQLite waits on a database lock before returning busy.
    StoreBusyTimeout,
    /// Maximum caller wait after a store submission is accepted.
    StoreRequestDeadline,
    /// Maximum retained operation receipts before Application refuses further admission.
    StoreReceiptCapacity,
}

/// Identifies the layer that supplied an effective setting value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConfigOrigin {
    /// Built-in conservative defaults compiled into this binary.
    Defaults,
    /// A host ceiling that cannot be expanded by lower-authority local settings.
    Host,
    /// A local-user request subject to any host ceiling.
    User,
    /// A project-local request subject to any host and user ceiling.
    Project,
    /// A single-session request subject to all previous ceilings.
    Session,
}

/// Contains optional ceilings from one named configuration layer.
#[derive(Debug, Clone, Default)]
pub struct AppConfigPatch {
    /// Optional upper bound for a complete private IPC connection.
    pub ipc_connection_deadline: Option<Duration>,
    /// Optional upper bound for simultaneously served private IPC connections.
    pub ipc_max_connections: Option<usize>,
    /// Optional upper bound for domain SQL closures waiting for execution.
    pub store_queue_capacity: Option<usize>,
    /// Optional upper bound for SQLite lock waits.
    pub store_busy_timeout: Option<Duration>,
    /// Optional upper bound for a caller waiting on accepted SQL work.
    pub store_request_deadline: Option<Duration>,
    /// Optional upper bound for Application mechanics receipts retained in SQLite.
    pub store_receipt_capacity: Option<usize>,
}

/// Couples one patch to its authority layer for deterministic merge and provenance.
#[derive(Debug, Clone)]
pub struct ConfigLayer {
    /// The named source that supplied this patch.
    pub origin: ConfigOrigin,
    /// The explicitly set ceiling values from that source.
    pub values: AppConfigPatch,
}

/// Captures immutable private-IPC settings consumed when a daemon starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpcConfig {
    /// Total read/write duration for one accepted Unix connection.
    pub connection_deadline: Duration,
    /// Concurrent accepted connection cap; excess peers are dropped without dispatch.
    pub max_connections: usize,
}

/// Captures immutable SQLite owner-thread settings consumed when a store opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreConfig {
    /// Bounded count of queued domain SQL closures awaiting the owner thread.
    pub queue_capacity: usize,
    /// SQLite busy duration before an incompatible competing lock reports busy.
    pub busy_timeout: Duration,
    /// Total caller wait after accepted work, after which reconciliation is required.
    pub request_deadline: Duration,
    /// Bounded count of receipts retained for operation reconciliation before new admission is refused.
    pub receipt_capacity: usize,
}

/// Holds the source of each effective setting without retaining raw layer input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigProvenance {
    ipc_connection_deadline: ConfigOrigin,
    ipc_max_connections: ConfigOrigin,
    store_queue_capacity: ConfigOrigin,
    store_busy_timeout: ConfigOrigin,
    store_request_deadline: ConfigOrigin,
    store_receipt_capacity: ConfigOrigin,
}

impl ConfigProvenance {
    /// Returns the layer that supplied the final effective value for `key`.
    pub fn origin_for(&self, key: ConfigKey) -> ConfigOrigin {
        match key {
            ConfigKey::IpcConnectionDeadline => self.ipc_connection_deadline,
            ConfigKey::IpcMaxConnections => self.ipc_max_connections,
            ConfigKey::StoreQueueCapacity => self.store_queue_capacity,
            ConfigKey::StoreBusyTimeout => self.store_busy_timeout,
            ConfigKey::StoreRequestDeadline => self.store_request_deadline,
            ConfigKey::StoreReceiptCapacity => self.store_receipt_capacity,
        }
    }
}

/// Represents one fully validated, immutable restart-only Application configuration generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveConfig {
    generation: NonZeroU64,
    ipc: IpcConfig,
    store: StoreConfig,
    provenance: ConfigProvenance,
}

impl EffectiveConfig {
    /// Creates generation one from built-in values for the first production daemon slice.
    ///
    /// This convenience uses no configuration file or reload mechanism. A caller that has named
    /// host/user/project/session input must instead call [`effective_config`] with a new generation
    /// and restart the affected daemon or store itself.
    pub fn defaults() -> Self {
        effective_config(NonZeroU64::new(1).expect("one is nonzero"), &[])
            .expect("built-in Application configuration is valid")
    }

    /// Returns this immutable configuration's generation, which changes only on explicit rebuild.
    pub fn generation(&self) -> NonZeroU64 {
        self.generation
    }

    /// Returns the private IPC limits consumed by daemon startup.
    pub fn ipc(&self) -> IpcConfig {
        self.ipc
    }

    /// Returns the SQLite owner-thread limits consumed by store startup.
    pub fn store(&self) -> StoreConfig {
        self.store
    }

    /// Returns the origin recorded for each value in this immutable generation.
    pub fn provenance(&self) -> ConfigProvenance {
        self.provenance
    }
}

/// Explains why a named configuration generation could not be constructed safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// Layers are not strictly ordered from host through session and cannot have deterministic authority.
    InvalidLayerOrder,
    /// A ceiling is zero and would make the associated bounded operation impossible.
    ZeroLimit { key: ConfigKey },
}

impl Display for ConfigError {
    /// Formats the rejection without revealing untrusted configuration content.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLayerOrder => formatter.write_str("configuration layers are out of order"),
            Self::ZeroLimit { key } => write!(formatter, "configuration limit is zero: {key:?}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Builds one immutable restart-only generation from ordered ceiling layers and records value origins.
///
/// `layers` must be strictly ordered `Host`, `User`, `Project`, then `Session`; omitted layers are
/// allowed. Every currently supported setting is a named upper limit, so a lower layer can narrow
/// but cannot expand an earlier value. This is intentionally not a generic reload engine: callers
/// construct a new generation and restart consumers after accepting it.
pub fn effective_config(
    generation: NonZeroU64,
    layers: &[ConfigLayer],
) -> Result<EffectiveConfig, ConfigError> {
    let mut effective = EffectiveConfig {
        generation,
        ipc: IpcConfig {
            connection_deadline: Duration::from_secs(2),
            max_connections: 16,
        },
        store: StoreConfig {
            queue_capacity: 32,
            busy_timeout: Duration::from_secs(1),
            request_deadline: Duration::from_secs(2),
            receipt_capacity: 1_024,
        },
        provenance: ConfigProvenance {
            ipc_connection_deadline: ConfigOrigin::Defaults,
            ipc_max_connections: ConfigOrigin::Defaults,
            store_queue_capacity: ConfigOrigin::Defaults,
            store_busy_timeout: ConfigOrigin::Defaults,
            store_request_deadline: ConfigOrigin::Defaults,
            store_receipt_capacity: ConfigOrigin::Defaults,
        },
    };
    let mut previous = ConfigOrigin::Defaults;
    for layer in layers {
        if layer.origin == ConfigOrigin::Defaults || layer.origin <= previous {
            return Err(ConfigError::InvalidLayerOrder);
        }
        previous = layer.origin;
        merge_duration(
            &mut effective.ipc.connection_deadline,
            &mut effective.provenance.ipc_connection_deadline,
            layer.values.ipc_connection_deadline,
            layer.origin,
            ConfigKey::IpcConnectionDeadline,
        )?;
        merge_usize(
            &mut effective.ipc.max_connections,
            &mut effective.provenance.ipc_max_connections,
            layer.values.ipc_max_connections,
            layer.origin,
            ConfigKey::IpcMaxConnections,
        )?;
        merge_usize(
            &mut effective.store.queue_capacity,
            &mut effective.provenance.store_queue_capacity,
            layer.values.store_queue_capacity,
            layer.origin,
            ConfigKey::StoreQueueCapacity,
        )?;
        merge_duration(
            &mut effective.store.busy_timeout,
            &mut effective.provenance.store_busy_timeout,
            layer.values.store_busy_timeout,
            layer.origin,
            ConfigKey::StoreBusyTimeout,
        )?;
        merge_duration(
            &mut effective.store.request_deadline,
            &mut effective.provenance.store_request_deadline,
            layer.values.store_request_deadline,
            layer.origin,
            ConfigKey::StoreRequestDeadline,
        )?;
        merge_usize(
            &mut effective.store.receipt_capacity,
            &mut effective.provenance.store_receipt_capacity,
            layer.values.store_receipt_capacity,
            layer.origin,
            ConfigKey::StoreReceiptCapacity,
        )?;
    }
    Ok(effective)
}

/// Narrows one duration ceiling after rejecting an explicit zero-duration request.
fn merge_duration(
    current: &mut Duration,
    provenance: &mut ConfigOrigin,
    request: Option<Duration>,
    origin: ConfigOrigin,
    key: ConfigKey,
) -> Result<(), ConfigError> {
    let Some(request) = request else {
        return Ok(());
    };
    if request.is_zero() {
        return Err(ConfigError::ZeroLimit { key });
    }
    if request < *current {
        *current = request;
        *provenance = origin;
    }
    Ok(())
}

/// Narrows one count ceiling after rejecting an explicit zero-capacity request.
fn merge_usize(
    current: &mut usize,
    provenance: &mut ConfigOrigin,
    request: Option<usize>,
    origin: ConfigOrigin,
    key: ConfigKey,
) -> Result<(), ConfigError> {
    let Some(request) = request else {
        return Ok(());
    };
    if request == 0 {
        return Err(ConfigError::ZeroLimit { key });
    }
    if request < *current {
        *current = request;
        *provenance = origin;
    }
    Ok(())
}
