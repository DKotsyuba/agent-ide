//! Explicit trusted deployment configuration; no environment/cwd discovery.
use crate::ClientError;
use bridge_protocol::{Secret, validate_id};
use std::{path::PathBuf, time::Duration};

/// Immutable shareable endpoint/credential/deadline configuration.
/// Endpoints must reside in service-owned protected directories; this SDK does
/// not recreate native process attestation. One client may serve many threads.
#[derive(Debug, Clone)]
pub struct Config {
    /// Explicit native registration/assertion Unix endpoint.
    pub registration_socket: PathBuf,
    /// Explicit durable publication/status/lifecycle Unix endpoint.
    pub core_socket: PathBuf,
    /// Protected configured producer/backend credential; Debug always redacts.
    pub credential: Secret,
    /// One finite positive total exchange deadline, including any reconnect.
    pub timeout: Duration,
    /// Whether one identical safe exchange reconnect is allowed; never loops.
    pub reconnect_once: bool,
}
impl Config {
    /// Builds explicit configuration and validates absolute paths/finite timeout.
    /// No socket is opened and no evidence is consumed. Defaults one safe retry.
    pub fn new(
        registration_socket: impl Into<PathBuf>,
        core_socket: impl Into<PathBuf>,
        credential: Secret,
        timeout: Duration,
    ) -> Result<Self, ClientError> {
        let config = Self {
            registration_socket: registration_socket.into(),
            core_socket: core_socket.into(),
            credential,
            timeout,
            reconnect_once: true,
        };
        config.validate()?;
        Ok(config)
    }
    /// Revalidates public fields before client adoption; does not probe endpoints.
    pub fn validate(&self) -> Result<(), ClientError> {
        if !self.registration_socket.is_absolute() || !self.core_socket.is_absolute() {
            return Err(ClientError::InvalidConfig("endpoints must be absolute"));
        }
        if self.timeout.is_zero()
            || std::time::Instant::now()
                .checked_add(self.timeout)
                .is_none()
        {
            return Err(ClientError::InvalidConfig("invalid finite deadline"));
        }
        validate_id("credential", self.credential.expose(), 16384)
            .map_err(ClientError::InvalidRequest)?;
        Ok(())
    }
}
