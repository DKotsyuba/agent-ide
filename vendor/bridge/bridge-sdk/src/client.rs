//! Small typed facade sharing immutable config; no connection/global request lock.
use crate::{ClientError, Config, transport};
use bridge_protocol::*;

/// Immutable synchronous producer facade; each call owns its own Unix stream.
#[derive(Debug, Clone)]
pub struct Client {
    /// Explicit trusted deployment configuration, never auto-discovered.
    config: Config,
}
impl Client {
    /// Validates configuration without connecting or consuming evidence.
    pub fn new(config: Config) -> Result<Self, ClientError> {
        config.validate()?;
        Ok(Self { config })
    }
    /// Executes an explicit generic operation with caller-persisted identity.
    /// Reads use core except capabilities, which queries adapter-composed host facts.
    /// Mutations return custody/route facts only; producer admission remains local.
    pub fn execute(
        &self,
        request_id: impl Into<String>,
        operation: Operation,
    ) -> Result<OperationResult, ClientError> {
        let request = Request::new(request_id, self.config.credential.clone(), operation)
            .map_err(ClientError::InvalidRequest)?;
        let endpoint = match request.operation {
            Operation::Capabilities(_)
            | Operation::RegisterCall(_)
            | Operation::ConsumeAssertion(_) => &self.config.registration_socket,
            _ => &self.config.core_socket,
        };
        let response = transport::exchange(
            endpoint,
            &request,
            self.config.timeout,
            self.config.reconnect_once,
        )?;
        if let Some(error) = response.error {
            return Err(ClientError::Service(error));
        }
        response.result.ok_or(ClientError::Transport {
            outcome: crate::TransportOutcome::OutcomeUnknown,
            reconciliation: request.reconciliation(),
            reason: "missing validated result",
        })
    }
    /// Reads authenticated native/core capabilities without caller evidence.
    pub fn capabilities(&self, request_id: impl Into<String>) -> Result<Capabilities, ClientError> {
        match self.execute(request_id, Operation::Capabilities(CapabilitiesParams {}))? {
            OperationResult::Capabilities(r) => Ok(r),
            _ => unreachable!("validated result operation"),
        }
    }
    /// Commits/recovers registration before producer notification-required work.
    pub fn register_call(
        &self,
        request_id: impl Into<String>,
        params: RegisterCallParams,
    ) -> Result<RegistrationResult, ClientError> {
        match self.execute(request_id, Operation::RegisterCall(params))? {
            OperationResult::RegisterCall(r) => Ok(r),
            _ => unreachable!("validated result operation"),
        }
    }
    /// Commits/recovers exact event custody; result is not native delivery.
    pub fn publish(
        &self,
        request_id: impl Into<String>,
        params: PublishParams,
    ) -> Result<PublishResult, ClientError> {
        match self.execute(request_id, Operation::Publish(params))? {
            OperationResult::Publish(r) => Ok(r),
            _ => unreachable!("validated result operation"),
        }
    }
    /// Observes owned historical state; never retries/resends native delivery.
    pub fn status(
        &self,
        request_id: impl Into<String>,
        selector: StatusSelector,
    ) -> Result<StatusResult, ClientError> {
        match self.execute(request_id, Operation::Status(StatusParams { selector }))? {
            OperationResult::Status(r) => Ok(r),
            _ => unreachable!("validated result operation"),
        }
    }
    /// Fences new route custody only, preserving prior accepted event effects.
    pub fn close_route(
        &self,
        request_id: impl Into<String>,
        params: CloseRouteParams,
    ) -> Result<CloseResult, ClientError> {
        match self.execute(request_id, Operation::CloseRoute(params))? {
            OperationResult::CloseRoute(r) => Ok(r),
            _ => unreachable!("validated result operation"),
        }
    }
    /// Requests advertised authenticated audience transfer; historical is not fresh.
    pub fn consume_assertion(
        &self,
        request_id: impl Into<String>,
        params: ConsumeAssertionParams,
    ) -> Result<ConsumeResult, ClientError> {
        match self.execute(request_id, Operation::ConsumeAssertion(params))? {
            OperationResult::ConsumeAssertion(r) => Ok(r),
            _ => unreachable!("validated result operation"),
        }
    }
    /// Requests advertised producer lifetime fence, never recall/replay authority.
    pub fn retire_delivery_scope(
        &self,
        request_id: impl Into<String>,
        params: RetireDeliveryScopeParams,
    ) -> Result<RetireResult, ClientError> {
        match self.execute(request_id, Operation::RetireDeliveryScope(params))? {
            OperationResult::RetireDeliveryScope(r) => Ok(r),
            _ => unreachable!("validated result operation"),
        }
    }
}
