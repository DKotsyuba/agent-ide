//! Generic immutable event intent and custody DTOs; no SDK business outbox.
pub use bridge_protocol::{
    Destination, EventReceipt, FrozenDestination, Metadata, Mode, Payload, Policy, PublishParams,
    PublishResult, RetireDeliveryScopeParams, RetireReceipt, RetireResult, RetirementAction,
};
