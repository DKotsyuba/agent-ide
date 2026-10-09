//! Module side of `bundled-module/0`: the [`ModuleServer`] trait a language module implements and
//! the [`serve`] loop that runs it over a pair of byte streams (stdin/stdout in the hidden
//! `agent-ide module <language> <role>` mode).
//!
//! The loop answers `hello` from the server's [`Declaration`], then serves one request at a time:
//! it checks the fence (this instance, strictly increasing request ids), assembles the declared
//! attachments, refuses undeclared or unsupported capabilities itself, and writes exactly one
//! response echoing the fence. While a request runs, the server may ask the core to run effect
//! recipes through [`Effects`]; it never spawns them itself. Any transport fault ends the loop.

use std::{collections::HashMap, future::Future};

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};

use super::{
    contract::{
        Capability, Control, Coverage, Declaration, EffectCall, ErrorCode, Fence, HelloOffer,
        ModuleError, Outcome, Readiness, Response, Role, Support, spill, unspill,
    },
    payload::{EffectOutcome, EffectRequest},
    wire::{
        Assembler, Attachment, AttachmentDecl, Frame, WireError, read_frame, write_attachment,
        write_control,
    },
};

/// One request as the server sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct Incoming {
    /// The request's fence (already checked).
    pub fence: Fence,
    /// The declared, supported capability addressed.
    pub capability: Capability,
    /// Remaining allowance in milliseconds.
    pub budget_ms: u64,
    /// The typed payload, still encoded.
    pub payload: Value,
    /// Every declared attachment, complete.
    pub attachments: Vec<Attachment>,
}

impl Incoming {
    /// The attachment with `id`.
    pub fn attachment(&self, id: u32) -> Option<&Attachment> {
        self.attachments
            .iter()
            .find(|attachment| attachment.id == id)
    }
}

/// One answer as the server produces it.
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    /// Result or typed refusal.
    pub outcome: Outcome,
    /// Readiness of the capability.
    pub readiness: Readiness,
    /// Completeness of the result.
    pub coverage: Coverage,
    /// Attachments sent after the response.
    pub attachments: Vec<Attachment>,
}

impl Answer {
    /// A complete, ready result.
    pub fn result(value: Value) -> Self {
        Self {
            outcome: Outcome::Result(value),
            readiness: Readiness::Ready,
            coverage: Coverage::Complete,
            attachments: Vec::new(),
        }
    }

    /// A typed refusal.
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            outcome: Outcome::Error(ModuleError {
                code,
                message: message.into(),
            }),
            readiness: if code == ErrorCode::Warming {
                Readiness::Warming
            } else {
                Readiness::Ready
            },
            coverage: Coverage::Unknown,
            attachments: Vec::new(),
        }
    }
}

/// Why [`serve`] or an [`Effects`] call stopped.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServeError {
    /// A transport fault.
    Wire(WireError),
    /// A well-framed message broke the protocol (wrong type, fence or attachment delivery).
    Protocol(String),
    /// `hello` refused the offer.
    Incompatible(String),
    /// The core cancelled the active request or shut the instance down during an effect.
    Cancelled,
}

impl std::fmt::Display for ServeError {
    /// Writes the cause.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wire(error) => write!(f, "{error}"),
            Self::Protocol(message) => write!(f, "protocol: {message}"),
            Self::Incompatible(message) => write!(f, "incompatible: {message}"),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for ServeError {}

impl From<WireError> for ServeError {
    /// Wraps a transport fault.
    fn from(error: WireError) -> Self {
        Self::Wire(error)
    }
}

/// The byte streams of one instance, boxed so servers stay generic only over themselves.
pub struct ModuleIo {
    /// From the core.
    input: Box<dyn AsyncRead + Send + Unpin>,
    /// To the core.
    output: Box<dyn AsyncWrite + Send + Unpin>,
    /// Attachment ceiling of one message, from `hello`.
    max_attachments: u64,
}

impl ModuleIo {
    /// Reads the next control object and the attachments its `decls` announce.
    async fn read_message(&mut self) -> Result<(Control, Vec<Attachment>), ServeError> {
        let value = match read_frame(&mut self.input).await? {
            Frame::Control(value) => value,
            Frame::Data(_) => return Err(ServeError::Protocol("chunk without a message".into())),
        };
        let control: Control = serde_json::from_value(value)
            .map_err(|error| ServeError::Protocol(error.to_string()))?;
        let (request_id, decls): (u64, &[AttachmentDecl]) = match &control {
            Control::Request(request) => (request.fence.request_id, &request.attachments),
            Control::EffectReply(reply) => (reply.fence.request_id, &reply.attachments),
            _ => (0, &[]),
        };
        let mut assembler = Assembler::new(request_id, decls, self.max_attachments)
            .map_err(|error| ServeError::Protocol(error.to_string()))?;
        while !assembler.is_complete() {
            match read_frame(&mut self.input).await? {
                Frame::Data(chunk) => assembler
                    .accept(chunk)
                    .map_err(|error| ServeError::Protocol(error.to_string()))?,
                Frame::Control(_) => {
                    return Err(ServeError::Protocol("message inside an attachment".into()));
                }
            }
        }
        let attachments = assembler
            .finish()
            .map_err(|error| ServeError::Protocol(error.to_string()))?;
        Ok((control, attachments))
    }

    /// Writes `control` followed by `attachments` of `request_id`.
    async fn write_message(
        &mut self,
        control: &Control,
        request_id: u64,
        attachments: &[Attachment],
    ) -> Result<(), ServeError> {
        write_control(&mut self.output, control).await?;
        for attachment in attachments {
            write_attachment(
                &mut self.output,
                request_id,
                attachment.id,
                &attachment.bytes,
            )
            .await?;
        }
        Ok(())
    }
}

/// The active request's channel to the core's effect runner.
pub struct Effects<'a> {
    /// The instance's streams.
    io: &'a mut ModuleIo,
    /// The active request.
    fence: Fence,
    /// Next call number.
    next: u32,
}

impl Effects<'_> {
    /// Asks the core to run `effect` and waits for its outcome and output attachments (stdout id
    /// 1, stderr id 2 when present). A `cancel` of this request or a `shutdown` ends the request
    /// with [`ServeError::Cancelled`]; anything else unexpected is a protocol error.
    pub async fn run(
        &mut self,
        effect: EffectRequest,
    ) -> Result<(EffectOutcome, Vec<Attachment>), ServeError> {
        self.next += 1;
        let call = self.next;
        let mut attachments = Vec::new();
        let (effect, body_attachment) = spill(
            serde_json::to_value(&effect)
                .map_err(|error| ServeError::Protocol(error.to_string()))?,
            &mut attachments,
        );
        let message = Control::Effect(EffectCall {
            fence: self.fence.clone(),
            call,
            effect,
            body_attachment,
            attachments: attachments.iter().map(Attachment::decl).collect(),
        });
        self.io
            .write_message(&message, self.fence.request_id, &attachments)
            .await?;
        match self.io.read_message().await? {
            (Control::EffectReply(reply), attachments)
                if reply.fence == self.fence && reply.call == call =>
            {
                Ok((reply.outcome, attachments))
            }
            (Control::Cancel(fence), _) if fence == self.fence => Err(ServeError::Cancelled),
            (Control::Shutdown { .. }, _) => Err(ServeError::Cancelled),
            _ => Err(ServeError::Protocol(
                "unexpected message during an effect".into(),
            )),
        }
    }
}

/// What a language module implements behind the [`serve`] loop.
pub trait ModuleServer: Send {
    /// The module's identity and declared capabilities.
    fn declaration(&self) -> Declaration;

    /// Prepares the instance for `offer` (selected configuration, role) before `hello` is
    /// answered; an error refuses the offer. The default accepts.
    fn hello(&mut self, offer: &HelloOffer) -> impl Future<Output = Result<(), String>> + Send {
        let _ = offer;
        async { Ok(()) }
    }

    /// Answers one request of a declared, supported capability. `effects` runs core-owned
    /// effect recipes for this request; an answer must not claim completeness it lacks.
    fn call<'a>(
        &'a mut self,
        request: Incoming,
        effects: Effects<'a>,
    ) -> impl Future<Output = Result<Answer, ServeError>> + Send + 'a;
}

/// Serves one instance of `server` in `role` on `input`/`output` until the core shuts it down or
/// closes the stream (`Ok`), or a fault ends it (`Err`).
pub async fn serve<S: ModuleServer>(
    mut server: S,
    role: Role,
    input: impl AsyncRead + Send + Unpin + 'static,
    output: impl AsyncWrite + Send + Unpin + 'static,
) -> Result<(), ServeError> {
    let mut io = ModuleIo {
        input: Box::new(input),
        output: Box::new(output),
        max_attachments: super::wire::MAX_MESSAGE_ATTACHMENTS,
    };
    let offer = match io.read_message().await? {
        (Control::Hello(offer), _) => offer,
        _ => return Err(ServeError::Protocol("expected hello".into())),
    };
    let declaration = server.declaration();
    let accepted = if offer.role == role {
        match declaration.answer(&offer) {
            Ok(reply) => server.hello(&offer).await.map(|()| reply),
            Err(reason) => Err(reason),
        }
    } else {
        Err(format!(
            "started as {}, offered {}",
            role.name(),
            offer.role.name()
        ))
    };
    let reply = match accepted {
        Ok(reply) => reply,
        Err(reason) => {
            io.write_message(
                &Control::HelloRefused {
                    reason: reason.clone(),
                },
                0,
                &[],
            )
            .await?;
            return Err(ServeError::Incompatible(reason));
        }
    };
    io.max_attachments = offer.limits.max_attachments;
    io.write_message(&Control::HelloReply(reply), 0, &[])
        .await?;
    let supported: HashMap<Capability, Support> = declaration
        .capabilities
        .iter()
        .map(|decl| (decl.capability, decl.support))
        .collect();
    let mut last_request = 0;
    loop {
        let (control, attachments) = match io.read_message().await {
            Err(ServeError::Wire(WireError::Eof)) => return Ok(()),
            other => other?,
        };
        let request = match control {
            Control::Request(request) => request,
            Control::Shutdown { .. } => return Ok(()),
            // A cancel that races the answer it targets has nothing left to stop.
            Control::Cancel(_) => continue,
            _ => return Err(ServeError::Protocol("unexpected message".into())),
        };
        if request.fence.instance != offer.instance || request.fence.request_id <= last_request {
            return Err(ServeError::Protocol("wrong fence".into()));
        }
        last_request = request.fence.request_id;
        let fence = request.fence.clone();
        let answer = if request.capability_version != 0 {
            Answer::error(ErrorCode::InvalidRequest, "unknown capability version")
        } else if supported.get(&request.capability) != Some(&Support::Supported) {
            Answer::error(ErrorCode::Unsupported, "capability not supported")
        } else {
            let mut attachments = attachments;
            let payload = unspill(request.payload, request.body_attachment, &mut attachments)
                .map_err(|()| ServeError::Protocol("bad spilled payload".into()))?;
            let incoming = Incoming {
                fence: fence.clone(),
                capability: request.capability,
                budget_ms: request.budget_ms,
                payload,
                attachments,
            };
            let effects = Effects {
                io: &mut io,
                fence: fence.clone(),
                next: 0,
            };
            match server.call(incoming, effects).await {
                Ok(answer) => answer,
                Err(ServeError::Cancelled) => continue,
                Err(error) => return Err(error),
            }
        };
        let mut attachments = answer.attachments;
        let (outcome, body_attachment) = match answer.outcome {
            Outcome::Result(value) => {
                let (inline, body) = spill(value, &mut attachments);
                (Outcome::Result(inline), body)
            }
            error => (error, None),
        };
        let response = Control::Response(Response {
            fence,
            outcome,
            body_attachment,
            readiness: answer.readiness,
            coverage: answer.coverage,
            attachments: attachments.iter().map(Attachment::decl).collect(),
        });
        io.write_message(&response, last_request, &attachments)
            .await?;
    }
}

/// [`serve`] on the process's stdin and stdout.
pub async fn serve_stdio<S: ModuleServer>(server: S, role: Role) -> Result<(), ServeError> {
    serve(server, role, tokio::io::stdin(), tokio::io::stdout()).await
}

/// A module that declares every capability unsupported: the hidden-mode placeholder until a
/// language's module task supplies its own server.
pub struct Unimplemented(pub Declaration);

impl Unimplemented {
    /// The placeholder for `module_id` at `package_version`.
    pub fn new(module_id: super::contract::ModuleId, package_version: &str) -> Self {
        Self(Declaration {
            module_id,
            package_version: package_version.to_owned(),
            capabilities: Capability::ALL
                .into_iter()
                .map(|capability| {
                    super::contract::CapabilityDecl::v0(capability, Support::Unsupported)
                })
                .collect(),
            linkage_kinds: Vec::new(),
        })
    }
}

impl ModuleServer for Unimplemented {
    /// The placeholder declaration.
    fn declaration(&self) -> Declaration {
        self.0.clone()
    }

    /// Never reached: the loop refuses unsupported capabilities itself.
    async fn call<'a>(
        &'a mut self,
        _request: Incoming,
        _effects: Effects<'a>,
    ) -> Result<Answer, ServeError> {
        Ok(Answer::error(ErrorCode::Unsupported, "not implemented"))
    }
}
