//! Claude foreground-helper correlation: ticket minting, exact launch recognition, one-use claim.
//!
//! Claude has no Codex sandbox metadata and no long-lived daemon-side execution path. A Claude
//! operation therefore runs inside a short foreground `Bash` helper that inherits the host's own
//! native sandbox. This module owns only the correlation and admission mechanics for that helper:
//!
//! * the daemon mints a [`LaunchTicket`] bound to one action-scoped `detail_ref` and returns the
//!   exact helper command in the ordinary bounded reply text;
//! * the ordinary `Bash` `PreToolUse` hook recognizes that command by comparing it against the
//!   daemon-stored expected bytes ([`LaunchLedger::recognize`]) and stays silent, so the host's
//!   own permission and sandbox evaluation of the unchanged command is what actually decides;
//! * the helper claims the bound operation exactly once over the private socket
//!   ([`LaunchLedger::claim`]) and receives one closed daemon-selected [`HelperJob`].
//!
//! Nothing here performs Git, source, provider or process effects, and nothing here is sandbox
//! attestation. A ticket establishes correlation and replay exclusion only; the authority contract
//! is the operator-managed strict Claude configuration declared by [`ClaudeOperatorProfile`].

use super::reply::FailureCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf};

/// Maximum complete serialized helper frame in either direction, before JSON decoding.
pub const MAX_HELPER_FRAME_BYTES: usize = 64 * 1024;
/// Only this closed helper wire revision is accepted in either direction.
pub const HELPER_PROTOCOL: u32 = 1;
/// Bounds concurrently outstanding launch tickets within one daemon boot.
pub const MAX_TICKETS: usize = 64;
/// Maximum accepted length of one exact expected helper command.
const MAX_COMMAND_BYTES: usize = 4096;
/// Maximum accepted length of any single identity field carried on the helper wire.
const MAX_IDENTIFIER_BYTES: usize = 256;
/// Maximum bounded owner text one helper result may carry back to the daemon.
const MAX_RESULT_TEXT_BYTES: usize = 32 * 1024;
/// Fixed helper subcommand; the model never selects an executable, argument or shell fragment.
const HELPER_SUBCOMMAND: &str = "claude-worker";

/// Declares the operator-managed strict Claude configuration this profile is accepted under.
///
/// The daemon never inspects, guesses or mutates live host settings. An operator states the
/// closed facts below in launcher configuration; an absent or mismatched profile leaves the
/// Claude execution path unavailable rather than silently degrading to unrestricted execution.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeOperatorProfile {
    /// Operator states the host sandbox is enabled for this account and project.
    pub enabled: bool,
    /// Operator states the host refuses to run a command when sandboxing is unavailable.
    pub fail_if_unavailable: bool,
    /// Operator states unsandboxed command escape is not permitted; `true` is never accepted.
    pub allow_unsandboxed_commands: bool,
    /// Operator states no `excludedCommands` entry matches the fixed helper command.
    pub no_matching_excluded_commands: bool,
    /// Operator states the read/write/network/socket scope matches the documented helper needs.
    pub scope_declared: bool,
    /// Host platform this evidence was accepted on; only macOS is currently supported.
    pub platform: HelperPlatform,
}

/// Names the host platforms for which an operator profile can currently be accepted.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperPlatform {
    /// macOS Seatbelt-backed Claude sandboxing, the only proven configuration.
    MacOs,
    /// Linux remains unavailable pending equivalent proof of the same guarantees.
    Linux,
}

impl ClaudeOperatorProfile {
    /// Accepts only a complete strict macOS profile; every other shape is unavailable.
    ///
    /// Failure is reported as [`FailureCode::ExecutionProfile`] so a missing or weakened operator
    /// declaration is never confused with a runtime provider or authority failure.
    pub fn validate(&self) -> Result<(), FailureCode> {
        let strict = self.enabled
            && self.fail_if_unavailable
            && !self.allow_unsandboxed_commands
            && self.no_matching_excluded_commands
            && self.scope_declared
            && self.platform == HelperPlatform::MacOs;
        strict.then_some(()).ok_or(FailureCode::ExecutionProfile)
    }
}

/// Names the closed operations a Claude helper may be launched for.
///
/// Inspect is deliberately absent: retrieval of an already retained result is same-binding daemon
/// bookkeeping and must never launch a helper or read source.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperOperation {
    /// Discovery plus canonical durable Workspace activation for one candidate.
    Start,
    /// Semantic context over currently registered source bytes.
    Context,
    /// Composed comparison evidence for the current Workspace scope.
    Diff,
    /// Revocation, provider reaping and child settlement.
    Stop,
}

/// Names the per-operation exclusive language profile a helper may run.
///
/// A single helper never runs both. Shared multi-worktree gopls is unavailable for Claude because
/// this foreground lifecycle cannot retain a safe shared listener across operations; the existing
/// Codex shared-listener implementation continues to cover that matrix cell unchanged.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperLanguage {
    /// gopls on a helper-private, non-shared logical view.
    Go,
    /// rust-analyzer with cache priming and proc-macro expansion both disabled.
    Rust,
}

/// Carries the effective Rust analyzer settings a Claude helper is permitted to run under.
///
/// Both switches are forced off. Disabling `procMacro` means semantics generated by procedural
/// macros — derive-generated methods, macro-expanded items and their references — are not visible
/// to the analyzer, so Rust results under this profile are honestly incomplete for such symbols.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RustEffectiveSettings {
    /// Always false; priming would outlive the foreground helper it was started for.
    pub cache_priming: bool,
    /// Always false; see the proc-macro semantics limitation documented on this type.
    pub proc_macro: bool,
}

impl RustEffectiveSettings {
    /// Rejects any settings pair that re-enables priming or proc-macro expansion.
    pub fn validate(&self) -> Result<(), FailureCode> {
        (!self.cache_priming && !self.proc_macro)
            .then_some(())
            .ok_or(FailureCode::ExecutionProfile)
    }
}

/// Names one daemon-selected provider a helper may spawn as its own direct child.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelperProvider {
    /// Absolute accepted analyzer executable chosen by the daemon, never by model input.
    pub executable: PathBuf,
    /// Exclusive language profile for this one operation.
    pub language: HelperLanguage,
    /// Effective Rust settings; present only for [`HelperLanguage::Rust`].
    pub rust_settings: Option<RustEffectiveSettings>,
    /// Persistent IDE-owned cache namespace retained across stop and handoff.
    pub cache_namespace: String,
}

impl HelperProvider {
    /// Checks absolute executable, language/settings agreement and a bounded cache namespace.
    pub fn validate(&self) -> Result<(), FailureCode> {
        if !self.executable.is_absolute()
            || self.cache_namespace.is_empty()
            || self.cache_namespace.len() > MAX_IDENTIFIER_BYTES
        {
            return Err(FailureCode::ExecutionProfile);
        }
        match (self.language, self.rust_settings.as_ref()) {
            (HelperLanguage::Rust, Some(settings)) => settings.validate(),
            (HelperLanguage::Go, None) => Ok(()),
            _ => Err(FailureCode::ExecutionProfile),
        }
    }
}

/// Bounds every finite resource one helper operation may consume.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelperBudgets {
    /// Maximum captured bytes per stream.
    pub output_bytes: usize,
    /// Maximum direct child processes the helper may spawn and must itself reap.
    pub processes: u32,
    /// Total helper lifetime, measured from claim.
    pub deadline_ms: u64,
}

impl HelperBudgets {
    /// Rejects zero or unbounded budgets so no helper can run without a finite ceiling.
    pub fn validate(&self) -> Result<(), FailureCode> {
        let bounded = (1..=1024 * 1024).contains(&self.output_bytes)
            && (1..=8).contains(&self.processes)
            && (1..=300_000).contains(&self.deadline_ms);
        bounded.then_some(()).ok_or(FailureCode::ExecutionProfile)
    }
}

/// One closed daemon-selected job handed to a helper that has successfully claimed its ticket.
///
/// Every executable, path, scope and budget in this frame is chosen by the daemon from trusted
/// launcher configuration. Model input contributes only the already validated method
/// [`HelperJob::parameters`]; it never selects an executable, shell fragment, scope or permission.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelperJob {
    /// Closed wire revision; only [`HELPER_PROTOCOL`] is accepted.
    pub protocol: u32,
    /// Closed operation this helper was launched for.
    pub operation: HelperOperation,
    /// Absolute candidate worktree; the helper still proves its native Git identity itself.
    pub candidate: PathBuf,
    /// Absolute accepted Git executable for the fixed discovery and snapshot commands.
    pub git: PathBuf,
    /// Canonical Workspace root when the daemon already knows it; absent before activation.
    pub canonical_root: Option<PathBuf>,
    /// Per-operation exclusive provider; absent when the operation needs no analyzer.
    pub provider: Option<HelperProvider>,
    /// Already validated closed method parameters carrying no target, profile or authority data.
    pub parameters: Value,
    /// Finite byte, process and deadline ceilings for this operation.
    pub budgets: HelperBudgets,
}

impl HelperJob {
    /// Validates the complete outbound frame before it is written to the helper socket.
    ///
    /// This is the daemon-side half of the two-boundary rule: a frame that cannot be validated is
    /// never sent, so a helper never has to trust a malformed job.
    pub fn validate(&self) -> Result<(), FailureCode> {
        if self.protocol != HELPER_PROTOCOL
            || !self.candidate.is_absolute()
            || !self.git.is_absolute()
            || self
                .canonical_root
                .as_ref()
                .is_some_and(|root| !root.is_absolute())
            || !self.parameters.is_object()
        {
            return Err(FailureCode::ExecutionProfile);
        }
        self.budgets.validate()?;
        match &self.provider {
            Some(provider) => provider.validate(),
            None => Ok(()),
        }
    }

    /// Encodes one bounded outbound frame, failing closed above [`MAX_HELPER_FRAME_BYTES`].
    pub fn encode(&self) -> Result<String, FailureCode> {
        self.validate()?;
        let frame = serde_json::to_string(self).map_err(|_| FailureCode::Internal)?;
        (frame.len() <= MAX_HELPER_FRAME_BYTES)
            .then_some(frame)
            .ok_or(FailureCode::Capacity)
    }

    /// Decodes one bounded inbound frame on the helper side, validating before use.
    pub fn decode(frame: &str) -> Result<Self, FailureCode> {
        if frame.len() > MAX_HELPER_FRAME_BYTES {
            return Err(FailureCode::Capacity);
        }
        let job: Self = serde_json::from_str(frame).map_err(|_| FailureCode::ExecutionProfile)?;
        job.validate()?;
        Ok(job)
    }
}

/// Reports how many direct children a helper spawned and how many it actually reaped.
///
/// The helper alone owns, cancels, drains and reaps its Git and provider children. The daemon
/// treats the helper endpoint as host-managed and borrowed: it never claims a direct-child reap
/// for the helper's PID and never kills a borrowed numeric PID.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChildSettlement {
    /// Direct children the helper started during this operation.
    pub spawned: u32,
    /// Direct children the helper observed exiting and reaped before closing.
    pub reaped: u32,
}

impl ChildSettlement {
    /// Returns whether every spawned direct child was actually reaped.
    ///
    /// A stop result may only be reported complete when this holds; otherwise the operation is
    /// uncertain and its admission stays quarantined.
    pub fn settled(&self) -> bool {
        self.spawned == self.reaped
    }
}

/// The bounded outcome one helper reports before closing its socket and exiting.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum HelperOutcome {
    /// The operation produced owner evidence rendered as bounded text.
    Complete {
        /// Bounded owner text containing no raw source, diagnostics or host configuration.
        text: String,
    },
    /// The operation reached a closed failure category without owner evidence.
    Failed {
        /// Stable actionable failure category.
        code: FailureCode,
    },
}

/// One bounded inbound helper frame carrying its result and real child-settlement evidence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelperResult {
    /// Closed wire revision; only [`HELPER_PROTOCOL`] is accepted.
    pub protocol: u32,
    /// The exact action-scoped handle this helper claimed.
    pub detail_ref: String,
    /// Bounded operation outcome.
    pub outcome: HelperOutcome,
    /// Measured direct-child settlement, never inferred from an exit status alone.
    pub children: ChildSettlement,
}

impl HelperResult {
    /// Validates the complete inbound frame before any daemon state is changed by it.
    pub fn validate(&self) -> Result<(), FailureCode> {
        if self.protocol != HELPER_PROTOCOL
            || self.detail_ref.is_empty()
            || self.detail_ref.len() > MAX_IDENTIFIER_BYTES
            || self.children.reaped > self.children.spawned
        {
            return Err(FailureCode::ExecutionProfile);
        }
        if let HelperOutcome::Complete { text } = &self.outcome
            && text.len() > MAX_RESULT_TEXT_BYTES
        {
            return Err(FailureCode::Capacity);
        }
        Ok(())
    }

    /// Encodes one bounded outbound frame on the helper side.
    pub fn encode(&self) -> Result<String, FailureCode> {
        self.validate()?;
        let frame = serde_json::to_string(self).map_err(|_| FailureCode::Internal)?;
        (frame.len() <= MAX_HELPER_FRAME_BYTES)
            .then_some(frame)
            .ok_or(FailureCode::Capacity)
    }

    /// Decodes one bounded inbound frame on the daemon side, validating before use.
    pub fn decode(frame: &str) -> Result<Self, FailureCode> {
        if frame.len() > MAX_HELPER_FRAME_BYTES {
            return Err(FailureCode::Capacity);
        }
        let result: Self =
            serde_json::from_str(frame).map_err(|_| FailureCode::ExecutionProfile)?;
        result.validate()?;
        Ok(result)
    }
}

/// Identifies the exact Claude actor a ticket is bound to.
///
/// `session_id` is Claude's root session and `agent_id` the distinct subagent identity when one is
/// present. Both come from the trusted native hook binding, never from tool arguments.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct HelperActor {
    /// Claude's required root session identity, present on parent and child events alike.
    session_id: String,
    /// Claude's distinct subagent identity, absent for a parent-session operation.
    agent_id: Option<String>,
}

impl HelperActor {
    /// Builds one bounded actor identity, rejecting empty or over-limit fields.
    pub fn new(session_id: &str, agent_id: Option<&str>) -> Result<Self, FailureCode> {
        let bounded = |value: &str| !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES;
        if !bounded(session_id) || agent_id.is_some_and(|value| !bounded(value)) {
            return Err(FailureCode::Conflict);
        }
        Ok(Self {
            session_id: session_id.to_owned(),
            agent_id: agent_id.map(str::to_owned),
        })
    }

    /// Returns Claude's root session identity.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Returns the distinct subagent identity when this actor is a child.
    pub fn agent_id(&self) -> Option<&str> {
        self.agent_id.as_deref()
    }
}

/// Tracks how far exactly one action-scoped helper handle has progressed.
///
/// The order is fixed: a ticket must be recognized as an actual native launch before it can be
/// claimed. A bare copied handle presented without its matching native pre-hook is still `Minted`
/// and is therefore rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
enum TicketState {
    /// Returned to the model; no native launch has been recognized yet.
    Minted,
    /// The exact expected command was recognized in an ordinary foreground `Bash` pre-hook.
    Launched {
        /// Native tool-call identity of the recognized `Bash` invocation.
        tool_use_id: String,
    },
    /// A helper claimed this handle exactly once; a second claim is rejected.
    Claimed {
        /// Native tool-call identity carried over from recognition.
        tool_use_id: String,
        /// Final helper frame, once it has arrived.
        frame: Option<HelperResult>,
        /// Whether the matching successful `Bash` post-hook has arrived.
        post: Option<bool>,
    },
    /// Claimed work whose settlement never became provable; its admission stays quarantined.
    Uncertain,
}

/// One action-scoped, single-use helper handle bound to exactly one Claude operation.
///
/// The handle value is the existing `Pending.detail_ref`. No additional public tool, reply state
/// or launch flag is introduced: the ticket is daemon-private bookkeeping hanging off that handle.
#[derive(Debug)]
pub struct LaunchTicket {
    /// Opaque binding-generation fingerprint this ticket is fenced to.
    binding: [u8; 32],
    /// Exact Claude actor permitted to claim this handle.
    actor: HelperActor,
    /// Opaque private transport channel this handle was minted on.
    channel: String,
    /// Exact expected helper command bytes; recognition compares against these, never a parser.
    command: String,
    /// Closed daemon-selected job released on a successful claim.
    job: HelperJob,
    /// Absolute expiry on the caller-supplied monotonic clock.
    deadline_ms: u64,
    /// Current single-use progress.
    state: TicketState,
}

impl LaunchTicket {
    /// Returns the exact command the model must run before inspecting this handle.
    pub fn command(&self) -> &str {
        &self.command
    }

    /// Returns whether this ticket has been claimed and is awaiting or holding settlement.
    pub fn claimed(&self) -> bool {
        matches!(self.state, TicketState::Claimed { .. })
    }
}

/// Reports what an ordinary `Bash` pre-hook observation meant for helper correlation.
///
/// Both variants are silent at the hook boundary. The hook never returns a permission decision,
/// never rewrites the command and never attaches updated input, so the host's own permission and
/// sandbox evaluation of the unchanged command is what actually authorizes the launch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchRecognition {
    /// The payload was not a byte-exact foreground helper command for a live ticket.
    ///
    /// All such tool payloads are discarded; nothing is retained about them.
    Ignored,
    /// The exact expected bytes were recognized and the ticket became claimable.
    Recognized,
}

/// Reports the closed outcome of one helper claim attempt over the private socket.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClaimOutcome {
    /// The claim was atomic, first, in time and correctly correlated.
    Granted(Box<HelperJob>),
    /// The claim was refused; no job, source, provider or Git effect occurred.
    Rejected(FailureCode),
}

/// Reports whether a settled operation may become visible to the model yet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Delivery {
    /// Both the final helper frame and the exact successful post-hook have arrived.
    Ready(Box<HelperResult>),
    /// One of the two required settlement halves is still missing.
    Waiting,
    /// Settlement failed, expired or the launch was denied; the outcome is honest and finite.
    Failed(FailureCode),
}

/// Holds the bounded set of outstanding Claude helper tickets for one daemon boot.
///
/// The ledger performs no I/O. It exists so that ticket admission, exact command recognition,
/// single-use claiming, expiry and post-order settlement are one testable atomic unit rather than
/// state spread across the transport, hook and worker paths.
#[derive(Debug, Default)]
pub struct LaunchLedger {
    /// Outstanding handles keyed by their action-scoped `detail_ref`.
    tickets: BTreeMap<String, LaunchTicket>,
}

impl LaunchLedger {
    /// Renders the exact fixed foreground helper command for one handle.
    ///
    /// The shape is fixed and fully daemon-chosen. It is rendered once, stored as the expected
    /// bytes, and returned to the model verbatim; recognition later compares bytes rather than
    /// parsing arbitrary shell syntax.
    pub fn helper_command(
        binary: &std::path::Path,
        runtime_dir: &std::path::Path,
        attachment: &str,
        detail_ref: &str,
    ) -> String {
        format!(
            "{} {HELPER_SUBCOMMAND} --runtime-dir {} --attachment {attachment} --detail-ref {detail_ref}",
            binary.display(),
            runtime_dir.display(),
        )
    }

    /// Mints one single-use ticket for an already validated Claude operation.
    ///
    /// Minting has no source, Git or provider effect: it only records what a later helper would be
    /// permitted to do. Capacity and job validity are checked before the handle becomes live.
    #[allow(clippy::too_many_arguments)]
    pub fn mint(
        &mut self,
        detail_ref: &str,
        binding: [u8; 32],
        actor: HelperActor,
        channel: &str,
        command: String,
        job: HelperJob,
        deadline_ms: u64,
    ) -> Result<(), FailureCode> {
        if self.tickets.len() >= MAX_TICKETS {
            return Err(FailureCode::Capacity);
        }
        if detail_ref.is_empty()
            || detail_ref.len() > MAX_IDENTIFIER_BYTES
            || command.is_empty()
            || command.len() > MAX_COMMAND_BYTES
            || channel.is_empty()
            || channel.len() > MAX_IDENTIFIER_BYTES
        {
            return Err(FailureCode::Conflict);
        }
        if self.tickets.contains_key(detail_ref) {
            return Err(FailureCode::Conflict);
        }
        job.validate()?;
        self.tickets.insert(
            detail_ref.to_owned(),
            LaunchTicket {
                binding,
                actor,
                channel: channel.to_owned(),
                command,
                job,
                deadline_ms,
                state: TicketState::Minted,
            },
        );
        Ok(())
    }

    /// Recognizes an ordinary foreground `Bash` pre-hook as the launch of one live ticket.
    ///
    /// Recognition requires byte-exact equality with the stored expected command and
    /// `run_in_background == false`. A background launch, a modified command, a superset command
    /// line or any other tool payload is [`LaunchRecognition::Ignored`] and discarded.
    /// Recognition is not authorization: it only records that the native launch happened.
    pub fn recognize(
        &mut self,
        command: &str,
        run_in_background: bool,
        tool_use_id: &str,
        now_ms: u64,
    ) -> LaunchRecognition {
        if run_in_background || tool_use_id.is_empty() {
            return LaunchRecognition::Ignored;
        }
        let Some(ticket) = self
            .tickets
            .values_mut()
            .find(|ticket| ticket.command.as_bytes() == command.as_bytes())
        else {
            return LaunchRecognition::Ignored;
        };
        if ticket.state != TicketState::Minted || now_ms >= ticket.deadline_ms {
            return LaunchRecognition::Ignored;
        }
        ticket.state = TicketState::Launched {
            tool_use_id: tool_use_id.to_owned(),
        };
        LaunchRecognition::Recognized
    }

    /// Atomically claims one recognized ticket exactly once and releases its closed job.
    ///
    /// Rejected: a handle whose native launch was never recognized (a bare copied reference), a
    /// different actor or channel, a stale binding generation, an expired deadline, and any second
    /// or replayed claim. Every rejection happens before a job is released, so a refused claim has
    /// no Git, source or provider effect whatsoever.
    pub fn claim(
        &mut self,
        detail_ref: &str,
        binding: [u8; 32],
        actor: &HelperActor,
        channel: &str,
        now_ms: u64,
    ) -> ClaimOutcome {
        let Some(ticket) = self.tickets.get_mut(detail_ref) else {
            return ClaimOutcome::Rejected(FailureCode::InvalidDetail);
        };
        if ticket.binding != binding {
            return ClaimOutcome::Rejected(FailureCode::WorkspaceAuthority);
        }
        if &ticket.actor != actor || ticket.channel != channel {
            return ClaimOutcome::Rejected(FailureCode::InvalidDetail);
        }
        if now_ms >= ticket.deadline_ms {
            return ClaimOutcome::Rejected(FailureCode::Deadline);
        }
        let TicketState::Launched { tool_use_id } = &ticket.state else {
            return ClaimOutcome::Rejected(FailureCode::InvalidDetail);
        };
        let tool_use_id = tool_use_id.clone();
        ticket.state = TicketState::Claimed {
            tool_use_id,
            frame: None,
            post: None,
        };
        ClaimOutcome::Granted(Box::new(ticket.job.clone()))
    }

    /// Records the helper's final frame; ordering against the post-hook does not matter.
    pub fn settle_frame(&mut self, result: HelperResult) -> Result<(), FailureCode> {
        result.validate()?;
        let Some(ticket) = self.tickets.get_mut(&result.detail_ref) else {
            return Err(FailureCode::InvalidDetail);
        };
        let TicketState::Claimed { frame, .. } = &mut ticket.state else {
            return Err(FailureCode::InvalidDetail);
        };
        if frame.is_some() {
            return Err(FailureCode::Conflict);
        }
        *frame = Some(result);
        Ok(())
    }

    /// Records the matching `Bash` post-hook for one claimed ticket.
    ///
    /// This post is special: it settles the helper's own operation and must not be treated as a
    /// generic native edit that invalidates the result the helper just produced. Only later
    /// unrelated native posts advance the native epoch.
    pub fn settle_post(&mut self, tool_use_id: &str, success: bool) -> Result<(), FailureCode> {
        let Some(ticket) = self.tickets.values_mut().find(|ticket| {
            matches!(&ticket.state, TicketState::Claimed { tool_use_id: bound, .. } if bound == tool_use_id)
        }) else {
            return Err(FailureCode::InvalidDetail);
        };
        let TicketState::Claimed { post, .. } = &mut ticket.state else {
            return Err(FailureCode::InvalidDetail);
        };
        if post.is_some() {
            return Err(FailureCode::Conflict);
        }
        *post = Some(success);
        Ok(())
    }

    /// Returns whether a completed post belongs to a helper this ledger launched.
    ///
    /// The hook path uses this to keep a helper's own post from invalidating its own result.
    pub fn owns_post(&self, tool_use_id: &str) -> bool {
        self.tickets.values().any(|ticket| {
            matches!(&ticket.state, TicketState::Claimed { tool_use_id: bound, .. } if bound == tool_use_id)
        })
    }

    /// Reports whether a claimed operation may become model-visible yet.
    ///
    /// Visibility requires both the final helper frame and the exact successful post-hook, in
    /// either order. A failed post, a missing half, or an uncertain ticket produces an honest
    /// finite outcome rather than a silent success.
    pub fn delivery(&self, detail_ref: &str) -> Delivery {
        let Some(ticket) = self.tickets.get(detail_ref) else {
            return Delivery::Failed(FailureCode::InvalidDetail);
        };
        match &ticket.state {
            TicketState::Uncertain => Delivery::Failed(FailureCode::Deadline),
            TicketState::Claimed {
                frame: Some(frame),
                post: Some(true),
                ..
            } => Delivery::Ready(Box::new(frame.clone())),
            TicketState::Claimed {
                post: Some(false), ..
            } => Delivery::Failed(FailureCode::Cancelled),
            TicketState::Claimed { .. } => Delivery::Waiting,
            TicketState::Minted | TicketState::Launched { .. } => Delivery::Waiting,
        }
    }

    /// Expires overdue tickets, distinguishing a launch that never happened from claimed work.
    ///
    /// An unclaimed ticket is dropped with no effect at all: nothing ran, so nothing must be
    /// cleaned up or reported. A claimed ticket that never settled becomes
    /// [`TicketState::Uncertain`] and is retained, so its admission stays quarantined instead of
    /// being silently reused.
    pub fn expire(&mut self, now_ms: u64) {
        self.tickets.retain(|_, ticket| {
            if now_ms < ticket.deadline_ms {
                return true;
            }
            match ticket.state {
                TicketState::Minted | TicketState::Launched { .. } => false,
                TicketState::Claimed { .. } => {
                    ticket.state = TicketState::Uncertain;
                    true
                }
                TicketState::Uncertain => true,
            }
        });
    }

    /// Drops every ticket fenced to one revoked binding generation.
    ///
    /// Stop revokes first; a ticket for a revoked generation can no longer be claimed, so a late
    /// helper is rejected rather than being allowed to run against stale authority.
    pub fn revoke(&mut self, binding: [u8; 32]) {
        self.tickets.retain(|_, ticket| ticket.binding != binding);
    }

    /// Returns whether one handle exists and belongs to the supplied binding generation.
    ///
    /// Retrieval uses this before reading any outcome, so a handle copied into another actor's
    /// or another generation's call can never surface a result it does not own.
    pub fn owned_by(&self, detail_ref: &str, binding: [u8; 32]) -> bool {
        self.tickets
            .get(detail_ref)
            .is_some_and(|ticket| ticket.binding == binding)
    }

    /// Returns the number of outstanding tickets for bounded-capacity assertions.
    pub fn len(&self) -> usize {
        self.tickets.len()
    }

    /// Returns whether no ticket is outstanding.
    pub fn is_empty(&self) -> bool {
        self.tickets.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds one valid Rust-profile job with entirely daemon-selected values.
    fn job() -> HelperJob {
        HelperJob {
            protocol: HELPER_PROTOCOL,
            operation: HelperOperation::Context,
            candidate: PathBuf::from("/private/tmp/work"),
            git: PathBuf::from("/usr/bin/git"),
            canonical_root: None,
            provider: Some(HelperProvider {
                executable: PathBuf::from("/Users/pluto/.local/bin/rust-analyzer"),
                language: HelperLanguage::Rust,
                rust_settings: Some(RustEffectiveSettings {
                    cache_priming: false,
                    proc_macro: false,
                }),
                cache_namespace: "ns-1".into(),
            }),
            parameters: serde_json::json!({"query": "x"}),
            budgets: HelperBudgets {
                output_bytes: 4096,
                processes: 2,
                deadline_ms: 30_000,
            },
        }
    }

    /// Mints one ticket on a fixed binding/actor/channel with the fixed helper command.
    fn ledger() -> (LaunchLedger, String, HelperActor) {
        let mut ledger = LaunchLedger::default();
        let actor = HelperActor::new("session", Some("agent")).unwrap();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        ledger
            .mint(
                "detail-1",
                [7; 32],
                actor.clone(),
                "channel",
                command,
                job(),
                1000,
            )
            .unwrap();
        (ledger, "detail-1".into(), actor)
    }

    /// Only strict macOS operator evidence enables the Claude path; Linux stays unavailable.
    #[test]
    fn only_strict_macos_operator_profile_is_accepted() {
        let strict = ClaudeOperatorProfile {
            enabled: true,
            fail_if_unavailable: true,
            allow_unsandboxed_commands: false,
            no_matching_excluded_commands: true,
            scope_declared: true,
            platform: HelperPlatform::MacOs,
        };
        assert_eq!(strict.validate(), Ok(()));
        for weakened in [
            ClaudeOperatorProfile {
                enabled: false,
                ..strict
            },
            ClaudeOperatorProfile {
                allow_unsandboxed_commands: true,
                ..strict
            },
            ClaudeOperatorProfile {
                fail_if_unavailable: false,
                ..strict
            },
            ClaudeOperatorProfile {
                platform: HelperPlatform::Linux,
                ..strict
            },
        ] {
            assert_eq!(weakened.validate(), Err(FailureCode::ExecutionProfile));
        }
    }

    /// Rust helper settings may never re-enable priming or proc-macro expansion.
    #[test]
    fn rust_profile_refuses_cache_priming_and_proc_macro() {
        let mut job = job();
        assert_eq!(job.validate(), Ok(()));
        job.provider.as_mut().unwrap().rust_settings = Some(RustEffectiveSettings {
            cache_priming: false,
            proc_macro: true,
        });
        assert_eq!(job.validate(), Err(FailureCode::ExecutionProfile));
        job.provider.as_mut().unwrap().language = HelperLanguage::Go;
        job.provider.as_mut().unwrap().rust_settings = None;
        assert_eq!(job.validate(), Ok(()));
    }

    /// A bare copied handle without a recognized native launch never releases a job.
    #[test]
    fn copied_reference_without_native_launch_is_rejected() {
        let (mut ledger, reference, actor) = ledger();
        assert_eq!(
            ledger.claim(&reference, [7; 32], &actor, "channel", 0),
            ClaimOutcome::Rejected(FailureCode::InvalidDetail)
        );
    }

    /// Only byte-exact foreground commands are recognized; nothing else is retained.
    #[test]
    fn recognition_requires_exact_bytes_and_foreground() {
        let (mut ledger, _, _) = ledger();
        let exact = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        assert_eq!(
            ledger.recognize(&format!("{exact} ; rm -rf /"), false, "call", 0),
            LaunchRecognition::Ignored
        );
        assert_eq!(
            ledger.recognize("cargo test", false, "call", 0),
            LaunchRecognition::Ignored
        );
        assert_eq!(
            ledger.recognize(&exact, true, "call", 0),
            LaunchRecognition::Ignored
        );
        assert_eq!(
            ledger.recognize(&exact, false, "call", 0),
            LaunchRecognition::Recognized
        );
        assert_eq!(
            ledger.recognize(&exact, false, "call-2", 0),
            LaunchRecognition::Ignored
        );
    }

    /// Wrong actor, wrong channel, stale generation and replayed claims are all refused.
    #[test]
    fn wrong_actor_channel_generation_and_replay_are_rejected() {
        let (mut ledger, reference, actor) = ledger();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        assert_eq!(
            ledger.recognize(&command, false, "call", 0),
            LaunchRecognition::Recognized
        );

        let other_actor = HelperActor::new("session", Some("other-agent")).unwrap();
        assert_eq!(
            ledger.claim(&reference, [7; 32], &other_actor, "channel", 0),
            ClaimOutcome::Rejected(FailureCode::InvalidDetail)
        );
        assert_eq!(
            ledger.claim(&reference, [7; 32], &actor, "other-channel", 0),
            ClaimOutcome::Rejected(FailureCode::InvalidDetail)
        );
        assert_eq!(
            ledger.claim(&reference, [9; 32], &actor, "channel", 0),
            ClaimOutcome::Rejected(FailureCode::WorkspaceAuthority)
        );

        assert!(matches!(
            ledger.claim(&reference, [7; 32], &actor, "channel", 0),
            ClaimOutcome::Granted(_)
        ));
        assert_eq!(
            ledger.claim(&reference, [7; 32], &actor, "channel", 0),
            ClaimOutcome::Rejected(FailureCode::InvalidDetail)
        );
    }

    /// A parent-session actor and a subagent of the same session are distinct claimants.
    #[test]
    fn parent_session_cannot_claim_a_subagent_ticket() {
        let (mut ledger, reference, _) = ledger();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        ledger.recognize(&command, false, "call", 0);
        let parent = HelperActor::new("session", None).unwrap();
        assert_eq!(
            ledger.claim(&reference, [7; 32], &parent, "channel", 0),
            ClaimOutcome::Rejected(FailureCode::InvalidDetail)
        );
    }

    /// An unclaimed expiry disappears with no effect; a claimed one stays quarantined.
    #[test]
    fn unclaimed_expiry_vanishes_and_claimed_expiry_stays_uncertain() {
        {
            let (mut ledger, reference, _) = ledger();
            ledger.expire(1000);
            assert!(ledger.is_empty());
            assert_eq!(
                ledger.delivery(&reference),
                Delivery::Failed(FailureCode::InvalidDetail)
            );
        }

        let (mut ledger, reference, actor) = ledger();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        ledger.recognize(&command, false, "call", 0);
        assert!(matches!(
            ledger.claim(&reference, [7; 32], &actor, "channel", 0),
            ClaimOutcome::Granted(_)
        ));
        ledger.expire(1000);
        assert_eq!(ledger.len(), 1);
        assert_eq!(
            ledger.delivery(&reference),
            Delivery::Failed(FailureCode::Deadline)
        );
    }

    /// Both settlement halves are required, and either arrival order reaches the same result.
    #[test]
    fn result_is_visible_only_after_frame_and_successful_post_in_either_order() {
        for frame_first in [true, false] {
            let (mut ledger, reference, actor) = ledger();
            let command = LaunchLedger::helper_command(
                std::path::Path::new("/usr/local/bin/agent-ide"),
                std::path::Path::new("/private/tmp/rt"),
                "attach",
                "detail-1",
            );
            ledger.recognize(&command, false, "call", 0);
            ledger.claim(&reference, [7; 32], &actor, "channel", 0);
            assert_eq!(ledger.delivery(&reference), Delivery::Waiting);

            let result = HelperResult {
                protocol: HELPER_PROTOCOL,
                detail_ref: reference.clone(),
                outcome: HelperOutcome::Complete { text: "ok".into() },
                children: ChildSettlement {
                    spawned: 2,
                    reaped: 2,
                },
            };
            if frame_first {
                ledger.settle_frame(result).unwrap();
                assert_eq!(ledger.delivery(&reference), Delivery::Waiting);
                ledger.settle_post("call", true).unwrap();
            } else {
                ledger.settle_post("call", true).unwrap();
                assert_eq!(ledger.delivery(&reference), Delivery::Waiting);
                ledger.settle_frame(result).unwrap();
            }
            let Delivery::Ready(delivered) = ledger.delivery(&reference) else {
                panic!("both halves settled");
            };
            assert!(delivered.children.settled());
        }
    }

    /// A failed post produces an honest finite failure instead of a silent success.
    #[test]
    fn failed_post_never_delivers_a_result() {
        let (mut ledger, reference, actor) = ledger();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        ledger.recognize(&command, false, "call", 0);
        ledger.claim(&reference, [7; 32], &actor, "channel", 0);
        ledger
            .settle_frame(HelperResult {
                protocol: HELPER_PROTOCOL,
                detail_ref: reference.clone(),
                outcome: HelperOutcome::Complete { text: "ok".into() },
                children: ChildSettlement {
                    spawned: 1,
                    reaped: 1,
                },
            })
            .unwrap();
        ledger.settle_post("call", false).unwrap();
        assert_eq!(
            ledger.delivery(&reference),
            Delivery::Failed(FailureCode::Cancelled)
        );
    }

    /// A helper's own post is recognized as its own, so it never invalidates its own result.
    #[test]
    fn helper_post_is_owned_and_unrelated_posts_are_not() {
        let (mut ledger, reference, actor) = ledger();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        ledger.recognize(&command, false, "call", 0);
        ledger.claim(&reference, [7; 32], &actor, "channel", 0);
        assert!(ledger.owns_post("call"));
        assert!(!ledger.owns_post("some-native-edit"));
    }

    /// Unreaped children are visible in the frame so a stop cannot be reported as settled.
    #[test]
    fn unsettled_children_are_reported_rather_than_assumed() {
        assert!(
            !ChildSettlement {
                spawned: 3,
                reaped: 1
            }
            .settled()
        );
        let forged = HelperResult {
            protocol: HELPER_PROTOCOL,
            detail_ref: "detail-1".into(),
            outcome: HelperOutcome::Failed {
                code: FailureCode::Deadline,
            },
            children: ChildSettlement {
                spawned: 1,
                reaped: 4,
            },
        };
        assert_eq!(forged.validate(), Err(FailureCode::ExecutionProfile));
    }

    /// Both wire directions reject foreign revisions, unknown fields and over-budget frames.
    #[test]
    fn wire_frames_are_closed_and_bounded_in_both_directions() {
        let encoded = job().encode().unwrap();
        assert_eq!(HelperJob::decode(&encoded).unwrap(), job());
        for raw in [
            r#"{"protocol":2,"operation":"context","candidate":"/a","git":"/b","canonical_root":null,"provider":null,"parameters":{},"budgets":{"output_bytes":1,"processes":1,"deadline_ms":1}}"#,
            r#"{"protocol":1,"operation":"context","candidate":"relative","git":"/b","canonical_root":null,"provider":null,"parameters":{},"budgets":{"output_bytes":1,"processes":1,"deadline_ms":1}}"#,
            r#"{"protocol":1,"operation":"inspect","candidate":"/a","git":"/b","canonical_root":null,"provider":null,"parameters":{},"budgets":{"output_bytes":1,"processes":1,"deadline_ms":1}}"#,
            r#"{"protocol":1,"operation":"context","candidate":"/a","git":"/b","canonical_root":null,"provider":null,"parameters":{},"budgets":{"output_bytes":1,"processes":1,"deadline_ms":1},"extra":1}"#,
        ] {
            assert!(HelperJob::decode(raw).is_err());
        }
        for raw in [
            r#"{"protocol":2,"detail_ref":"d","outcome":"failed","code":"internal","children":{"spawned":0,"reaped":0}}"#,
            r#"{"protocol":1,"detail_ref":"","outcome":"failed","code":"internal","children":{"spawned":0,"reaped":0}}"#,
            r#"{"protocol":1,"detail_ref":"d","outcome":"unknown","children":{"spawned":0,"reaped":0}}"#,
        ] {
            assert!(HelperResult::decode(raw).is_err());
        }
        assert!(HelperJob::decode(&"x".repeat(MAX_HELPER_FRAME_BYTES + 1)).is_err());
    }

    /// Stop revokes first: a ticket on a revoked generation can no longer be claimed.
    #[test]
    fn revocation_removes_tickets_before_a_late_helper_can_claim() {
        let (mut ledger, reference, actor) = ledger();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        ledger.recognize(&command, false, "call", 0);
        ledger.revoke([7; 32]);
        assert!(ledger.is_empty());
        assert_eq!(
            ledger.claim(&reference, [7; 32], &actor, "channel", 0),
            ClaimOutcome::Rejected(FailureCode::InvalidDetail)
        );
    }

    /// Ticket admission is bounded and duplicate handles are refused rather than overwritten.
    #[test]
    fn ticket_admission_is_bounded_and_refuses_duplicates() {
        let (mut ledger, _, actor) = ledger();
        assert_eq!(
            ledger.mint(
                "detail-1",
                [7; 32],
                actor.clone(),
                "channel",
                "cmd".into(),
                job(),
                1000
            ),
            Err(FailureCode::Conflict)
        );
        for index in 1..MAX_TICKETS {
            let reference = format!("detail-{}", index + 1);
            ledger
                .mint(
                    &reference,
                    [7; 32],
                    actor.clone(),
                    "channel",
                    format!("cmd-{index}"),
                    job(),
                    1000,
                )
                .unwrap();
        }
        assert_eq!(ledger.len(), MAX_TICKETS);
        assert_eq!(
            ledger.mint(
                "overflow",
                [7; 32],
                actor,
                "channel",
                "cmd-overflow".into(),
                job(),
                1000
            ),
            Err(FailureCode::Capacity)
        );
    }
}
