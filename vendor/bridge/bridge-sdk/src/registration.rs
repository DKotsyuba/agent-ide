//! Public route/observation/assertion DTOs are the single protocol definitions.
//! Historical receipts never grant fresh work; producer admission stays local.
pub use bridge_protocol::{
    CallerEvidenceState, ConsumeAssertionParams, ConsumeResult, ConsumptionReceipt, FreshAssertion,
    InvocationContext, InvocationValidity, RegisterCallParams, RegistrationReceipt,
    RegistrationResult, ReplyPolicy, RouteConstraints, RouteState,
};
