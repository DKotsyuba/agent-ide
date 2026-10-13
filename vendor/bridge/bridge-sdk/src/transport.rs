//! One real bounded Unix stream exchange; no factory, pool or async executor.
use crate::{ClientError, TransportOutcome};
use bridge_protocol::{MAX_RESPONSE_BYTES, Request, Response, decode_response, encode_request};
use socket2::{Domain, SockAddr, Socket, Type};
use std::{
    io::{Read, Write},
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::Path,
    time::{Duration, Instant},
};

/// Exchanges an immutable request at most twice within one absolute deadline.
/// All retried mutations reuse identical bytes and correlation/event IDs. A lost
/// response never grants a direct native fallback or a replacement identity.
pub(crate) fn exchange(
    path: &Path,
    request: &Request,
    timeout: Duration,
    reconnect: bool,
) -> Result<Response, ClientError> {
    let mut bytes = encode_request(request).map_err(ClientError::InvalidRequest)?;
    bytes.push(b'\n');
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or(ClientError::InvalidConfig("deadline overflow"))?;
    let mut last = None;
    let mut uncertain = false;
    for _ in 0..if reconnect { 2 } else { 1 } {
        if remaining(deadline).is_err() {
            break;
        }
        match once(path, request, &bytes, deadline) {
            Ok(response) => {
                if uncertain
                    && response.error.as_ref().is_some_and(|e| {
                        !matches!(
                            e.effect,
                            Some(
                                bridge_protocol::MutationEffect::Committed
                                    | bridge_protocol::MutationEffect::CommittedReplay
                            )
                        )
                    })
                {
                    return Err(failure(
                        request,
                        true,
                        "earlier mutation outcome unresolved",
                    ));
                }
                return Ok(response);
            }
            Err(error) => {
                uncertain |= matches!(
                    error,
                    ClientError::Transport {
                        outcome: TransportOutcome::OutcomeUnknown,
                        ..
                    }
                );
                last = Some(error);
            }
        }
    }
    let mut error = last.unwrap_or_else(|| failure(request, false, "deadline exhausted"));
    if uncertain && let ClientError::Transport { outcome, .. } = &mut error {
        *outcome = TransportOutcome::OutcomeUnknown;
    }
    Err(error)
}
/// Returns positive remaining budget; partial traffic never renews it.
fn remaining(deadline: Instant) -> Result<Duration, ()> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or(())
}
/// Constructs static safe uncertainty from observed write stage and operation kind.
fn failure(request: &Request, write_attempted: bool, reason: &'static str) -> ClientError {
    let reconciliation = request.reconciliation();
    let outcome = if !write_attempted {
        TransportOutcome::NotSent
    } else if reconciliation.is_some() {
        TransportOutcome::OutcomeUnknown
    } else {
        TransportOutcome::ReadFailed
    };
    ClientError::Transport {
        outcome,
        reconciliation,
        reason,
    }
}
/// Owns one safe nonblocking bounded connection and one correlated response frame.
fn once(
    path: &Path,
    request: &Request,
    bytes: &[u8],
    deadline: Instant,
) -> Result<Response, ClientError> {
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)
        .map_err(|_| failure(request, false, "socket creation failed"))?;
    let address =
        SockAddr::unix(path).map_err(|_| failure(request, false, "invalid socket address"))?;
    socket
        .connect_timeout(
            &address,
            remaining(deadline).map_err(|_| failure(request, false, "connect deadline"))?,
        )
        .map_err(|_| failure(request, false, "connect failed"))?;
    let owned: OwnedFd = socket.into();
    let mut stream = UnixStream::from(owned);
    let mut offset = 0;
    while offset < bytes.len() {
        let budget =
            remaining(deadline).map_err(|_| failure(request, offset != 0, "write deadline"))?;
        stream
            .set_write_timeout(Some(budget))
            .map_err(|_| failure(request, offset != 0, "write timeout unavailable"))?;
        match stream.write(&bytes[offset..]) {
            Ok(0) => return Err(failure(request, true, "write closed")),
            Ok(n) => offset += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
            Err(_) => return Err(failure(request, true, "write failed")),
        }
    }
    let mut frame = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let budget =
            remaining(deadline).map_err(|_| failure(request, true, "response deadline"))?;
        stream
            .set_read_timeout(Some(budget))
            .map_err(|_| failure(request, true, "read timeout unavailable"))?;
        let n = match stream.read(&mut buffer) {
            Ok(0) => return Err(failure(request, true, "response closed")),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(failure(request, true, "read failed")),
        };
        if let Some(end) = buffer[..n].iter().position(|b| *b == b'\n') {
            if frame.len() + end > MAX_RESPONSE_BYTES || end + 1 != n {
                return Err(failure(request, true, "invalid response framing"));
            }
            frame.extend_from_slice(&buffer[..end]);
            return decode_response(&frame, request)
                .map_err(|_| failure(request, true, "invalid correlated response"));
        }
        if frame.len() + n > MAX_RESPONSE_BYTES {
            return Err(failure(request, true, "response too large"));
        }
        frame.extend_from_slice(&buffer[..n]);
    }
}
