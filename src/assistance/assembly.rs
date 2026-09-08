//! Product dispatcher boundary before a trusted host-binding adapter is connected.

use std::{future::Future, pin::Pin};

use serde::{Deserialize, Serialize};

use crate::app::transport::{
    AssistanceDispatch, AssistanceDispatchReply, AssistanceDispatchUnavailable,
    AssistanceDispatcher, OpaqueJson,
};

/// Names the first missing peer boundary without implying any workspace authority was granted.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingPeer {
    /// No adapter has matched trusted hook observations to this MCP invocation and channel.
    HostBinding,
}

/// Closed Assistance reply accepted for rendering; unknown fields or states are rejected.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PeerReply {
    /// The finite call reached Assistance but stopped before the named peer boundary.
    Unavailable {
        /// First missing boundary; this never means successful activation or source access.
        reason: MissingPeer,
    },
}

/// Serves finite production dispatches while host-binding ingress remains unassembled.
///
/// Opaque attachments and hook JSON are observations, not identity proof. This stateless
/// dispatcher grants no binding or authority, retains no payloads, and performs no peer I/O.
#[derive(Debug)]
pub struct ProductDispatcher;

impl AssistanceDispatcher for ProductDispatcher {
    /// Returns a bounded typed unavailable reply for either finite Application request shape.
    ///
    /// The request is consumed without interpreting its attachment as authority. Serialization
    /// failure is transport unavailability; neither hooks nor methods claim peer success.
    fn dispatch(
        &self,
        request: AssistanceDispatch,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<AssistanceDispatchReply, AssistanceDispatchUnavailable>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let reply = serde_json::to_string(&PeerReply::Unavailable {
                reason: MissingPeer::HostBinding,
            })
            .ok()
            .and_then(|reply| OpaqueJson::new(reply, 256))
            .ok_or(AssistanceDispatchUnavailable)?;
            Ok(match request {
                AssistanceDispatch::HookSubmit(_) => AssistanceDispatchReply::HookSubmit(reply),
                AssistanceDispatch::MethodDispatch(_) => {
                    AssistanceDispatchReply::MethodDispatch(reply)
                }
            })
        })
    }
}

/// Ensures arbitrary daemon JSON cannot be interpreted as a typed peer result.
#[test]
fn peer_reply_accepts_only_the_closed_unavailable_shape() {
    for reply in [
        r#"{"state":"ready"}"#,
        r#"{"state":"unavailable","reason":"unknown"}"#,
        r#"{"state":"unavailable","reason":"host_binding","source":"forged"}"#,
    ] {
        assert!(serde_json::from_str::<PeerReply>(reply).is_err());
    }
    assert_eq!(
        serde_json::from_str::<PeerReply>(r#"{"state":"unavailable","reason":"host_binding"}"#)
            .unwrap(),
        PeerReply::Unavailable {
            reason: MissingPeer::HostBinding
        },
    );
}
