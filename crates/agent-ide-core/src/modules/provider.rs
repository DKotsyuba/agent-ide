//! Module-side hosting of a language's provider: a [`LiveSession`] on the module's own provider
//! child, answered as product DTOs. Wraps the language's [`SupportServer`] for everything else.
//!
//! The core sends exact sources with its revisions; the module synchronizes them under its own
//! monotonic sequence and converts every provider position into a scope-relative UTF-8 byte
//! range, reading a file the request did not carry from disk (its range then carries no
//! revision and the core re-observes it). Provider failures answer the typed `unavailable`
//! refusal at stage `provider`; the instance survives and restarts its provider on demand.

use std::{
    collections::HashMap,
    future::Future,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use async_lsp::lsp_types as lsp;
use serde::Deserialize;
use serde_json::Value;

use super::{
    adapter::SupportServer,
    contract::{
        Capability, CapabilityDecl, Cause, Declaration, ErrorCode, HelloOffer, Stage, Support,
    },
    payload::{
        Call, CallItem, CallsQuery, ContextEvidence, Diagnostic, DiagnosticsEvidence, EditProposal,
        FileEdit, Hover, Location, OutlineRequest, RenameAnswer, RenameRequest, Replacement,
        SemanticQuery, SourceRef, SourceText, WorkspaceSymbol, decode, encode,
    },
    serve::{Answer, Effects, Incoming, ModuleServer, ServeError},
};
use crate::{
    intelligence::{
        context::{ContextMode, ContextQuery},
        freshness::{DiagnosticReadiness, Freshness, ViewGeneration},
        session::{LiveSession, ProviderSettings, Session},
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

/// The core-supplied session parameters inside `hello.config.provider.session`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionParams {
    /// Absolute worktree.
    pub worktree: PathBuf,
    /// Absolute repository root.
    pub repository_root: PathBuf,
    /// Raw Git common directory.
    pub git_common_dir: PathBuf,
    /// Positive lifecycle incarnation.
    pub incarnation: u64,
    /// Authority epoch.
    pub epoch: u64,
    /// The core's generation fences.
    pub generation: [u64; 4],
    /// Per-request provider deadline.
    pub request_timeout_ms: u64,
}

/// What a language supplies to host its provider: from the hello configuration's `provider`
/// value, the session settings and the command that starts the provider with piped stdio.
pub trait ProviderBuilder: Send + 'static {
    /// Builds settings and command; an error is a deterministic configuration refusal.
    fn build(&self, provider: &Value) -> io::Result<(ProviderSettings, tokio::process::Command)>;
}

/// A started provider.
struct Hosted {
    /// The session.
    live: LiveSession,
    /// The provider child, killed on drop.
    _child: tokio::process::Child,
}

/// Serves a language's support and its hosted provider.
pub struct ProviderServer<B: ProviderBuilder> {
    /// The language's support adapter.
    support: SupportServer,
    /// Starts the provider.
    builder: B,
    /// The hello configuration's provider value.
    provider: Value,
    /// Its session parameters.
    params: Option<SessionParams>,
    /// The hosted provider, started on first demand.
    hosted: Option<Hosted>,
    /// Next source sequence.
    sequence: u64,
    /// Call hierarchy items by handle, valid for this instance.
    items: HashMap<String, lsp::CallHierarchyItem>,
}

impl<B: ProviderBuilder> ProviderServer<B> {
    /// The server for `support`'s language with `builder`.
    pub fn new(support: SupportServer, builder: B) -> Self {
        Self {
            support,
            builder,
            provider: Value::Null,
            params: None,
            hosted: None,
            sequence: 0,
            items: HashMap::new(),
        }
    }

    /// The worktree of this instance.
    fn worktree(&self) -> io::Result<WorktreeRef> {
        let params = self
            .params
            .as_ref()
            .ok_or_else(|| io::Error::other("no provider session configured"))?;
        WorktreeRef::from_discovery(
            params.worktree.clone(),
            params.repository_root.clone(),
            params.git_common_dir.clone(),
            params.incarnation,
        )
        .map_err(|_| io::Error::other("invalid worktree"))
    }

    /// The live session, started when absent or dead.
    async fn session(&mut self) -> io::Result<&mut Session> {
        if self
            .hosted
            .as_ref()
            .is_some_and(|hosted| !hosted.live.is_alive())
        {
            self.hosted = None;
        }
        if self.hosted.is_none() {
            let params = self
                .params
                .clone()
                .ok_or_else(|| io::Error::other("no provider session configured"))?;
            let (settings, mut command) = self.builder.build(&self.provider)?;
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
            let live = LiveSession::open(
                stdout,
                stdin,
                self.worktree()?,
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
            self.hosted = Some(Hosted {
                live,
                _child: child,
            });
        }
        Ok(&mut self.hosted.as_mut().expect("started above").live.session)
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

    /// The module-side observation of `source` under the next sequence.
    fn observe(
        &mut self,
        source: &SourceRef,
        request: &Incoming,
    ) -> io::Result<(SourceObservation, Vec<u8>)> {
        let text = Self::text(source, request)?;
        self.sequence += 1;
        let params = self
            .params
            .as_ref()
            .ok_or_else(|| io::Error::other("no session"))?;
        let bytes = text.clone().unwrap_or_default().into_bytes();
        let observation = SourceObservation::new(
            self.worktree()?,
            params.epoch,
            self.sequence,
            ObservationRef::new(format!("module-{}", self.sequence))
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
                    let (observation, bytes) = self.observe(&source, request)?;
                    let query = match byte_offset {
                        Some(offset) => ContextQuery::Symbol {
                            byte_offset: offset as usize,
                        },
                        None => ContextQuery::File,
                    };
                    let session = self.session().await?;
                    let result = session.context(&observation, &bytes, query).await?;
                    session.wait_for_matching_diagnostics().await;
                    let snapshot = session.diagnostics();
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
                SemanticQuery::Diagnostics { source } => {
                    let (observation, bytes) = self.observe(&source, request)?;
                    let session = self.session().await?;
                    let snapshot = session.diagnostics();
                    let encoding = session.capabilities().position_encoding.clone();
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
        let root = &self.params.as_ref()?.worktree;
        let path = uri.to_file_path().ok()?;
        path.strip_prefix(root).ok().map(Path::to_path_buf)
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
            let root = &self.params.as_ref()?.worktree;
            (std::fs::read_to_string(root.join(&path)).ok()?, None)
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
        let uri = lsp::Url::from_file_path(
            self.params
                .as_ref()
                .map(|params| params.worktree.join(&source.path))
                .unwrap_or_default(),
        )
        .ok();
        DiagnosticsEvidence {
            revision: bound.then(|| source.revision.clone()),
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
            if matches!(
                decl.capability,
                Capability::Outline | Capability::Semantic | Capability::Calls | Capability::Rename
            ) {
                *decl = CapabilityDecl::v0(decl.capability, Support::Supported);
            }
        }
        declaration
    }

    /// Keeps the provider configuration and session parameters for the first demand.
    fn hello(&mut self, offer: &HelloOffer) -> impl Future<Output = Result<(), String>> + Send {
        let provider = offer.config.provider.clone().unwrap_or(Value::Null);
        let params = provider
            .get("session")
            .cloned()
            .map(serde_json::from_value::<SessionParams>);
        let outcome = match params {
            Some(Ok(params)) => {
                self.params = Some(params);
                self.provider = provider;
                Ok(())
            }
            Some(Err(error)) => Err(format!("invalid session parameters: {error}")),
            None => Ok(()),
        };
        async move { outcome }
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
            Ok(value) => Answer::result(value),
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                Answer::error(ErrorCode::InvalidRequest, error.to_string())
            }
            Err(error) => {
                let alive = self
                    .hosted
                    .as_ref()
                    .is_some_and(|hosted| hosted.live.is_alive());
                if !alive {
                    self.hosted = None;
                }
                let cause = match error.kind() {
                    io::ErrorKind::TimedOut => Cause::Timeout,
                    io::ErrorKind::NotFound => Cause::ToolMissing,
                    _ if !alive => Cause::Exited,
                    _ => Cause::Malformed,
                };
                Answer::unavailable(Stage::Provider, cause, "provider request failed")
            }
        })
    }
}
