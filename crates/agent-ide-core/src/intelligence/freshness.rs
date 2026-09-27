//! Typed freshness and cache-lifecycle facts for a logical Intelligence view.

use std::collections::VecDeque;

use crate::{
    app::cache::{CacheNamespace, CacheNamespaceId, CacheRoot, VerifiedCacheRetirement},
    workspace::{
        authority::WorktreeRef,
        durable::VerifiedWorktreeClosure,
        observation::{SourceCoverage, SourceObservation},
    },
};

/// Limits opaque provider and cache identity values before they enter a retained lifecycle record.
const MAX_ID_BYTES: usize = 128;

/// Classifies whether returned semantic evidence is usable for the current logical view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Freshness {
    /// The result matches all current source and generation fences with complete coverage.
    Current,
    /// A later source, configuration, toolchain, backend, or view generation superseded it.
    Stale,
    /// A pushed diagnostic lacks the provider-specific pull/barrier proof required for `Current`.
    Provisional,
    /// Coverage or diagnostic readiness cannot establish a current result.
    Unknown,
}

/// States whether a diagnostic collection can claim that a document is clean.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticReadiness {
    /// A matching, versioned provider diagnostic result explicitly reported an empty set.
    Clean,
    /// A matching versioned result, or a bound nonempty unversioned one-shot push, reported items.
    Reported,
    /// No correlated diagnostic result exists; silence and empty unversioned pushes are not clean.
    Unknown,
}

/// Captures the five monotonic generations that fence an Intelligence result.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ViewGeneration {
    /// Provider backend incarnation, advanced when a backend is replaced or invalidated.
    pub backend: u64,
    /// Provider configuration generation.
    pub configuration: u64,
    /// Provider toolchain generation.
    pub toolchain: u64,
    /// Logical-view generation, advanced when a view is reattached or drained.
    pub view: u64,
}

/// Binds a provider document version to one exact Workspace observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceBinding {
    /// Canonical Workspace worktree identity that scopes the provider document.
    worktree_id: String,
    /// Workspace lifecycle incarnation that prevents reuse after recreation at one path.
    incarnation: u64,
    /// Workspace authority generation that fenced collection.
    authority_epoch: u64,
    /// Monotonic durable source sequence for ordering source changes.
    sequence: u64,
    /// Workspace observation correlation reference.
    observation_ref: String,
    /// Opaque source revision distinct from Git revisions.
    source_revision: String,
    /// Exact present-byte digest, absent only for an explicit missing-path observation.
    digest: Option<[u8; 32]>,
    /// Explicit observation completeness, which blocks current claims when incomplete.
    coverage: SourceCoverage,
}

impl SourceBinding {
    /// Captures every source fact whose change makes an older provider result stale or unknown.
    pub fn from_observation(observation: &SourceObservation) -> Self {
        Self {
            worktree_id: observation.worktree().id().to_owned(),
            incarnation: observation.worktree().incarnation(),
            authority_epoch: observation.authority_epoch(),
            sequence: observation.sequence(),
            observation_ref: observation.reference().as_str().to_owned(),
            source_revision: observation.source_revision().as_str().to_owned(),
            digest: observation.bytes().map(|bytes| *bytes.digest()),
            coverage: observation.coverage(),
        }
    }

    /// Returns the observation sequence used to order source changes for this view.
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns whether this binding has complete source coverage.
    pub const fn coverage(&self) -> SourceCoverage {
        self.coverage
    }
}

/// Identifies a provider reply or pushed diagnostic without carrying its unbounded content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticReference(String);

impl DiagnosticReference {
    /// Validates a bounded opaque provider result reference.
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (!value.is_empty() && value.len() <= MAX_ID_BYTES).then_some(Self(value))
    }

    /// Returns the opaque reference for correlation only.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Keeps a bounded newest-first delta of diagnostic result references.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticDeltas {
    /// Maximum retained references, excluding discarded historical entries.
    capacity: usize,
    /// Oldest-to-newest bounded provider result references.
    references: VecDeque<DiagnosticReference>,
    /// Whether one or more oldest references were discarded due to the ceiling.
    overflowed: bool,
}

impl DiagnosticDeltas {
    /// Creates an empty delta log with a positive finite reference ceiling.
    pub fn new(capacity: usize) -> Option<Self> {
        (capacity != 0).then_some(Self {
            capacity,
            references: VecDeque::new(),
            overflowed: false,
        })
    }

    /// Adds a result reference, discarding only the oldest reference once the bounded ceiling is full.
    pub fn push(&mut self, reference: DiagnosticReference) {
        if self.references.len() == self.capacity {
            self.references.pop_front();
            self.overflowed = true;
        }
        self.references.push_back(reference);
    }

    /// Returns retained references in arrival order.
    pub fn references(&self) -> impl ExactSizeIterator<Item = &DiagnosticReference> {
        self.references.iter()
    }

    /// Returns whether at least one older delta reference was discarded.
    pub const fn overflowed(&self) -> bool {
        self.overflowed
    }
}

/// Tracks one logical view's current source and generation fences without controlling any process.
#[derive(Clone, Debug)]
pub struct ViewFreshness {
    /// Current backend/configuration/toolchain/view fences.
    generation: ViewGeneration,
    /// Current source facts that provider results must reproduce exactly.
    source: SourceBinding,
    /// Provider document version paired with the current source binding.
    document_version: u64,
    /// Latest diagnostic readiness without converting lack of data into cleanliness.
    diagnostics: DiagnosticReadiness,
    /// Bounded diagnostic result history for consumers that need deltas.
    deltas: DiagnosticDeltas,
    /// Whether Workspace may still accept new result work for this logical view.
    active: bool,
}

impl ViewFreshness {
    /// Starts an active view from a complete or incomplete Workspace observation and a bounded delta ceiling.
    pub fn new(
        source: SourceBinding,
        generation: ViewGeneration,
        document_version: u64,
        delta_capacity: usize,
    ) -> Option<Self> {
        if document_version == 0 {
            return None;
        }
        Some(Self {
            generation,
            source,
            document_version,
            diagnostics: DiagnosticReadiness::Unknown,
            deltas: DiagnosticDeltas::new(delta_capacity)?,
            active: true,
        })
    }

    /// Replaces the current source binding after Workspace reports a newer observation.
    pub fn update_source(&mut self, source: SourceBinding, document_version: u64) -> bool {
        if document_version == 0 || source.sequence < self.source.sequence {
            return false;
        }
        self.source = source;
        self.document_version = document_version;
        self.diagnostics = DiagnosticReadiness::Unknown;
        true
    }

    /// Replaces current generation fences after a backend, configuration, toolchain, or view transition.
    pub fn update_generation(&mut self, generation: ViewGeneration) {
        self.generation = generation;
        self.diagnostics = DiagnosticReadiness::Unknown;
    }

    /// Fences the logical view after Workspace revocation, stop, or handoff; no peer process is touched.
    pub fn quiesce(&mut self) {
        self.active = false;
        self.diagnostics = DiagnosticReadiness::Unknown;
    }

    /// Evaluates a result against every captured fence and its diagnostic delivery proof.
    pub fn evaluate(
        &self,
        source: &SourceBinding,
        generation: ViewGeneration,
        document_version: u64,
        pushed: bool,
    ) -> Freshness {
        if !self.active
            || source != &self.source
            || generation != self.generation
            || document_version != self.document_version
        {
            return Freshness::Stale;
        }
        if !source.coverage().is_complete() {
            return Freshness::Unknown;
        }
        if pushed {
            Freshness::Provisional
        } else {
            Freshness::Current
        }
    }

    /// Records a diagnostics result only when it is current and pulled; empty data otherwise remains unknown.
    pub fn record_diagnostics(
        &mut self,
        source: &SourceBinding,
        generation: ViewGeneration,
        document_version: u64,
        pushed: bool,
        reference: DiagnosticReference,
        has_diagnostics: bool,
    ) -> Freshness {
        let freshness = self.evaluate(source, generation, document_version, pushed);
        self.deltas.push(reference);
        self.diagnostics = match freshness {
            Freshness::Current => {
                if has_diagnostics {
                    DiagnosticReadiness::Reported
                } else {
                    DiagnosticReadiness::Clean
                }
            }
            _ => DiagnosticReadiness::Unknown,
        };
        freshness
    }

    /// Returns the current diagnostic readiness; unknown readiness never implies clean.
    pub const fn diagnostic_readiness(&self) -> DiagnosticReadiness {
        self.diagnostics
    }

    /// Returns the bounded retained diagnostic-result references.
    pub fn diagnostic_deltas(&self) -> &DiagnosticDeltas {
        &self.deltas
    }
}

/// Captures only provider-compatible cache inputs, deliberately excluding actor, session, binding, and authority IDs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheIdentity {
    /// Provider binary/protocol identity.
    provider: String,
    /// Declared provider profile and sharing mode identity.
    profile: String,
    /// Effective provider configuration identity.
    configuration: String,
    /// Exact provider toolchain identity.
    toolchain: String,
    /// Effective trust-boundary identity.
    trust: String,
    /// Provider-supported worktree state identity.
    worktree_state: String,
}

impl CacheIdentity {
    /// Validates compatible provider and worktree-state inputs for one reusable native-cache namespace.
    pub fn new(
        provider: impl Into<String>,
        profile: impl Into<String>,
        configuration: impl Into<String>,
        toolchain: impl Into<String>,
        trust: impl Into<String>,
        worktree_state: impl Into<String>,
    ) -> Option<Self> {
        let identity = Self {
            provider: provider.into(),
            profile: profile.into(),
            configuration: configuration.into(),
            toolchain: toolchain.into(),
            trust: trust.into(),
            worktree_state: worktree_state.into(),
        };
        [
            &identity.provider,
            &identity.profile,
            &identity.configuration,
            &identity.toolchain,
            &identity.trust,
            &identity.worktree_state,
        ]
        .iter()
        .all(|value| !value.is_empty() && value.len() <= MAX_ID_BYTES)
        .then_some(identity)
    }

    /// Returns whether a stopped coder view may hand its namespace to a quiescent compatible reviewer view.
    pub fn compatible_with(&self, other: &Self) -> bool {
        self == other
    }
}

/// Owns a retained cache namespace whose retirement requires a verified Workspace lifecycle fact.
#[derive(Debug)]
pub struct CacheLifecycle {
    /// Compatibility inputs that deliberately omit user/session/authority values.
    identity: CacheIdentity,
    /// Canonical nonce-bound worktree identity that owns this namespace. The incarnation alone is
    /// Store-local and repeats across databases, so the exact `WorktreeRef::id()` is recorded
    /// beside it and both must match a verified closure before anything is deleted.
    worktree: String,
    /// Canonical worktree incarnation that owns this namespace; only its exact verified closure
    /// may retire it, so a closure of any other durable lifecycle generation is refused.
    incarnation: u64,
    /// Retained private namespace until one verified retirement fact completes successfully.
    namespace: Option<CacheNamespace>,
    /// Whether the outgoing view has stopped using this namespace.
    quiescent: bool,
    /// Whether a failed verified retirement blocked reuse until a later successful verified retry.
    retirement_failed: bool,
}

impl CacheLifecycle {
    /// Retains an opaque namespace for one compatible cache identity; this does not create a provider process.
    ///
    /// `worktree` is the canonical durable worktree that owns the namespace; both its nonce-bound
    /// identity and its incarnation are recorded so retirement can demand that exact worktree's
    /// verified closure. Returns the Application error when the private directory cannot be created
    /// or validated.
    pub fn retain(
        root: &CacheRoot,
        namespace: CacheNamespaceId,
        identity: CacheIdentity,
        worktree: &WorktreeRef,
    ) -> Result<Self, crate::app::AppError> {
        Ok(Self {
            identity,
            worktree: worktree.id().to_owned(),
            incarnation: worktree.incarnation(),
            namespace: Some(root.retain(namespace)?),
            quiescent: false,
            retirement_failed: false,
        })
    }

    /// Returns the retained namespace's private directory, or `None` once retirement completed.
    ///
    /// This is the only source of a provider's effective cache path: reading it from the retained
    /// lifecycle keeps retention bookkeeping and the directory a provider actually writes to from
    /// diverging.
    pub fn namespace_path(&self) -> Option<&std::path::Path> {
        self.namespace.as_ref().map(CacheNamespace::path)
    }

    /// Returns whether no view currently uses this namespace, so a handoff or retirement may proceed.
    pub const fn quiescent(&self) -> bool {
        self.quiescent
    }

    /// Marks a released, stopped, handed-off, or temporarily missing view as quiescent while retaining its namespace.
    pub fn quiesce(&mut self) {
        self.quiescent = true;
    }

    /// Reuses this namespace only for a compatible incoming identity after the old view quiesced.
    pub fn handoff(&mut self, incoming: &CacheIdentity) -> bool {
        self.handoff_allowed(incoming)
    }

    /// Returns whether a handoff to `incoming` would be accepted, without claiming one happened.
    ///
    /// Retention bookkeeping is decided before any namespace is created, so the caller needs this
    /// answer while it still only holds a shared borrow of the lifecycle map.
    pub fn handoff_allowed(&self, incoming: &CacheIdentity) -> bool {
        self.quiescent
            && self.namespace.is_some()
            && !self.retirement_failed
            && self.identity.compatible_with(incoming)
    }

    /// Retires the namespace only for the exact canonical worktree incarnation Workspace closed.
    ///
    /// `closure` is Workspace's private-field closure receipt; it cannot be forged, and both its
    /// nonce-bound worktree identity and its incarnation must equal the ones recorded at `retain`,
    /// so a closure of a different worktree, of the same incarnation number in a different Store,
    /// or of a reopened later incarnation deletes nothing. Refuses while any view still uses this
    /// lifecycle: retirement must follow admission revocation and provider quiescence, never race
    /// an active owner. There is no reset spelling; an unsupported reset path stays unavailable
    /// rather than accepting a caller-chosen reason. A filesystem failure retains the namespace,
    /// blocks handoff, and returns the Application error so the caller may retry the same verified
    /// closure after correcting local conditions. Retiring an already-retired lifecycle succeeds.
    pub fn retire(
        &mut self,
        closure: &VerifiedWorktreeClosure,
    ) -> Result<(), crate::app::AppError> {
        if !self.quiescent {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "cannot retire a cache lifecycle while a view still uses it",
            )
            .into());
        }
        if closure.worktree() != self.worktree || closure.incarnation() != self.incarnation {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "verified closure does not match the canonical worktree owning this namespace",
            )
            .into());
        }
        let Some(namespace) = self.namespace.as_ref() else {
            return Ok(());
        };
        match namespace.retire(VerifiedCacheRetirement::verified()) {
            Ok(()) => {
                self.namespace = None;
                self.quiescent = true;
                Ok(())
            }
            Err(error) => {
                self.retirement_failed = true;
                Err(error)
            }
        }
    }

    /// Returns whether lifecycle ownership remains retained after non-retirement lifecycle events or a failed verified retry.
    pub const fn retained(&self) -> bool {
        self.namespace.is_some()
    }
}
