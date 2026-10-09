//! Module-side hosting of a language's provider: a [`LiveSession`] on the module's own provider
//! child, answered as product DTOs. Wraps the language's [`SupportServer`] for everything else.
//!
//! The provider runs only from the one pinned launch specification the core granted in `hello`
//! ([`ProviderGrant`]): the module re-measures every accepted file before it starts the child in
//! its own process group, drains the child's stderr into a bounded buffer and starts it at most
//! once per instance. A dead provider answers the typed `unavailable` refusal at stage
//! `provider` from then on; recovery is the core supervisor's restart of the whole instance.
//!
//! No core identity is rebuilt here: the session's worktree and source bookkeeping are this
//! module's own local keys (its working directory, a local sequence); the core's revision string
//! is only echoed back in locations. Positions become scope-relative UTF-8 byte ranges, a file the
//! request did not carry is read from disk (its range then carries no revision).

use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_lsp::lsp_types as lsp;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    adapter::SupportServer,
    contract::{
        Capability, CapabilityDecl, Cause, Coverage, Declaration, ErrorCode, HelloOffer, Readiness,
        Stage, Support,
    },
    payload::{
        Call, CallItem, CallsQuery, ContextEvidence, Diagnostic, DiagnosticsEvidence, EditProposal,
        FileEdit, Hover, Location, OutlineRequest, RenameAnswer, RenameRequest, Replacement,
        SemanticQuery, SourceRef, SourceText, WorkspaceSymbol, decode, encode,
    },
    serve::{Answer, Effects, Incoming, ModuleServer, ServeError},
};
use crate::{
    execution::{CommandKind, ControlledCommand},
    intelligence::{
        context::{ContextMode, ContextQuery},
        freshness::{DiagnosticReadiness, Freshness, ViewGeneration},
        session::{LiveSession, ProviderSettings, ReadinessError, Session},
    },
    lang::{edits::byte_offset, kind_of},
    workspace::{
        authority::WorktreeRef,
        observation::{
            ObservationRef, ObservedState, SourceBytes, SourceCoverage, SourceObservation,
            SourceRevision,
        },
    },
};

/// Retained provider stderr bytes (the most recent ones).
const PROVIDER_STDERR: usize = 64 * 1024;

/// The part of a readiness query's budget that carries the answer back: the core asks with its
/// own wait plus this margin, and the hosted provider's barrier waits the budget less it, so a
/// module answers when an in-process wait would.
pub(crate) const READINESS_MARGIN_MS: u64 = 500;

/// The provider launch the core granted this instance (`hello.config.provider.grant`): the
/// declaration's accepted executables and files with their digests. Only these may run or be
/// loaded as the provider; the module computes the arguments and environment itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderGrant {
    /// Accepted executables and files (path, BLAKE3 hex).
    pub accepted: Vec<(PathBuf, String)>,
    /// Per-request provider deadline.
    pub request_timeout_ms: u64,
    /// Roots the core admits for the provider's own paths beside the worktree and the accepted
    /// files' directories (a private cache namespace, toolchain and tool homes from the launcher
    /// declaration).
    #[serde(default)]
    pub roots: Vec<PathBuf>,
}

/// How a language starts its provider from the granted files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderLaunchPlan {
    /// The program; it must be one of the grant's accepted files.
    pub program: PathBuf,
    /// Exact arguments.
    pub args: Vec<String>,
    /// The complete environment; nothing else is inherited.
    pub env: BTreeMap<String, String>,
    /// Further files the program loads (a provider script); each must be accepted.
    pub reads: Vec<PathBuf>,
}

impl ProviderGrant {
    /// Checks a plan before anything starts: the program and every file it loads must be
    /// accepted with an unchanged digest, and every absolute path in its arguments and
    /// environment (each `:`-separated part) must lie in `worktree`, a granted root or the
    /// directory of an accepted file. Returns the Execution command, its executable measured.
    fn admit(&self, worktree: &Path, plan: &ProviderLaunchPlan) -> io::Result<ControlledCommand> {
        let denied = |what: &str| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("provider not accepted: {what}"),
            )
        };
        let accepted = |path: &PathBuf| {
            self.accepted
                .iter()
                .find(|(accepted, _)| accepted == path)
                .map(|(_, digest)| digest.as_str())
        };
        for path in &plan.reads {
            let digest = accepted(path).ok_or_else(|| denied("read"))?;
            let bytes = std::fs::read(path)
                .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "provider file missing"))?;
            if blake3::hash(&bytes).to_hex().as_str() != digest {
                return Err(denied("read changed"));
            }
        }
        let inside = |part: &str| {
            let path = Path::new(part);
            !path.is_absolute()
                || (path
                    .components()
                    .all(|part| !matches!(part, std::path::Component::ParentDir))
                    && (path.starts_with(worktree)
                        || self.roots.iter().any(|root| path.starts_with(root))
                        || self.accepted.iter().any(|(file, _)| {
                            file.parent().is_some_and(|dir| path.starts_with(dir))
                        })))
        };
        for value in plan.args.iter().chain(plan.env.values()) {
            if !value.split(':').all(inside) {
                return Err(denied("path outside the grant"));
            }
        }
        let digest = accepted(&plan.program).ok_or_else(|| denied("program"))?;
        let digest = blake3::Hash::from_hex(digest).map_err(|_| denied("program digest"))?;
        let command = ControlledCommand::from_validated_peer(
            CommandKind::Provider,
            plan.program.clone(),
            plan.args.iter().map(std::ffi::OsString::from).collect(),
            worktree.to_path_buf(),
            plan.env
                .iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        )
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "provider program unavailable"))?;
        if !command.has_program_digest(&digest) {
            return Err(denied("program changed"));
        }
        Ok(command)
    }
}

/// What a language supplies to host its provider: from the module's `worktree` and the
/// language-specific `settings` value of `hello.config.provider`, the session settings (profile)
/// and the launch plan. The language computes its own facts here (an interpreter, import roots).
pub trait ProviderBuilder: Send + 'static {
    /// Builds both; an error is a deterministic configuration refusal.
    fn plan(
        &self,
        worktree: &Path,
        settings: &Value,
    ) -> io::Result<(ProviderSettings, ProviderLaunchPlan)>;

    /// Adjusts the provider's diagnostics before they leave the module, as the language's
    /// in-process backend does (a summary of a flood it explains once); the default keeps them.
    fn diagnostics(&self, snapshot: &mut crate::intelligence::session::DiagnosticSnapshot) {
        let _ = snapshot;
    }

    /// Whether a context answer first waits (at most [`DIAGNOSTICS_WAIT`], within the request's
    /// budget) for the provider's diagnostics of the synchronized text, as the language's
    /// in-process backend does: by default every context does; a language that waits only for
    /// the whole-file read (the post-edit diagnostic read) answers `whole_file`.
    fn waits_for_diagnostics(&self, whole_file: bool) -> bool {
        let _ = whole_file;
        true
    }
}

/// The longest a context answer waits for the provider's diagnostics, as in process.
const DIAGNOSTICS_WAIT: Duration = Duration::from_secs(3);

/// How long a context answer may still wait for diagnostics: `None` when the language does not
/// wait, else [`DIAGNOSTICS_WAIT`] cut to the request's remaining budget less the answer margin,
/// so a provider that never publishes cannot outlast the core's deadline.
fn diagnostics_wait(waits: bool, budget_ms: u64, elapsed: Duration) -> Option<Duration> {
    waits.then(|| {
        DIAGNOSTICS_WAIT.min(
            Duration::from_millis(budget_ms.saturating_sub(READINESS_MARGIN_MS))
                .saturating_sub(elapsed),
        )
    })
}

/// A started provider.
struct Hosted {
    /// The session.
    live: LiveSession,
    /// The provider child, killed on drop; it shares the module's process group.
    _child: tokio::process::Child,
}

/// Serves a language's support and its hosted provider.
pub struct ProviderServer<B: ProviderBuilder> {
    /// The language's support adapter.
    support: SupportServer,
    /// Builds the provider settings.
    builder: B,
    /// The granted launch.
    grant: Option<ProviderGrant>,
    /// The language-specific settings value.
    settings: Value,
    /// The hosted provider, started on first demand.
    hosted: Option<Hosted>,
    /// The provider was started once and is gone; this instance never starts another.
    spent: bool,
    /// Why the one start failed, answered again by every later provider request.
    start_failure: Option<(io::ErrorKind, String)>,
    /// The provider's most recent stderr bytes (private diagnostics only).
    stderr: Arc<Mutex<Vec<u8>>>,
    /// Local source sequence.
    sequence: u64,
    /// The last observed source (path, revision, text): an unchanged source keeps its sequence,
    /// so the provider document is not re-synchronized for every request.
    observed: Option<(PathBuf, String, Option<String>)>,
    /// Call hierarchy items by handle, valid for this instance.
    items: HashMap<String, lsp::CallHierarchyItem>,
    /// This module's working directory (its local worktree key).
    root: PathBuf,
}

/// Local session keys: they identify nothing outside this module.
const LOCAL_EPOCH: u64 = 1;
/// Local generation of the hosted session.
const LOCAL_GENERATION: ViewGeneration = ViewGeneration {
    backend: 1,
    configuration: 1,
    toolchain: 1,
    view: 1,
};

impl<B: ProviderBuilder> ProviderServer<B> {
    /// The server for `support`'s language with `builder`, rooted at the working directory.
    pub fn new(support: SupportServer, builder: B) -> Self {
        Self {
            support,
            builder,
            grant: None,
            settings: Value::Null,
            hosted: None,
            spent: false,
            start_failure: None,
            stderr: Arc::default(),
            sequence: 0,
            observed: None,
            items: HashMap::new(),
            root: std::env::current_dir()
                .and_then(|dir| dir.canonicalize())
                .unwrap_or_default(),
        }
    }

    /// The module's local worktree key.
    fn worktree(&self) -> io::Result<WorktreeRef> {
        WorktreeRef::from_discovery(
            self.root.clone(),
            self.root.clone(),
            self.root.join(".git"),
            1,
        )
        .map_err(|_| io::Error::other("module working directory is not absolute"))
    }

    /// The live session, started once from the grant; a spent provider is never replaced.
    async fn session(&mut self) -> io::Result<&mut Session> {
        if self
            .hosted
            .as_ref()
            .is_some_and(|hosted| !hosted.live.is_alive())
        {
            self.hosted = None;
            self.spent = true;
        }
        if self.spent {
            return Err(match &self.start_failure {
                Some((kind, message)) => io::Error::new(*kind, message.clone()),
                None => io::Error::new(io::ErrorKind::BrokenPipe, "provider exited"),
            });
        }
        if self.hosted.is_none() {
            let grant = self
                .grant
                .clone()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no provider granted"))?;
            self.spent = true;
            let started = self.start(grant).await;
            if let Err(error) = &started {
                self.start_failure = Some((error.kind(), error.to_string()));
            }
            started?;
        }
        Ok(&mut self.hosted.as_mut().expect("started above").live.session)
    }

    /// Plans, admits and starts the granted provider and opens its session.
    async fn start(&mut self, grant: ProviderGrant) -> io::Result<()> {
        {
            let (settings, plan) = self.builder.plan(&self.root, &self.settings)?;
            let command = grant.admit(&self.root, &plan)?;
            let mut child = crate::execution::spawn_granted_provider(&command)
                .map_err(|error| io::Error::other(format!("{error:?}")))?;
            let (Some(stdin), Some(stdout), Some(mut stderr)) =
                (child.stdin.take(), child.stdout.take(), child.stderr.take())
            else {
                return Err(io::Error::other("provider pipes missing"));
            };
            let retained = self.stderr.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buffer = [0u8; 8192];
                while let Ok(read) = stderr.read(&mut buffer).await
                    && read > 0
                {
                    let mut retained = retained
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    retained.extend_from_slice(&buffer[..read]);
                    let excess = retained.len().saturating_sub(PROVIDER_STDERR);
                    retained.drain(..excess);
                }
            });
            let live = LiveSession::open(
                stdout,
                stdin,
                self.worktree()?,
                LOCAL_EPOCH,
                LOCAL_GENERATION,
                settings,
                Duration::from_millis(grant.request_timeout_ms.clamp(1, 60_000)),
            )
            .await?;
            self.spent = false;
            self.hosted = Some(Hosted {
                live,
                _child: child,
            });
            // A started provider is spent once it dies: mark it now, clear above on death.
        }
        Ok(())
    }

    /// The exact text of `source`.
    fn text(source: &SourceRef, request: &Incoming) -> io::Result<Option<String>> {
        match &source.text {
            SourceText::Inline(text) => Ok(Some(text.clone())),
            SourceText::Missing => Ok(None),
            SourceText::Attachment(id) => {
                let attachment = request
                    .attachment(*id)
                    .ok_or_else(|| io::Error::other("source attachment missing"))?;
                String::from_utf8(attachment.bytes.clone())
                    .map(Some)
                    .map_err(|_| io::Error::other("source is not UTF-8"))
            }
        }
    }

    /// The local session observation of `source`, under the next local sequence unless it is the
    /// source last observed; only the core's revision string is carried, to be echoed back.
    fn observe(
        &mut self,
        source: &SourceRef,
        request: &Incoming,
    ) -> io::Result<(SourceObservation, Vec<u8>)> {
        let text = Self::text(source, request)?;
        let current = (source.path.clone(), source.revision.clone(), text.clone());
        if self.observed.as_ref() != Some(&current) {
            self.sequence += 1;
            self.observed = Some(current);
        }
        let bytes = text.clone().unwrap_or_default().into_bytes();
        let observation = SourceObservation::new(
            self.worktree()?,
            LOCAL_EPOCH,
            self.sequence,
            ObservationRef::new(format!("local-{}", self.sequence))
                .map_err(|_| io::Error::other("observation ref"))?,
            source.path.clone(),
            text.as_ref().map(|_| SourceBytes::from_bytes(&bytes)),
            SourceRevision::new(source.revision.clone())
                .map_err(|_| io::Error::other("revision"))?,
            SourceCoverage::Complete,
            if text.is_some() {
                ObservedState::Present
            } else {
                ObservedState::Missing
            },
        )
        .map_err(|_| io::Error::other("invalid source"))?;
        Ok((observation, bytes))
    }

    /// Answers one provider-backed request.
    async fn provider_answer(&mut self, request: &Incoming) -> io::Result<Value> {
        let payload = request.payload.clone();
        let invalid = |error: String| io::Error::new(io::ErrorKind::InvalidInput, error);
        match request.capability {
            Capability::Outline => {
                let query: OutlineRequest = decode(payload).map_err(invalid)?;
                let (observation, bytes) = self.observe(&query.source, request)?;
                let symbols = self
                    .session()
                    .await?
                    .document_symbols(&observation, &bytes)
                    .await?;
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let language = self.support.language();
                Ok(encode(&language.support().normalize(
                    &query.source.path,
                    &text,
                    symbols,
                )))
            }
            Capability::Semantic => match decode::<SemanticQuery>(payload).map_err(invalid)? {
                SemanticQuery::Context {
                    source,
                    byte_offset,
                } => {
                    let started = tokio::time::Instant::now();
                    let (observation, bytes) = self.observe(&source, request)?;
                    let query = match byte_offset {
                        Some(offset) => ContextQuery::Symbol {
                            byte_offset: offset as usize,
                        },
                        None => ContextQuery::File,
                    };
                    let waits = self.builder.waits_for_diagnostics(byte_offset.is_none());
                    let session = self.session().await?;
                    let result = session.context(&observation, &bytes, query).await?;
                    if let Some(wait) =
                        diagnostics_wait(waits, request.budget_ms, started.elapsed())
                    {
                        let _ = tokio::time::timeout(wait, session.wait_for_matching_diagnostics())
                            .await;
                    }
                    let mut snapshot = session.diagnostics();
                    self.builder.diagnostics(&mut snapshot);
                    let encoding = result.position_encoding.clone();
                    let convert = |locations: Option<Vec<lsp::Location>>| {
                        locations.map(|locations| {
                            locations
                                .iter()
                                .filter_map(|location| {
                                    self.locate(location, &source, &bytes, &encoding)
                                })
                                .collect::<Vec<_>>()
                        })
                    };
                    let evidence = ContextEvidence {
                        lexical: match &result.mode {
                            ContextMode::Semantic => None,
                            ContextMode::Lexical { reason } => Some(reason.clone()),
                        },
                        document_version: result.document_version,
                        position_encoding: encoding.as_str().to_owned(),
                        definitions: convert(result.definitions.clone()),
                        references: convert(result.references.clone()),
                        truncated: result.truncated,
                        diagnostics: self.diagnostics(
                            &snapshot,
                            &observation,
                            &source,
                            &bytes,
                            &encoding,
                        ),
                    };
                    Ok(encode(&evidence))
                }
                SemanticQuery::Hover {
                    source,
                    byte_offset,
                } => {
                    let (observation, bytes) = self.observe(&source, request)?;
                    let contents = self
                        .session()
                        .await?
                        .hover(&observation, &bytes, byte_offset as usize)
                        .await?;
                    Ok(encode(&contents.map(|contents| Hover {
                        contents,
                        range: None,
                    })))
                }
                SemanticQuery::Definitions {
                    source,
                    byte_offset,
                }
                | SemanticQuery::References {
                    source,
                    byte_offset,
                } => {
                    let definitions = matches!(
                        decode::<SemanticQuery>(request.payload.clone()),
                        Ok(SemanticQuery::Definitions { .. })
                    );
                    let (observation, bytes) = self.observe(&source, request)?;
                    let session = self.session().await?;
                    let found = if definitions {
                        session
                            .definitions(&observation, &bytes, byte_offset as usize)
                            .await?
                    } else {
                        session
                            .references(&observation, &bytes, byte_offset as usize)
                            .await?
                    };
                    let encoding = session.capabilities().position_encoding.clone();
                    Ok(encode(&Some(
                        found
                            .iter()
                            .filter_map(|location| {
                                self.locate(location, &source, &bytes, &encoding)
                            })
                            .collect::<Vec<_>>(),
                    )))
                }
                SemanticQuery::WorkspaceSymbols { query } => {
                    let session = self.session().await?;
                    let found = session.workspace_symbols(&query).await?;
                    let encoding = session.capabilities().position_encoding.clone();
                    let none = SourceRef {
                        path: PathBuf::new(),
                        revision: String::new(),
                        text: SourceText::Missing,
                    };
                    Ok(encode(&Some(
                        found
                            .iter()
                            .filter_map(|symbol| {
                                Some(WorkspaceSymbol {
                                    name: symbol.name.clone(),
                                    kind: kind_of(symbol.kind),
                                    container: symbol.container_name.clone(),
                                    location: self.locate(
                                        &symbol.location,
                                        &none,
                                        &[],
                                        &encoding,
                                    )?,
                                })
                            })
                            .collect::<Vec<_>>(),
                    )))
                }
                SemanticQuery::Readiness {} => {
                    self.session().await?;
                    // The core's own wait: its request budget less the answer margin it added.
                    let budget = Duration::from_millis(
                        request.budget_ms.saturating_sub(READINESS_MARGIN_MS),
                    );
                    let hosted = self.hosted.as_mut().expect("started above");
                    let readiness = match hosted.live.wait_ready(budget).await {
                        Ok(()) => Readiness::Ready,
                        Err(ReadinessError::Loading) => Readiness::Warming,
                        Err(ReadinessError::WorkspaceError) => Readiness::Degraded,
                        Err(ReadinessError::Gone) => {
                            return Err(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "provider exited",
                            ));
                        }
                    };
                    Ok(encode(&readiness))
                }
                SemanticQuery::Diagnostics { source } => {
                    let (observation, bytes) = self.observe(&source, request)?;
                    let session = self.session().await?;
                    let mut snapshot = session.diagnostics();
                    let encoding = session.capabilities().position_encoding.clone();
                    self.builder.diagnostics(&mut snapshot);
                    Ok(encode(&self.diagnostics(
                        &snapshot,
                        &observation,
                        &source,
                        &bytes,
                        &encoding,
                    )))
                }
            },
            Capability::Calls => {
                let query: CallsQuery = decode(payload).map_err(invalid)?;
                match query {
                    CallsQuery::Prepare {
                        source,
                        byte_offset,
                    } => {
                        let (observation, bytes) = self.observe(&source, request)?;
                        let session = self.session().await?;
                        let items = session
                            .prepare_call_hierarchy(&observation, &bytes, byte_offset as usize)
                            .await?;
                        let encoding = session.capabilities().position_encoding.clone();
                        let converted = items
                            .into_iter()
                            .filter_map(|item| self.call_item(item, &source, &bytes, &encoding))
                            .collect::<Vec<_>>();
                        Ok(encode(&Some(converted)))
                    }
                    CallsQuery::Incoming { item } | CallsQuery::Outgoing { item } => {
                        let incoming = matches!(
                            decode::<CallsQuery>(request.payload.clone()),
                            Ok(CallsQuery::Incoming { .. })
                        );
                        let lsp_item = item
                            .handle
                            .as_ref()
                            .and_then(|handle| self.items.get(handle))
                            .cloned()
                            .ok_or_else(|| invalid("unknown call item".into()))?;
                        let none = SourceRef {
                            path: PathBuf::new(),
                            revision: String::new(),
                            text: SourceText::Missing,
                        };
                        let session = self.session().await?;
                        let encoding = session.capabilities().position_encoding.clone();
                        let pairs: Vec<(lsp::CallHierarchyItem, Vec<lsp::Range>)> = if incoming {
                            session
                                .incoming_calls_for(lsp_item)
                                .await?
                                .into_iter()
                                .map(|call| (call.from, call.from_ranges))
                                .collect()
                        } else {
                            session
                                .outgoing_calls_for(lsp_item)
                                .await?
                                .into_iter()
                                .map(|call| (call.to, call.from_ranges))
                                .collect()
                        };
                        let calls = pairs
                            .into_iter()
                            .filter_map(|(other, ranges)| {
                                let uri = other.uri.clone();
                                let item = self.call_item(other, &none, &[], &encoding)?;
                                let ranges = ranges
                                    .into_iter()
                                    .filter_map(|range| {
                                        self.locate(
                                            &lsp::Location {
                                                uri: uri.clone(),
                                                range,
                                            },
                                            &none,
                                            &[],
                                            &encoding,
                                        )
                                    })
                                    .collect();
                                Some(Call { item, ranges })
                            })
                            .collect::<Vec<_>>();
                        Ok(encode(&Some(calls)))
                    }
                }
            }
            Capability::Rename => {
                let query: RenameRequest = decode(payload).map_err(invalid)?;
                let (observation, bytes) = self.observe(&query.source, request)?;
                let edit = self
                    .session()
                    .await?
                    .rename(
                        &observation,
                        &bytes,
                        query.byte_offset as usize,
                        &query.new_name,
                    )
                    .await?;
                Ok(encode(&self.rename_answer(edit, &query, request)?))
            }
            other => Err(invalid(format!("{other:?} is not a provider capability"))),
        }
    }

    /// The worktree-relative path of a provider URI, if it is a file inside the worktree.
    fn relative(&self, uri: &lsp::Url) -> Option<PathBuf> {
        let path = uri.to_file_path().ok()?;
        path.strip_prefix(&self.root).ok().map(Path::to_path_buf)
    }

    /// Converts a provider location: inside the request source with its revision, elsewhere from
    /// the file read now (no revision).
    fn locate(
        &self,
        location: &lsp::Location,
        source: &SourceRef,
        bytes: &[u8],
        encoding: &lsp::PositionEncodingKind,
    ) -> Option<Location> {
        let path = self.relative(&location.uri)?;
        let (text, revision) = if path == source.path && !bytes.is_empty() {
            (
                String::from_utf8_lossy(bytes).into_owned(),
                Some(source.revision.clone()),
            )
        } else {
            (std::fs::read_to_string(self.root.join(&path)).ok()?, None)
        };
        Some(Location {
            start_byte: byte_offset(&text, location.range.start, encoding) as u64,
            end_byte: byte_offset(&text, location.range.end, encoding) as u64,
            path,
            revision,
        })
    }

    /// Converts and remembers one call hierarchy item.
    fn call_item(
        &mut self,
        item: lsp::CallHierarchyItem,
        source: &SourceRef,
        bytes: &[u8],
        encoding: &lsp::PositionEncodingKind,
    ) -> Option<CallItem> {
        let location = self.locate(
            &lsp::Location {
                uri: item.uri.clone(),
                range: item.range,
            },
            source,
            bytes,
            encoding,
        )?;
        let selection = self.locate(
            &lsp::Location {
                uri: item.uri.clone(),
                range: item.selection_range,
            },
            source,
            bytes,
            encoding,
        )?;
        let handle = format!("h{}", self.items.len() + 1);
        let converted = CallItem {
            name: item.name.clone(),
            kind: kind_of(item.kind),
            detail: item.detail.clone(),
            location,
            selection,
            handle: Some(handle.clone()),
        };
        self.items.insert(handle, item);
        Some(converted)
    }

    /// The diagnostics evidence of `snapshot`, bound to `observation` only when the provider's
    /// push was bound to its sequence.
    fn diagnostics(
        &self,
        snapshot: &crate::intelligence::session::DiagnosticSnapshot,
        observation: &SourceObservation,
        source: &SourceRef,
        bytes: &[u8],
        encoding: &lsp::PositionEncodingKind,
    ) -> DiagnosticsEvidence {
        let bound = snapshot
            .source
            .as_ref()
            .is_some_and(|binding| binding.sequence() == observation.sequence());
        let uri = lsp::Url::from_file_path(self.root.join(&source.path)).ok();
        DiagnosticsEvidence {
            revision: bound.then(|| source.revision.clone()),
            document_version: if bound {
                snapshot.document_version
            } else {
                None
            },
            readiness: match (bound, snapshot.readiness) {
                (true, DiagnosticReadiness::Clean) => "clean",
                (true, DiagnosticReadiness::Reported) => "reported",
                _ => "unknown",
            }
            .to_owned(),
            freshness: match (bound, snapshot.freshness) {
                (true, Freshness::Current) => "current",
                (true, Freshness::Provisional) => "provisional",
                (true, Freshness::Stale) => "stale",
                _ => "unknown",
            }
            .to_owned(),
            diagnostics: if bound {
                snapshot
                    .diagnostics
                    .iter()
                    .filter_map(|diagnostic| {
                        let location = self.locate(
                            &lsp::Location {
                                uri: uri.clone()?,
                                range: diagnostic.range,
                            },
                            source,
                            bytes,
                            encoding,
                        )?;
                        Some(Diagnostic {
                            location,
                            severity: diagnostic.severity.map(|severity| {
                                match severity {
                                    lsp::DiagnosticSeverity::ERROR => "error",
                                    lsp::DiagnosticSeverity::WARNING => "warning",
                                    lsp::DiagnosticSeverity::INFORMATION => "information",
                                    _ => "hint",
                                }
                                .to_owned()
                            }),
                            code: diagnostic.code.as_ref().map(|code| match code {
                                lsp::NumberOrString::Number(number) => number.to_string(),
                                lsp::NumberOrString::String(text) => text.clone(),
                            }),
                            source: diagnostic.source.clone(),
                            message: diagnostic.message.clone(),
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            },
            truncated: bound && snapshot.truncated,
        }
    }

    /// The rename answer for the provider's workspace edit: a proposal over the sources the core
    /// observed, the paths it still has to observe, or a refusal.
    fn rename_answer(
        &self,
        edit: Option<lsp::WorkspaceEdit>,
        query: &RenameRequest,
        request: &Incoming,
    ) -> io::Result<RenameAnswer> {
        let Some(edit) = edit else {
            return Ok(RenameAnswer::Refused("not renameable here".into()));
        };
        let grouped = crate::lang::edits::group_workspace_edit(edit);
        if !grouped.unsupported.is_empty() {
            return Ok(RenameAnswer::Refused(
                "resource operations are not supported".into(),
            ));
        }
        let encoding = self
            .hosted
            .as_ref()
            .map(|hosted| hosted.live.session.capabilities().position_encoding.clone())
            .unwrap_or(lsp::PositionEncodingKind::UTF16);
        let sources: HashMap<PathBuf, &SourceRef> = std::iter::once(&query.source)
            .chain(&query.sources)
            .map(|source| (source.path.clone(), source))
            .collect();
        let mut missing = Vec::new();
        let mut files = Vec::new();
        for file in grouped.files {
            let Some(path) = self.relative(&file.uri) else {
                return Ok(RenameAnswer::Refused("an edit leaves the worktree".into()));
            };
            let Some(source) = sources.get(&path) else {
                missing.push(path);
                continue;
            };
            let text = Self::text(source, request)?.unwrap_or_default();
            let mut replacements: Vec<Replacement> = file
                .edits
                .iter()
                .map(|edit| Replacement {
                    start_byte: byte_offset(&text, edit.range.start, &encoding) as u64,
                    end_byte: byte_offset(&text, edit.range.end, &encoding) as u64,
                    new_text: edit.new_text.clone(),
                })
                .collect();
            replacements.sort_by_key(|replacement| replacement.start_byte);
            files.push(FileEdit {
                path,
                base_revision: source.revision.clone(),
                replacements,
            });
        }
        Ok(if missing.is_empty() {
            RenameAnswer::Proposal(EditProposal { files })
        } else {
            RenameAnswer::NeedSources(missing)
        })
    }
}

impl<B: ProviderBuilder> ModuleServer for ProviderServer<B> {
    /// The support declaration plus the provider capabilities.
    fn declaration(&self) -> Declaration {
        let mut declaration = self.support.declaration();
        for decl in &mut declaration.capabilities {
            // Call hierarchy answers whatever the hosted provider advertises, exactly as an
            // in-process session asks it (outgoing calls included where incoming are not shown).
            if matches!(
                decl.capability,
                Capability::Outline | Capability::Semantic | Capability::Calls | Capability::Rename
            ) {
                *decl = CapabilityDecl::v0(decl.capability, Support::Supported);
            }
        }
        declaration
    }

    /// Keeps the granted provider launch and the language settings for the first demand.
    fn hello(&mut self, offer: &HelloOffer) -> impl Future<Output = Result<(), String>> + Send {
        let provider = offer.config.provider.clone().unwrap_or(Value::Null);
        let outcome = match provider
            .get("grant")
            .cloned()
            .map(serde_json::from_value::<ProviderGrant>)
        {
            Some(Ok(grant)) => {
                self.grant = Some(grant);
                self.settings = provider.get("settings").cloned().unwrap_or(Value::Null);
                Ok(true)
            }
            Some(Err(error)) => Err(format!("invalid provider grant: {error}")),
            None => Ok(false),
        };
        async move {
            // A granted provider starts (and initializes) before `hello` is answered, as the
            // in-process session starts within its open: a later readiness query or request
            // never pays the start. A start failure does not refuse `hello`; it is answered
            // typed by every provider request.
            if outcome? {
                let _ = self.session().await;
            }
            Ok(())
        }
    }

    /// Provider capabilities through the hosted session, the rest through the support adapter.
    async fn call<'a>(
        &'a mut self,
        request: Incoming,
        effects: Effects<'a>,
    ) -> Result<Answer, ServeError> {
        if !matches!(
            request.capability,
            Capability::Outline | Capability::Semantic | Capability::Calls | Capability::Rename
        ) {
            return self.support.call(request, effects).await;
        }
        Ok(match self.provider_answer(&request).await {
            // The answer carries the hosted provider's own readiness, never a blanket ready.
            Ok(value) => {
                let readiness = self.hosted.as_ref().map_or(Readiness::Ready, |hosted| {
                    hosted.live.session.module_readiness_of()
                });
                Answer {
                    readiness,
                    // A provider that is still loading or failed to load its workspace may have
                    // considered only part of it: never a complete answer.
                    coverage: if readiness == Readiness::Ready {
                        Coverage::Complete
                    } else {
                        Coverage::Partial
                    },
                    ..Answer::result(value)
                }
            }
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                Answer::error(ErrorCode::InvalidRequest, error.to_string())
            }
            Err(error) => {
                let alive = self
                    .hosted
                    .as_ref()
                    .is_some_and(|hosted| hosted.live.is_alive());
                if !alive && self.hosted.take().is_some() {
                    self.spent = true;
                }
                let cause = match error.kind() {
                    io::ErrorKind::TimedOut => Cause::Timeout,
                    io::ErrorKind::NotFound => Cause::ToolMissing,
                    io::ErrorKind::PermissionDenied => Cause::PolicyRefused,
                    _ if !alive => Cause::Exited,
                    // A live provider that failed this one request (an error reply) keeps
                    // serving, as an in-process session does: the request fails, the module
                    // is not retired.
                    _ => return Ok(Answer::error(ErrorCode::Failed, "provider request failed")),
                };
                Answer::unavailable(Stage::Provider, cause, "provider request failed")
            }
        })
    }
}

#[cfg(test)]
mod grant_tests {
    use super::*;

    /// A context answer waits for diagnostics only when its language does, at most 3 s and never
    /// past the request's remaining budget less the answer margin.
    #[test]
    fn the_diagnostics_wait_stays_inside_the_request_budget() {
        let ms = Duration::from_millis;
        assert_eq!(diagnostics_wait(false, 30_000, ms(0)), None);
        assert_eq!(
            diagnostics_wait(true, 30_000, ms(0)),
            Some(DIAGNOSTICS_WAIT)
        );
        assert_eq!(diagnostics_wait(true, 2_000, ms(500)), Some(ms(1_000)));
        assert_eq!(diagnostics_wait(true, 2_000, ms(1_800)), Some(ms(0)));
    }

    /// A plan starts only from accepted, unchanged files, with every absolute path of its
    /// arguments and environment inside the worktree, a granted root or an accepted file's
    /// directory; `..` never passes.
    #[test]
    fn provider_plans_are_admitted_against_the_grant() {
        let program = PathBuf::from("/bin/sh");
        let digest = blake3::hash(&std::fs::read(&program).unwrap())
            .to_hex()
            .to_string();
        let grant = ProviderGrant {
            accepted: vec![(program.clone(), digest)],
            request_timeout_ms: 1000,
            roots: vec![PathBuf::from("/private/var/cache-ns")],
        };
        let plan = |env: &[(&str, &str)]| ProviderLaunchPlan {
            program: program.clone(),
            args: vec!["--stdio".into()],
            env: env
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            reads: Vec::new(),
        };
        let worktree = Path::new("/private/var/work");
        assert!(
            grant
                .admit(
                    worktree,
                    &plan(&[
                        ("PATH", "/bin:/private/var/work/bin"),
                        ("TMPDIR", "/private/var/cache-ns/tmp")
                    ])
                )
                .is_ok()
        );
        for outside in [
            "/etc/ssh",
            "/private/var/cache-ns/../root",
            "/bin:/usr/local/secret",
        ] {
            assert_eq!(
                grant
                    .admit(worktree, &plan(&[("X", outside)]))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied,
                "{outside}"
            );
        }
        let mut other = plan(&[]);
        other.program = PathBuf::from("/bin/ls");
        assert!(
            grant.admit(worktree, &other).is_err(),
            "an unaccepted program"
        );
    }
}
