//! Core side of one `bundled-module/0` instance: the `hello` exchange and sequential requests
//! with an absolute budget, exact fences and attachment assembly.
//!
//! A [`HostChannel`] is transport only. Process admission, spawning, supervision, restart policy
//! and stderr capture belong to the host runtime that owns the channel; this type reports every
//! fault as a typed [`ModuleUnavailable`] and poisons itself, so no later message can settle
//! another request. Effects a module asks for during a request go to the caller's
//! [`EffectRunner`]; a repeated call number answers the earlier outcome without running again.

use std::time::Duration;

use crate::checks::BoxFuture;
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
};

use super::{
    contract::{
        Capability, Cause, Control, Coverage, EffectReply, Fence, HelloOffer, HelloReply,
        ModuleUnavailable, Outcome, Readiness, Request, Stage, spill, unspill,
    },
    payload::{EffectOutcome, EffectRequest},
    wire::{
        Assembler, Attachment, AttachmentError, Frame, WireError, read_frame, write_attachment,
        write_control,
    },
};

/// Runs the effect recipes a module asks for; the core's implementation expands, admits and
/// confines them.
pub trait EffectRunner: Send {
    /// Runs `effect` for the active request `fence`; returns the outcome and its output
    /// attachments (stdout id 1, stderr id 2).
    fn run<'a>(
        &'a mut self,
        fence: &'a Fence,
        effect: EffectRequest,
    ) -> BoxFuture<'a, (EffectOutcome, Vec<Attachment>)>;
}

/// An [`EffectRunner`] that refuses every effect.
pub struct NoEffects;

impl EffectRunner for NoEffects {
    /// Refuses with `policy_refused`.
    fn run<'a>(
        &'a mut self,
        _fence: &'a Fence,
        _effect: EffectRequest,
    ) -> BoxFuture<'a, (EffectOutcome, Vec<Attachment>)> {
        Box::pin(async {
            (
                EffectOutcome::Refused {
                    cause: Cause::PolicyRefused,
                    message: "no effects for this request".into(),
                },
                Vec::new(),
            )
        })
    }
}

/// One request the core sends.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    /// Capability addressed.
    pub capability: Capability,
    /// Opaque scope key.
    pub scope_key: String,
    /// Opaque revision key.
    pub revision_key: String,
    /// Typed payload.
    pub payload: Value,
    /// Attachments sent after the request.
    pub attachments: Vec<Attachment>,
}

/// One accepted reply.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    /// Result or the module's typed refusal.
    pub outcome: Outcome,
    /// Readiness of the capability.
    pub readiness: Readiness,
    /// Completeness of the result.
    pub coverage: Coverage,
    /// Attachments of the reply.
    pub attachments: Vec<Attachment>,
}

/// Core side of one instance.
pub struct HostChannel {
    /// To the module.
    writer: Box<dyn AsyncWrite + Send + Unpin>,
    /// Frames (or the terminal read error) from the reader task, in arrival order.
    frames: mpsc::Receiver<Result<Frame, WireError>>,
    /// The accepted offer.
    offer: HelloOffer,
    /// Last request id sent.
    last_request: u64,
    /// A call has not settled; a dropped call leaves it set and poisons the next one.
    in_flight: bool,
    /// The first fault, after which every call fails without touching the module.
    fault: Option<(Stage, Cause)>,
}

/// The [`Cause`] of a transport fault.
fn wire_cause(error: &WireError) -> Cause {
    match error {
        WireError::Eof | WireError::Truncated | WireError::Io(_) => Cause::Exited,
        WireError::Oversized(_) => Cause::Oversized,
        WireError::ZeroLength | WireError::UnknownKind(_) | WireError::Malformed => {
            Cause::Malformed
        }
    }
}

/// The [`Cause`] of a refused attachment delivery.
fn attachment_cause(error: &AttachmentError) -> Cause {
    match error {
        AttachmentError::OverBudget(_) => Cause::Oversized,
        AttachmentError::WrongRequest(_) => Cause::WrongFence,
        _ => Cause::Malformed,
    }
}

impl HostChannel {
    /// Wraps the module's stdout (`input`) and stdin (`output`), sends `offer` and waits at most
    /// `budget` for an acceptable `hello` reply.
    pub async fn open<R, W>(
        input: R,
        output: W,
        offer: HelloOffer,
        budget: Duration,
    ) -> Result<(Self, HelloReply), ModuleUnavailable>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (sender, frames) = mpsc::channel(16);
        tokio::spawn(async move {
            let mut input = input;
            loop {
                let frame = read_frame(&mut input).await;
                let last = frame.is_err();
                if sender.send(frame).await.is_err() || last {
                    return;
                }
            }
        });
        let mut channel = Self {
            writer: Box::new(output),
            frames,
            offer,
            last_request: 0,
            in_flight: false,
            fault: None,
        };
        let exchange = async {
            let hello = Control::Hello(channel.offer.clone());
            write_control(&mut channel.writer, &hello)
                .await
                .map_err(|error| wire_cause(&error))?;
            match channel.next_control().await? {
                Control::HelloReply(reply) => match channel.offer.accept(&reply) {
                    Ok(()) => Ok(reply),
                    Err(_) => Err(Cause::Incompatible),
                },
                Control::HelloRefused { .. } => Err(Cause::Incompatible),
                _ => Err(Cause::Malformed),
            }
        };
        let outcome = tokio::time::timeout(budget, exchange).await;
        match outcome {
            Ok(Ok(reply)) => Ok((channel, reply)),
            Ok(Err(cause)) => Err(channel.poison(Stage::Hello, cause)),
            Err(_) => Err(channel.poison(Stage::Hello, Cause::Timeout)),
        }
    }

    /// The accepted offer.
    pub fn offer(&self) -> &HelloOffer {
        &self.offer
    }

    /// The fault that poisoned this channel, if any.
    pub fn fault(&self) -> Option<(Stage, Cause)> {
        self.fault
    }

    /// Records the first fault and returns its typed failure.
    pub fn poison(&mut self, stage: Stage, cause: Cause) -> ModuleUnavailable {
        let (stage, cause) = *self.fault.get_or_insert((stage, cause));
        ModuleUnavailable {
            module_id: self.offer.module_id.clone(),
            module_version: self.offer.package_version.clone(),
            role: self.offer.role,
            stage,
            cause,
            instance: Some(self.offer.instance),
            retry_after_ms: None,
        }
    }

    /// The next control object; a chunk here is malformed.
    async fn next_control(&mut self) -> Result<Control, Cause> {
        match self.frames.recv().await {
            None => Err(Cause::Exited),
            Some(Err(error)) => Err(wire_cause(&error)),
            Some(Ok(Frame::Data(_))) => Err(Cause::Malformed),
            Some(Ok(Frame::Control(value))) => {
                serde_json::from_value(value).map_err(|_| Cause::Malformed)
            }
        }
    }

    /// Receives the attachments `decls` announce for `request_id`.
    async fn attachments(
        &mut self,
        request_id: u64,
        decls: &[super::wire::AttachmentDecl],
    ) -> Result<Vec<Attachment>, Cause> {
        let mut assembler = Assembler::new(request_id, decls, self.offer.limits.max_attachments)
            .map_err(|error| attachment_cause(&error))?;
        while !assembler.is_complete() {
            match self.frames.recv().await {
                None => return Err(Cause::Exited),
                Some(Err(error)) => return Err(wire_cause(&error)),
                Some(Ok(Frame::Control(_))) => return Err(Cause::Malformed),
                Some(Ok(Frame::Data(chunk))) => assembler
                    .accept(chunk)
                    .map_err(|error| attachment_cause(&error))?,
            }
        }
        assembler.finish().map_err(|error| attachment_cause(&error))
    }

    /// Sends `call` and waits at most `budget` (covering the write, every effect and the reply)
    /// for the reply that echoes its fence. A module's typed error is an ordinary [`Reply`];
    /// exit, stall, malformed or oversized frames, a wrong fence, a broken attachment delivery
    /// and a previously abandoned call poison the channel.
    pub async fn call(
        &mut self,
        call: Call,
        budget: Duration,
        effects: &mut dyn EffectRunner,
    ) -> Result<Reply, ModuleUnavailable> {
        if self.in_flight {
            self.poison(Stage::Request, Cause::Timeout);
        }
        if let Some((stage, cause)) = self.fault {
            return Err(self.poison(stage, cause));
        }
        self.last_request += 1;
        let fence = Fence {
            instance: self.offer.instance,
            request_id: self.last_request,
            scope_key: call.scope_key,
            revision_key: call.revision_key,
        };
        let mut attachments = call.attachments;
        let (payload, body_attachment) = spill(call.payload, &mut attachments);
        let request = Control::Request(Request {
            fence: fence.clone(),
            capability: call.capability,
            capability_version: 0,
            budget_ms: budget.as_millis() as u64,
            payload,
            body_attachment,
            attachments: attachments.iter().map(Attachment::decl).collect(),
        });
        self.in_flight = true;
        let outcome = tokio::time::timeout(
            budget,
            self.exchange(&fence, &request, &attachments, effects),
        )
        .await;
        self.in_flight = false;
        match outcome {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err((stage, cause))) => Err(self.poison(stage, cause)),
            Err(_) => Err(self.poison(Stage::Request, Cause::Timeout)),
        }
    }

    /// Writes the request and its attachments, serves effect calls, and returns the reply.
    async fn exchange(
        &mut self,
        fence: &Fence,
        request: &Control,
        attachments: &[Attachment],
        effects: &mut dyn EffectRunner,
    ) -> Result<Reply, (Stage, Cause)> {
        let send = |error: WireError| (Stage::Request, wire_cause(&error));
        write_control(&mut self.writer, request)
            .await
            .map_err(send)?;
        for attachment in attachments {
            write_attachment(
                &mut self.writer,
                fence.request_id,
                attachment.id,
                &attachment.bytes,
            )
            .await
            .map_err(send)?;
        }
        // Only the latest effect is retained: the module waits for each reply before its next
        // call, so a repeat can only name the latest call, and older output is never held.
        let mut latest: Option<(u32, EffectOutcome, Vec<Attachment>)> = None;
        loop {
            let control = self
                .next_control()
                .await
                .map_err(|cause| (Stage::Request, cause))?;
            match control {
                Control::Response(response) => {
                    if response.fence != *fence {
                        return Err((Stage::Request, Cause::WrongFence));
                    }
                    let mut attachments = self
                        .attachments(fence.request_id, &response.attachments)
                        .await
                        .map_err(|cause| (Stage::Decode, cause))?;
                    let outcome = match response.outcome {
                        Outcome::Result(inline) => Outcome::Result(
                            unspill(inline, response.body_attachment, &mut attachments)
                                .map_err(|()| (Stage::Decode, Cause::Malformed))?,
                        ),
                        error if response.body_attachment.is_none() => error,
                        _ => return Err((Stage::Decode, Cause::Malformed)),
                    };
                    return Ok(Reply {
                        outcome,
                        readiness: response.readiness,
                        coverage: response.coverage,
                        attachments,
                    });
                }
                Control::Effect(effect) => {
                    if effect.fence != *fence {
                        return Err((Stage::Effect, Cause::WrongFence));
                    }
                    let previous = latest.as_ref().map_or(0, |(call, _, _)| *call);
                    if effect.call < previous {
                        return Err((Stage::Effect, Cause::Malformed));
                    }
                    let mut parts = self
                        .attachments(fence.request_id, &effect.attachments)
                        .await
                        .map_err(|cause| (Stage::Effect, cause))?;
                    let request: EffectRequest =
                        unspill(effect.effect, effect.body_attachment, &mut parts)
                            .ok()
                            .and_then(|value| serde_json::from_value(value).ok())
                            .ok_or((Stage::Effect, Cause::Malformed))?;
                    if effect.call > previous {
                        let (outcome, output) = effects.run(fence, request).await;
                        latest = Some((effect.call, outcome, output));
                    }
                    let Some((call, outcome, output)) = &latest else {
                        return Err((Stage::Effect, Cause::Malformed));
                    };
                    let reply = Control::EffectReply(EffectReply {
                        fence: fence.clone(),
                        call: *call,
                        outcome: outcome.clone(),
                        attachments: output.iter().map(Attachment::decl).collect(),
                    });
                    let send = |error: WireError| (Stage::Effect, wire_cause(&error));
                    write_control(&mut self.writer, &reply)
                        .await
                        .map_err(send)?;
                    for attachment in output {
                        write_attachment(
                            &mut self.writer,
                            fence.request_id,
                            attachment.id,
                            &attachment.bytes,
                        )
                        .await
                        .map_err(send)?;
                    }
                }
                _ => return Err((Stage::Request, Cause::Malformed)),
            }
        }
    }

    /// Asks the module to exit; the owner still reaps it.
    pub async fn shutdown(&mut self) {
        let shutdown = Control::Shutdown {
            instance: self.offer.instance,
        };
        let _ = write_control(&mut self.writer, &shutdown).await;
    }
}
