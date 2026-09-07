//! Bounded LSP framing, request tracking, and read-only server callbacks.
//!
//! This module validates LSP frame envelopes before `async-lsp` parses messages. It does not
//! decode JSON-RPC requests, spawn processes, or expose a product-facing wire API.

use std::collections::BTreeSet;

use async_lsp::{
    lsp_types::{ApplyWorkspaceEditResponse, request::ApplyWorkspaceEdit},
    router::Router,
};

/// Sets finite limits for one internal LSP transport generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WireLimits {
    /// Largest allowed header, including its terminating CRLF pair.
    pub max_header_bytes: usize,
    /// Largest allowed JSON body declared by one `Content-Length` header.
    pub max_body_bytes: usize,
    /// Largest number of client logical requests awaiting a response.
    pub max_outstanding_requests: usize,
    /// Largest retained undecoded transport input across fragments.
    pub max_buffered_bytes: usize,
}

impl WireLimits {
    /// Creates nonzero limits whose retained-input cap can hold one complete maximum frame.
    pub(crate) const fn new(
        max_header_bytes: usize,
        max_body_bytes: usize,
        max_outstanding_requests: usize,
        max_buffered_bytes: usize,
    ) -> Option<Self> {
        if max_header_bytes == 0
            || max_body_bytes == 0
            || max_outstanding_requests == 0
            || max_buffered_bytes < max_header_bytes.saturating_add(max_body_bytes)
        {
            return None;
        }
        Some(Self {
            max_header_bytes,
            max_body_bytes,
            max_outstanding_requests,
            max_buffered_bytes,
        })
    }
}

/// Explains why a wire generation cannot continue accepting input or requests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WireError {
    /// A frame did not terminate its header within the configured header ceiling.
    HeaderTooLarge,
    /// A frame declared a JSON body larger than the configured body ceiling.
    BodyTooLarge,
    /// Retained fragmented input exceeded the configured bounded transport buffer.
    BufferTooLarge,
    /// Header bytes were not valid UTF-8 LSP header text.
    InvalidHeaderEncoding,
    /// A header line was malformed, omitted Content-Length, or repeated it.
    MalformedHeader,
    /// Content-Length was not a nonnegative base-10 byte count.
    InvalidContentLength,
    /// A request ID was already awaiting its response.
    DuplicateRequest,
    /// The logical outstanding-request ceiling was reached.
    TooManyOutstandingRequests,
    /// The generation was invalidated by a previous protocol failure or EOF.
    GenerationInvalidated,
}

/// Classifies a response after cancellation or normal completion lookup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResponseDisposition {
    /// The response matches an active logical request and may be delivered once.
    Deliver,
    /// The response is late, cancelled, unknown, or already delivered and must be discarded.
    DiscardLate,
}

/// Reports the one-time state change caused by transport EOF.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EofDisposition {
    /// EOF invalidated this generation and requires owned-pipe cleanup handoff to Execution.
    Invalidated { generation: u64 },
    /// EOF was already processed for this generation.
    AlreadyInvalidated { generation: u64 },
}

/// Tracks one bounded transport generation before semantic views use its parsed messages.
#[derive(Debug)]
pub(crate) struct WireSafety {
    /// Immutable ceilings selected by the accepted profile.
    limits: WireLimits,
    /// Bytes not yet emitted as a complete, validated LSP frame.
    buffered: Vec<u8>,
    /// Header and body lengths for the frame currently awaiting body bytes.
    pending_frame: Option<(usize, usize)>,
    /// Logical client request IDs that still accept one response.
    outstanding: BTreeSet<u64>,
    /// Monotonic generation identifier supplied by the owning provider lifecycle.
    generation: u64,
    /// Whether framing failure or EOF made this generation unusable.
    invalidated: bool,
}

impl WireSafety {
    /// Starts an empty, valid wire generation with the supplied finite ceilings.
    pub(crate) fn new(limits: WireLimits, generation: u64) -> Self {
        Self {
            limits,
            buffered: Vec::new(),
            pending_frame: None,
            outstanding: BTreeSet::new(),
            generation,
            invalidated: false,
        }
    }

    /// Validates fragmented or coalesced bytes and returns complete untouched LSP frames.
    ///
    /// A returned frame still includes its original header and body. Any error invalidates this
    /// generation so an owning view cannot resume it with later input.
    pub(crate) fn ingest(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, WireError> {
        self.ensure_active()?;
        if self.buffered.len().saturating_add(bytes.len()) > self.limits.max_buffered_bytes {
            return self.fail(WireError::BufferTooLarge);
        }
        self.buffered.extend_from_slice(bytes);

        let mut frames = Vec::new();
        loop {
            if self.pending_frame.is_none() {
                let Some(header_end) = header_end(&self.buffered) else {
                    if self.buffered.len() > self.limits.max_header_bytes {
                        return self.fail(WireError::HeaderTooLarge);
                    }
                    break;
                };
                if header_end > self.limits.max_header_bytes {
                    return self.fail(WireError::HeaderTooLarge);
                }
                let body_len = match content_length(&self.buffered[..header_end]) {
                    Ok(length) => length,
                    Err(error) => return self.fail(error),
                };
                if body_len > self.limits.max_body_bytes {
                    return self.fail(WireError::BodyTooLarge);
                }
                self.pending_frame = Some((header_end, body_len));
            }

            let (header_len, body_len) = self.pending_frame.expect("frame header was recorded");
            let frame_len = header_len.saturating_add(body_len);
            if self.buffered.len() < frame_len {
                break;
            }
            frames.push(self.buffered.drain(..frame_len).collect());
            self.pending_frame = None;
        }
        Ok(frames)
    }

    /// Registers one logical request before it is sent through `async-lsp`.
    pub(crate) fn begin_request(&mut self, request: u64) -> Result<(), WireError> {
        self.ensure_active()?;
        if self.outstanding.contains(&request) {
            return Err(WireError::DuplicateRequest);
        }
        if self.outstanding.len() == self.limits.max_outstanding_requests {
            return Err(WireError::TooManyOutstandingRequests);
        }
        self.outstanding.insert(request);
        Ok(())
    }

    /// Disposes a logical request before a later response can reach a view.
    pub(crate) fn cancel_request(&mut self, request: u64) -> bool {
        self.outstanding.remove(&request)
    }

    /// Delivers one active response once or classifies it as a late response to discard.
    pub(crate) fn complete_response(&mut self, request: u64) -> ResponseDisposition {
        if self.outstanding.remove(&request) {
            ResponseDisposition::Deliver
        } else {
            ResponseDisposition::DiscardLate
        }
    }

    /// Invalidates this generation on EOF and requests the one owned-pipe cleanup handoff.
    pub(crate) fn eof(&mut self) -> EofDisposition {
        if self.invalidated {
            EofDisposition::AlreadyInvalidated {
                generation: self.generation,
            }
        } else {
            self.invalidated = true;
            self.outstanding.clear();
            EofDisposition::Invalidated {
                generation: self.generation,
            }
        }
    }

    /// Returns an error when an invalidated generation receives another operation.
    fn ensure_active(&self) -> Result<(), WireError> {
        (!self.invalidated)
            .then_some(())
            .ok_or(WireError::GenerationInvalidated)
    }

    /// Invalidates the generation while returning the parsing failure that caused it.
    fn fail<T>(&mut self, error: WireError) -> Result<T, WireError> {
        self.invalidated = true;
        self.outstanding.clear();
        Err(error)
    }
}

/// Creates the `async-lsp` router that rejects server workspace edits without touching disk.
pub(crate) fn read_only_router() -> Router<()> {
    let mut router = Router::new(());
    router.request::<ApplyWorkspaceEdit, _>(|_, _| async {
        Ok(ApplyWorkspaceEditResponse {
            applied: false,
            failure_reason: Some("workspace edits are unavailable in Intelligence v0.1".into()),
            failed_change: None,
        })
    });
    router
}

/// Finds the byte offset immediately after an LSP header terminator.
fn header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|start| start + 4)
}

/// Parses exactly one Content-Length header from complete UTF-8 LSP header bytes.
fn content_length(header: &[u8]) -> Result<usize, WireError> {
    let header = std::str::from_utf8(header).map_err(|_| WireError::InvalidHeaderEncoding)?;
    let header = header
        .strip_suffix("\r\n\r\n")
        .ok_or(WireError::MalformedHeader)?;
    let mut length = None;
    for line in header.split("\r\n") {
        let (name, value) = line.split_once(':').ok_or(WireError::MalformedHeader)?;
        if name.eq_ignore_ascii_case("Content-Length") {
            if length.is_some() {
                return Err(WireError::MalformedHeader);
            }
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| WireError::InvalidContentLength)?,
            );
        }
    }
    length.ok_or(WireError::MalformedHeader)
}

#[cfg(test)]
/// Exercises the bounded safety contract without a provider process.
mod tests {
    use super::*;
    use async_lsp::{
        AnyRequest,
        lsp_types::{ApplyWorkspaceEditParams, WorkspaceEdit, request::Request},
    };
    use serde_json::json;
    use tower_service::Service;

    /// Builds a complete test LSP frame without parsing its JSON body.
    fn frame(body: &str) -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
    }

    /// Returns finite test limits that accept two tiny coalesced frames.
    fn limits() -> WireLimits {
        WireLimits::new(64, 32, 1, 128).expect("test limits are valid")
    }

    /// Accepts fragmented headers and coalesced frames without changing their bytes.
    #[test]
    fn validates_fragmented_and_coalesced_frames() {
        let first = frame("{}");
        let second = frame("[]");
        let mut safety = WireSafety::new(limits(), 9);
        assert!(safety.ingest(&first[..8]).unwrap().is_empty());
        let mut rest = first[8..].to_vec();
        rest.extend_from_slice(&second);
        assert_eq!(safety.ingest(&rest).unwrap(), vec![first, second]);
    }

    /// Rejects oversized, malformed, and invalid-length framing before JSON parsing.
    #[test]
    fn rejects_bad_frame_envelopes() {
        let mut header = WireSafety::new(limits(), 1);
        assert_eq!(header.ingest(&[b'x'; 65]), Err(WireError::HeaderTooLarge));
        let mut malformed = WireSafety::new(limits(), 2);
        assert_eq!(
            malformed.ingest(b"Content-Length: nope\r\n\r\n"),
            Err(WireError::InvalidContentLength)
        );
        let mut duplicate = WireSafety::new(limits(), 3);
        assert_eq!(
            duplicate.ingest(b"Content-Length: 1\r\nContent-Length: 1\r\n\r\n"),
            Err(WireError::MalformedHeader)
        );
        let mut body = WireSafety::new(limits(), 3);
        assert_eq!(
            body.ingest(b"Content-Length: 33\r\n\r\n"),
            Err(WireError::BodyTooLarge)
        );
    }

    /// Disposes cancellations and refuses to deliver their late responses.
    #[test]
    fn disposes_cancelled_request_before_late_response() {
        let mut safety = WireSafety::new(limits(), 4);
        safety.begin_request(7).unwrap();
        assert_eq!(
            safety.begin_request(8),
            Err(WireError::TooManyOutstandingRequests)
        );
        assert!(safety.cancel_request(7));
        assert_eq!(
            safety.complete_response(7),
            ResponseDisposition::DiscardLate
        );
    }

    /// Invalidates once on EOF and requires a later caller to create a new generation.
    #[test]
    fn eof_invalidates_generation_and_discards_requests() {
        let mut safety = WireSafety::new(limits(), 5);
        safety.begin_request(1).unwrap();
        assert_eq!(safety.eof(), EofDisposition::Invalidated { generation: 5 });
        assert_eq!(
            safety.begin_request(2),
            Err(WireError::GenerationInvalidated)
        );
        assert_eq!(
            safety.eof(),
            EofDisposition::AlreadyInvalidated { generation: 5 }
        );
    }

    /// Verifies the real async-lsp router rejects workspace/applyEdit without a disk writer.
    #[tokio::test]
    async fn rejects_workspace_apply_edit() {
        let request: AnyRequest = serde_json::from_value(json!({
            "id": 1,
            "method": ApplyWorkspaceEdit::METHOD,
            "params": ApplyWorkspaceEditParams {
                label: None,
                edit: WorkspaceEdit::default(),
            },
        }))
        .unwrap();
        let response = read_only_router().call(request).await.unwrap();
        let response: ApplyWorkspaceEditResponse = serde_json::from_value(response).unwrap();
        assert!(!response.applied);
        assert!(response.failure_reason.unwrap().contains("unavailable"));
    }
}
