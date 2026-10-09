//! Phase-2b pilot (M-011): a throw-away framed-stdio boundary between the core and one external,
//! read-only module process.
//!
//! Nothing here is a public format and nothing is frozen: the wire is a 4-byte big-endian length
//! followed by one JSON object, every request is `{"id","method","params"}` and every reply
//! `{"id","result"}` or `{"id","error"}`. The core side ([`Channel`]) owns budgets and fences: a
//! module that exits, stalls past its budget, sends a malformed or oversized frame or answers out
//! of order poisons its channel, and the owner replaces the instance on the next call. The module
//! side ([`serve_analyzer`]) hosts an ordinary [`LiveSession`] against its own provider child and
//! answers only read-only methods; every source fact in a reply is re-derived by the core from its
//! own observation, so the module never presents source identity.

use std::{io, path::PathBuf, time::Duration};

use async_lsp::lsp_types as lsp;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::mpsc,
};

use super::{
    context::{ContextMode, ContextQuery, ContextResult},
    freshness::{DiagnosticReadiness, Freshness, ViewGeneration},
    session::{DiagnosticSnapshot, LiveSession, ProviderSettings},
};
use crate::workspace::{
    authority::WorktreeRef,
    observation::{
        ObservationRef, ObservedState, SourceBytes, SourceCoverage, SourceObservation,
        SourceRevision,
    },
};

/// Opt-in flag naming the one language whose analyzer and checker run out of process.
pub const FLAG: &str = "AGENT_IDE_PILOT_MODULE";
/// Optional per-call budget override in milliseconds (default [`DEFAULT_BUDGET`]).
pub const BUDGET_ENV: &str = "AGENT_IDE_PILOT_BUDGET_MS";
/// Test seam `<stall|malformed|oversize|widen>:<method>:<flag file>`: the module misbehaves once
/// on `method`, when it can remove the flag file (`widen` is the checker's escaped write root on
/// `run`). Honoured only in `test-seams` builds.
pub const FAULT_SEAM: &str = "AGENT_IDE_TEST_PILOT_FAULT";
/// Largest accepted frame body, checked before any allocation.
pub const MAX_FRAME: usize = 8 * 1024 * 1024;
/// Per-call budget when [`BUDGET_ENV`] is unset.
const DEFAULT_BUDGET: Duration = Duration::from_secs(20);

/// Whether the pilot flag selects `language` (its lowercase identifier).
pub fn enabled(language: &str) -> bool {
    std::env::var(FLAG).is_ok_and(|value| value == language)
}

/// The per-call budget: [`BUDGET_ENV`] clamped to 100 ms..60 s, else [`DEFAULT_BUDGET`].
pub fn call_budget() -> Duration {
    std::env::var(BUDGET_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|ms| Duration::from_millis(ms.clamp(100, 60_000)))
        .unwrap_or(DEFAULT_BUDGET)
}

/// Writes one frame; a body over [`MAX_FRAME`] is refused before anything is written.
pub async fn write_frame<W: AsyncWrite + Unpin + ?Sized>(
    writer: &mut W,
    value: &Value,
) -> io::Result<()> {
    let body = serde_json::to_vec(value)?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    writer.write_all(&(body.len() as u32).to_be_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await
}

/// Reads one frame; EOF, a length over [`MAX_FRAME`] (rejected before allocating) or a body that
/// is not one JSON object is an error.
pub async fn read_frame<R: AsyncRead + Unpin + ?Sized>(reader: &mut R) -> io::Result<Value> {
    let mut length = [0; 4];
    reader.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized frame",
        ));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    match serde_json::from_slice::<Value>(&body) {
        Ok(value) if value.is_object() => Ok(value),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed frame",
        )),
    }
}

/// Core side of one module instance: sequential calls with a budget and poisoning on any fault.
pub struct Channel {
    /// The module's stdin.
    writer: Box<dyn AsyncWrite + Send + Sync + Unpin>,
    /// Frames (or the terminal read error) from the reader task, in arrival order.
    replies: mpsc::Receiver<io::Result<Value>>,
    /// Last request id sent.
    next_id: u64,
    /// True while a call has not settled; a dropped call leaves it set and poisons the next one.
    in_flight: bool,
    /// The first fault, after which every call fails without touching the module.
    fault: Option<String>,
}

impl Channel {
    /// Wraps the module's stdout/stdin and spawns the reader task, whose completion means the
    /// module's output is gone (the caller's liveness signal; typed as a session driver).
    pub fn spawn<R, W>(
        input: R,
        output: W,
    ) -> (Self, tokio::task::JoinHandle<Result<(), async_lsp::Error>>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + Sync + 'static,
    {
        let (sender, replies) = mpsc::channel(4);
        let reader = tokio::spawn(async move {
            let mut input = input;
            loop {
                let frame = read_frame(&mut input).await;
                let last = frame.is_err();
                if sender.send(frame).await.is_err() || last {
                    return Ok(());
                }
            }
        });
        (
            Self {
                writer: Box::new(output),
                replies,
                next_id: 0,
                in_flight: false,
                fault: None,
            },
            reader,
        )
    }

    /// The fault that poisoned this channel, if any.
    pub fn fault(&self) -> Option<&str> {
        self.fault.as_deref()
    }

    /// Sends one request and waits at most `budget` (covering the write and the reply) for the
    /// reply with the same id. A module `error` reply is an ordinary error; exit, stall,
    /// malformed or out-of-order frames, and a previously abandoned call poison the channel.
    pub async fn call(
        &mut self,
        method: &str,
        params: Value,
        budget: Duration,
    ) -> io::Result<Value> {
        if self.in_flight {
            self.poison("pilot module call abandoned mid-flight".to_owned());
        }
        if let Some(fault) = &self.fault {
            return Err(io::Error::other(fault.clone()));
        }
        self.next_id += 1;
        let id = self.next_id;
        self.in_flight = true;
        let request = json!({"id": id, "method": method, "params": params});
        let outcome = tokio::time::timeout(budget, async {
            write_frame(&mut self.writer, &request).await?;
            self.replies
                .recv()
                .await
                .unwrap_or_else(|| Err(io::ErrorKind::UnexpectedEof.into()))
        })
        .await;
        self.in_flight = false;
        let mut reply = match outcome {
            Err(_) => {
                return Err(self.poison(format!(
                    "pilot module stalled past {} ms",
                    budget.as_millis()
                )));
            }
            Ok(Err(error)) if error.kind() == io::ErrorKind::InvalidData => {
                return Err(self.poison(format!("pilot module sent a bad frame ({error})")));
            }
            Ok(Err(_)) => return Err(self.poison("pilot module exited".to_owned())),
            Ok(Ok(reply)) => reply,
        };
        if reply["id"].as_u64() != Some(id) {
            return Err(self.poison("pilot module answered out of order".to_owned()));
        }
        if let Some(error) = reply.get("error") {
            return Err(io::Error::other(format!(
                "pilot module: {}",
                error.as_str().unwrap_or("error")
            )));
        }
        Ok(reply["result"].take())
    }

    /// Records the first fault and returns it as an error.
    fn poison(&mut self, fault: String) -> io::Error {
        let fault = self.fault.get_or_insert(fault).clone();
        io::Error::other(fault)
    }
}

/// Decodes one reply field, poisoning nothing: a well-framed but ill-typed reply is an error.
pub fn decode<T: serde::de::DeserializeOwned>(value: Value) -> io::Result<T> {
    serde_json::from_value(value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "pilot module reply ill-typed"))
}

/// The one-time misbehaviour the fault seam selects for `method`, consumed when its flag file
/// can be removed.
pub fn fault_seam(method: &str) -> Option<String> {
    let value = crate::test_seams::var(FAULT_SEAM)?;
    let mut parts = value.splitn(3, ':');
    let (kind, target, flag) = (parts.next()?, parts.next()?, parts.next()?);
    (target == method && std::fs::remove_file(flag).is_ok()).then(|| kind.to_owned())
}

/// Acts out a seam-selected fault on `writer`: `stall` never answers, `malformed` sends a
/// non-JSON body, `oversize` announces a frame over [`MAX_FRAME`].
pub async fn act_fault<W: AsyncWrite + Unpin + ?Sized>(
    kind: &str,
    writer: &mut W,
) -> io::Result<()> {
    match kind {
        "malformed" => writer.write_all(b"\0\0\0\x05{not}").await?,
        "oversize" => writer.write_all(&u32::MAX.to_be_bytes()).await?,
        _ => tokio::time::sleep(Duration::from_secs(3600)).await,
    }
    writer.flush().await
}

/// Worktree identity the core sends in `hello`; the module rebuilds an unverified reference with
/// it, so module-side identities are lookup keys only.
#[derive(Serialize, Deserialize)]
pub struct WireWorktree {
    /// Absolute worktree root.
    pub path: PathBuf,
    /// Absolute repository root.
    pub repository_root: PathBuf,
    /// Raw Git common directory.
    pub git_common_dir: PathBuf,
    /// Positive lifecycle incarnation.
    pub incarnation: u64,
}

impl WireWorktree {
    /// Describes `worktree` for the module.
    pub fn of(worktree: &WorktreeRef) -> Self {
        Self {
            path: worktree.worktree_path().to_path_buf(),
            repository_root: worktree.repository_root().to_path_buf(),
            git_common_dir: worktree.git_common_dir().to_path_buf(),
            incarnation: worktree.incarnation(),
        }
    }
}

/// One source observation as the module needs it: the core's path, sequence, references and
/// exact text (absent for a missing path). The module re-derives the digest from the text.
#[derive(Serialize, Deserialize)]
pub(crate) struct WireSource {
    /// Worktree-relative path.
    path: PathBuf,
    /// Core-allocated monotonic sequence.
    sequence: u64,
    /// Observation correlation reference.
    reference: String,
    /// Opaque source revision.
    revision: String,
    /// Exact UTF-8 text, `None` for an observed missing path.
    text: Option<String>,
}

impl WireSource {
    /// Describes `observation` with its already UTF-8-validated `text`.
    pub(crate) fn of(observation: &SourceObservation, text: &str) -> Self {
        Self {
            path: observation.path().to_path_buf(),
            sequence: observation.sequence(),
            reference: observation.reference().as_str().to_owned(),
            revision: observation.source_revision().as_str().to_owned(),
            text: observation.bytes().map(|_| text.to_owned()),
        }
    }

    /// Rebuilds the observation inside the module, scoped to the module's own worktree and epoch.
    fn observation(
        &self,
        worktree: &WorktreeRef,
        epoch: u64,
    ) -> io::Result<(SourceObservation, Vec<u8>)> {
        let bytes = self.text.clone().unwrap_or_default().into_bytes();
        let present = self.text.is_some();
        let observation = SourceObservation::new(
            worktree.clone(),
            epoch,
            self.sequence,
            ObservationRef::new(self.reference.clone()).map_err(|_| bad_params())?,
            self.path.clone(),
            present.then(|| SourceBytes::from_bytes(&bytes)),
            SourceRevision::new(self.revision.clone()).map_err(|_| bad_params())?,
            SourceCoverage::Complete,
            if present {
                ObservedState::Present
            } else {
                ObservedState::Missing
            },
        )
        .map_err(|_| bad_params())?;
        Ok((observation, bytes))
    }
}

/// Provider evidence of one context exchange; source facts are re-derived by the core.
#[derive(Serialize, Deserialize)]
pub(crate) struct WireContext {
    /// The generation the module's session fenced the result with (the one `hello` gave it).
    pub(crate) generation: Option<[u64; 4]>,
    /// Synchronized document version.
    pub(crate) document_version: Option<i32>,
    /// Negotiated position encoding of the semantic locations.
    pub(crate) position_encoding: lsp::PositionEncodingKind,
    /// `None` for semantic evidence, the lexical reason otherwise.
    pub(crate) lexical: Option<String>,
    /// Provider definitions.
    pub(crate) definitions: Option<Vec<lsp::Location>>,
    /// Provider references.
    pub(crate) references: Option<Vec<lsp::Location>>,
    /// Whether a location ceiling omitted data.
    pub(crate) truncated: bool,
}

impl WireContext {
    /// Captures the provider parts of `result`.
    fn of(result: &ContextResult) -> Self {
        Self {
            generation: result.generation.map(|generation| {
                [
                    generation.backend,
                    generation.configuration,
                    generation.toolchain,
                    generation.view,
                ]
            }),
            document_version: result.document_version,
            position_encoding: result.position_encoding.clone(),
            lexical: match &result.mode {
                ContextMode::Semantic => None,
                ContextMode::Lexical { reason } => Some(reason.clone()),
            },
            definitions: result.definitions.clone(),
            references: result.references.clone(),
            truncated: result.truncated,
        }
    }

    /// Overlays the provider parts onto `result`, a core-built lexical result over the core's own
    /// observation. A module generation other than the core's `generation` is refused as
    /// invalid data (the caller retires the session) and leaves `result` unchanged.
    pub(crate) fn apply(
        self,
        result: &mut ContextResult,
        generation: ViewGeneration,
    ) -> io::Result<()> {
        let expected = [
            generation.backend,
            generation.configuration,
            generation.toolchain,
            generation.view,
        ];
        if self.generation.is_some_and(|echoed| echoed != expected) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "pilot module answered for another generation",
            ));
        }
        result.generation = self.generation.map(|_| generation);
        result.document_version = self.document_version;
        result.truncated = self.truncated;
        match self.lexical {
            None => {
                result.mode = ContextMode::Semantic;
                result.position_encoding = self.position_encoding;
                result.lexical_matches.clear();
            }
            Some(reason) => result.mode = ContextMode::Lexical { reason },
        }
        result.definitions = self.definitions;
        result.references = self.references;
        Ok(())
    }
}

/// Diagnostics evidence; the core binds it to its own observation when the sequence matches.
#[derive(Serialize, Deserialize)]
pub(crate) struct WireDiagnostics {
    /// Sequence of the source the module's push was bound to.
    pub(crate) source_sequence: Option<u64>,
    /// Published document version.
    pub(crate) document_version: Option<i32>,
    /// `clean`, `reported` or `unknown`.
    pub(crate) readiness: String,
    /// `current`, `stale`, `provisional` or `unknown`.
    pub(crate) freshness: String,
    /// At most 128 diagnostics.
    pub(crate) diagnostics: Vec<lsp::Diagnostic>,
    /// Whether the count ceiling omitted items.
    pub(crate) truncated: bool,
}

impl WireDiagnostics {
    /// Captures `snapshot`.
    fn of(snapshot: &DiagnosticSnapshot) -> Self {
        Self {
            source_sequence: snapshot.source.as_ref().map(|source| source.sequence()),
            document_version: snapshot.document_version,
            readiness: match snapshot.readiness {
                DiagnosticReadiness::Clean => "clean",
                DiagnosticReadiness::Reported => "reported",
                DiagnosticReadiness::Unknown => "unknown",
            }
            .to_owned(),
            freshness: match snapshot.freshness {
                Freshness::Current => "current",
                Freshness::Stale => "stale",
                Freshness::Provisional => "provisional",
                Freshness::Unknown => "unknown",
            }
            .to_owned(),
            diagnostics: snapshot.diagnostics.clone(),
            truncated: snapshot.truncated,
        }
    }

    /// Writes the evidence into `snapshot`, bound to `observation` only when the module's push
    /// was bound to the same sequence; anything else stays unknown.
    pub(crate) fn apply(self, snapshot: &mut DiagnosticSnapshot, observation: &SourceObservation) {
        let bound = self.source_sequence == Some(observation.sequence());
        snapshot.source =
            bound.then(|| super::freshness::SourceBinding::from_observation(observation));
        snapshot.document_version = self.document_version.filter(|_| bound);
        snapshot.readiness = match (bound, self.readiness.as_str()) {
            (true, "clean") => DiagnosticReadiness::Clean,
            (true, "reported") => DiagnosticReadiness::Reported,
            _ => DiagnosticReadiness::Unknown,
        };
        snapshot.freshness = match (bound, self.freshness.as_str()) {
            (true, "current") => Freshness::Current,
            (true, "provisional") => Freshness::Provisional,
            (true, "stale") => Freshness::Stale,
            _ => Freshness::Unknown,
        };
        snapshot.diagnostics = if bound { self.diagnostics } else { Vec::new() };
        snapshot.diagnostics.truncate(128);
        snapshot.truncated = bound && self.truncated;
    }
}

/// The `hello` parameters of an analyzer module.
#[derive(Serialize, Deserialize)]
pub struct AnalyzerHello {
    /// The core's worktree.
    pub worktree: WireWorktree,
    /// Authority epoch the session opens with.
    pub epoch: u64,
    /// The core-minted generation fences; the module echoes them in every context reply and the
    /// core refuses a reply carrying any other generation.
    pub generation: [u64; 4],
    /// Per-request LSP deadline inside the module, milliseconds.
    pub request_timeout_ms: u64,
    /// Language-specific provider parameters, opaque to the core.
    pub provider: Value,
}

/// Rejected request parameters.
fn bad_params() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid pilot parameters")
}

/// Module side of an analyzer: reads `hello` from stdin, lets `build` turn its provider
/// parameters into a profile and a provider command, starts that child with piped stdio, opens
/// an ordinary [`LiveSession`] on it and answers read-only requests until stdin closes.
///
/// `rename` and every unknown method answer an error; the provider child is killed on return.
pub async fn serve_analyzer<F>(build: F) -> io::Result<()>
where
    F: FnOnce(&Value) -> io::Result<(ProviderSettings, tokio::process::Command)>,
{
    let mut input = tokio::io::stdin();
    let mut output = tokio::io::stdout();
    let hello = read_frame(&mut input).await?;
    let id = hello["id"].clone();
    let params: AnalyzerHello = decode(hello["params"].clone())?;
    let worktree = WorktreeRef::from_discovery(
        params.worktree.path,
        params.worktree.repository_root,
        params.worktree.git_common_dir,
        params.worktree.incarnation,
    )
    .map_err(|_| bad_params())?;
    let (settings, mut command) = build(&params.provider)?;
    let mut child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(io::Error::other("provider pipes missing"));
    };
    let [backend, configuration, toolchain, view] = params.generation;
    let mut live = LiveSession::open(
        stdout,
        stdin,
        worktree.clone(),
        params.epoch,
        ViewGeneration {
            backend,
            configuration,
            toolchain,
            view,
        },
        settings,
        Duration::from_millis(params.request_timeout_ms.clamp(1, 60_000)),
    )
    .await?;
    let capabilities = live.session.capabilities();
    write_frame(
        &mut output,
        &json!({"id": id, "result": {
            "advertised": capabilities.advertised,
            "position_encoding": capabilities.position_encoding,
            "server_info": capabilities.server_info,
        }}),
    )
    .await?;
    while let Ok(request) = read_frame(&mut input).await {
        let method = request["method"].as_str().unwrap_or_default().to_owned();
        if method == "shutdown" {
            write_frame(&mut output, &json!({"id": request["id"], "result": null})).await?;
            break;
        }
        if let Some(kind) = fault_seam(&method) {
            act_fault(&kind, &mut output).await?;
            continue;
        }
        let reply = match answer(
            &mut live,
            &worktree,
            params.epoch,
            &method,
            &request["params"],
        )
        .await
        {
            Ok(result) => json!({"id": request["id"], "result": result}),
            Err(error) => json!({"id": request["id"], "error": error.to_string()}),
        };
        write_frame(&mut output, &reply).await?;
        // A dead provider makes this instance useless: exit so the core's next call sees EOF,
        // retires the instance and starts a fresh one.
        if !live.is_alive() {
            break;
        }
    }
    let _ = live.shutdown().await;
    let _ = child.kill().await;
    Ok(())
}

/// Answers one read-only request on the module's own session.
async fn answer(
    live: &mut LiveSession,
    worktree: &WorktreeRef,
    epoch: u64,
    method: &str,
    params: &Value,
) -> io::Result<Value> {
    let session = &mut live.session;
    let source = || -> io::Result<(SourceObservation, Vec<u8>)> {
        decode::<WireSource>(params["source"].clone())?.observation(worktree, epoch)
    };
    let offset = || {
        params["byte_offset"]
            .as_u64()
            .map(|offset| offset as usize)
            .ok_or_else(bad_params)
    };
    let value = match method {
        "context" => {
            let (observation, bytes) = source()?;
            let query = match params["byte_offset"].as_u64() {
                Some(offset) => ContextQuery::Symbol {
                    byte_offset: offset as usize,
                },
                None => ContextQuery::File,
            };
            let result = session.context(&observation, &bytes, query).await?;
            session.wait_for_matching_diagnostics().await;
            json!({
                "context": WireContext::of(&result),
                "diagnostics": WireDiagnostics::of(&session.diagnostics()),
            })
        }
        "document_symbols" => {
            let (observation, bytes) = source()?;
            json!(session.document_symbols(&observation, &bytes).await?)
        }
        "hover" => {
            let (observation, bytes) = source()?;
            json!(session.hover(&observation, &bytes, offset()?).await?)
        }
        "definitions" => {
            let (observation, bytes) = source()?;
            json!(session.definitions(&observation, &bytes, offset()?).await?)
        }
        "references" => {
            let (observation, bytes) = source()?;
            json!(session.references(&observation, &bytes, offset()?).await?)
        }
        "prepare_call_hierarchy" => {
            let (observation, bytes) = source()?;
            json!(
                session
                    .prepare_call_hierarchy(&observation, &bytes, offset()?)
                    .await?
            )
        }
        "incoming_calls" => {
            let (observation, bytes) = source()?;
            json!(
                session
                    .incoming_calls(&observation, &bytes, offset()?)
                    .await?
            )
        }
        "outgoing_calls" => {
            let (observation, bytes) = source()?;
            json!(
                session
                    .outgoing_calls(&observation, &bytes, offset()?)
                    .await?
            )
        }
        "incoming_calls_for" => {
            let item: lsp::CallHierarchyItem = decode(params["item"].clone())?;
            json!(session.incoming_calls_for(item).await?)
        }
        "workspace_symbols" => {
            let query = params["query"].as_str().ok_or_else(bad_params)?;
            json!(session.workspace_symbols(query).await?)
        }
        _ => return Err(io::Error::other(format!("unsupported method {method}"))),
    };
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames round-trip; oversized, malformed and non-object bodies are refused.
    #[tokio::test]
    async fn frames_round_trip_and_refuse_bad_bodies() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &json!({"id": 1})).await.unwrap();
        assert_eq!(
            read_frame(&mut buffer.as_slice()).await.unwrap(),
            json!({"id": 1})
        );
        let oversized = (MAX_FRAME as u32 + 1).to_be_bytes();
        let error = read_frame(&mut oversized.as_slice()).await.unwrap_err();
        assert_eq!(error.to_string(), "oversized frame");
        let error = read_frame(&mut &b"\0\0\0\x05{not}"[..]).await.unwrap_err();
        assert_eq!(error.to_string(), "malformed frame");
        let error = read_frame(&mut &b"\0\0\0\x017"[..]).await.unwrap_err();
        assert_eq!(error.to_string(), "malformed frame");
        let error = read_frame(&mut &b"\0\0"[..]).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    /// A scripted in-memory module body: reads requests from its first stream, answers on the
    /// second.
    type Script = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

    /// Starts a channel against an in-memory module running `script`.
    fn scripted(
        script: impl FnOnce(tokio::io::DuplexStream, tokio::io::DuplexStream) -> Script,
    ) -> Channel {
        let (core_out, module_in) = tokio::io::duplex(1 << 16);
        let (module_out, core_in) = tokio::io::duplex(1 << 16);
        tokio::spawn(script(module_in, module_out));
        Channel::spawn(core_in, core_out).0
    }

    /// A good reply answers; an error reply is an error without poisoning; EOF poisons for good.
    #[tokio::test]
    async fn replies_errors_and_exit() {
        let mut channel = scripted(|mut input, mut output| {
            Box::pin(async move {
                let request = read_frame(&mut input).await.unwrap();
                write_frame(&mut output, &json!({"id": request["id"], "result": 7}))
                    .await
                    .unwrap();
                let request = read_frame(&mut input).await.unwrap();
                write_frame(&mut output, &json!({"id": request["id"], "error": "no"}))
                    .await
                    .unwrap();
            })
        });
        let budget = Duration::from_secs(5);
        assert_eq!(
            channel.call("a", json!({}), budget).await.unwrap(),
            json!(7)
        );
        let error = channel.call("b", json!({}), budget).await.unwrap_err();
        assert_eq!(error.to_string(), "pilot module: no");
        assert!(channel.fault().is_none());
        let error = channel.call("c", json!({}), budget).await.unwrap_err();
        assert_eq!(error.to_string(), "pilot module exited");
        let error = channel.call("d", json!({}), budget).await.unwrap_err();
        assert_eq!(error.to_string(), "pilot module exited");
    }

    /// A stall past the budget, a malformed frame, an oversized announcement and a wrong id each
    /// poison the channel with their own cause.
    #[tokio::test]
    async fn faults_poison_with_their_cause() {
        for (kind, cause) in [
            ("stall", "pilot module stalled past 200 ms"),
            (
                "malformed",
                "pilot module sent a bad frame (malformed frame)",
            ),
            (
                "oversize",
                "pilot module sent a bad frame (oversized frame)",
            ),
            ("wrong-id", "pilot module answered out of order"),
        ] {
            let mut channel = scripted(move |mut input, mut output| {
                Box::pin(async move {
                    let request = read_frame(&mut input).await.unwrap();
                    if kind == "wrong-id" {
                        let id = request["id"].as_u64().unwrap() + 1;
                        write_frame(&mut output, &json!({"id": id, "result": 1}))
                            .await
                            .unwrap();
                    } else {
                        act_fault(kind, &mut output).await.unwrap();
                    }
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                })
            });
            let error = channel
                .call("m", json!({}), Duration::from_millis(200))
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), cause, "{kind}");
            assert_eq!(channel.fault(), Some(cause), "{kind}");
        }
    }

    /// A call dropped before its reply (caller cancellation) poisons the next call, so a late
    /// reply can never answer a later request.
    #[tokio::test]
    async fn an_abandoned_call_poisons_the_next() {
        let mut channel = scripted(|mut input, mut output| {
            Box::pin(async move {
                let request = read_frame(&mut input).await.unwrap();
                tokio::time::sleep(Duration::from_millis(300)).await;
                write_frame(&mut output, &json!({"id": request["id"], "result": 1}))
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_secs(3600)).await;
            })
        });
        let budget = Duration::from_secs(5);
        let abandoned = tokio::time::timeout(
            Duration::from_millis(50),
            channel.call("a", json!({}), budget),
        )
        .await;
        assert!(abandoned.is_err());
        let error = channel.call("b", json!({}), budget).await.unwrap_err();
        assert_eq!(error.to_string(), "pilot module call abandoned mid-flight");
    }

    /// Measurement, not a contract: round trips of one framed call through real OS pipes to
    /// `/bin/cat` (which echoes the request frame, a valid reply with the same id) for several
    /// payload sizes; prints one JSON line per size with raw-sample percentiles in microseconds.
    #[tokio::test]
    #[ignore = "measurement of the pilot channel over OS pipes"]
    async fn channel_round_trip_over_pipes() {
        for size in [64usize, 4 * 1024, 64 * 1024, 1024 * 1024] {
            let mut child = tokio::process::Command::new("/bin/cat")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let (input, output) = (child.stdout.take().unwrap(), child.stdin.take().unwrap());
            let mut channel = Channel::spawn(input, output).0;
            let payload = json!({"text": "x".repeat(size)});
            let budget = Duration::from_secs(5);
            for _ in 0..100 {
                channel.call("echo", payload.clone(), budget).await.unwrap();
            }
            let mut micros = Vec::new();
            for _ in 0..2000 {
                let started = std::time::Instant::now();
                channel.call("echo", payload.clone(), budget).await.unwrap();
                micros.push(started.elapsed().as_secs_f64() * 1e6);
            }
            micros.sort_by(f64::total_cmp);
            let at = |q: f64| micros[((micros.len() - 1) as f64 * q).round() as usize];
            println!(
                "{}",
                json!({"payload_bytes": size, "n": micros.len(), "warmup": 100,
                       "p50_us": at(0.5), "p95_us": at(0.95), "max_us": at(1.0)})
            );
        }
    }
}
