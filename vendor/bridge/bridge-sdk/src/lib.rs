//! Thin synchronous universal producer client for protected local Bridge sockets.
//! No domain outbox, native attestation, core/storage dependency or background work.
pub mod client;
pub mod config;
pub mod native_meta;
pub mod publication;
pub mod registration;
pub mod status;
mod transport;
pub use bridge_protocol;
use bridge_protocol::{ApiError, ProtocolError, ReconciliationKey};
pub use client::Client;
pub use config::Config;
pub use native_meta::extract_native_evidence;

/// Safe transport observation, distinct from service commit/native delivery facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportOutcome {
    /// Definitely no request write was attempted.
    NotSent,
    /// Some request bytes may have reached service; exact reconciliation only.
    OutcomeUnknown,
    /// Read-only request failed; no mutation effect is claimed.
    ReadFailed,
}
/// Secret-safe SDK error; formatting never prints raw response/body/I/O details.
#[derive(Clone, PartialEq)]
pub enum ClientError {
    /// Explicit local configuration was invalid; no I/O occurred.
    InvalidConfig(&'static str),
    /// Pure local request encoding/validation failed before sending.
    InvalidRequest(ProtocolError),
    /// Correlated validated authenticated service error, retained for typed handling.
    Service(ApiError),
    /// Transport result with original mutation reconciliation identity if any.
    Transport {
        /// Not-sent/unknown/read-failed observation.
        outcome: TransportOutcome,
        /// Original stable operation/event request key, never a replacement.
        reconciliation: Option<ReconciliationKey>,
        /// Static safe explanation, not raw native/service response text.
        reason: &'static str,
    },
}
impl std::fmt::Debug for ClientError {
    /// Prints safe classes only; service detail and response bodies stay private.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfig(reason) => f.debug_tuple("InvalidConfig").field(reason).finish(),
            Self::InvalidRequest(error) => f.debug_tuple("InvalidRequest").field(error).finish(),
            Self::Service(error) => f
                .debug_struct("Service")
                .field("class", &error.class)
                .field("reason", &error.reason)
                .field("effect", &error.effect)
                .finish(),
            Self::Transport {
                outcome, reason, ..
            } => f
                .debug_struct("Transport")
                .field("outcome", outcome)
                .field("reason", reason)
                .finish(),
        }
    }
}
impl std::fmt::Display for ClientError {
    /// Uses the same secret-safe class-only diagnostic as Debug.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ClientError {}
