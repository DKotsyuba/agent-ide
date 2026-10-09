//! A [`LiveSession`] whose provider runs inside a bundled module, reached over a
//! `bundled-module/0` [`HostChannel`]. Every request carries the core's exact observation and
//! revision; replies are product DTOs the session turns back into the provider types its callers
//! already consume, with UTF-8 positions computed from the exact text of each file (the request's
//! own observation, or the file read now for any other location).
//!
//! Semantic evidence never establishes source identity: context results are built over the core's
//! own observation and only the module's evidence is overlaid. Any channel fault retires the
//! generation; the module's typed refusals are ordinary errors.

use std::{collections::HashMap, io, path::Path, time::Duration};

use async_lsp::lsp_types as lsp;
use serde::de::DeserializeOwned;

use super::{LiveSession, ProviderCapabilities, ProviderSettings, Session, SessionOptions};
use crate::{
    intelligence::{
        context::{self, ContextMode, ContextQuery, ContextResult},
        freshness::{DiagnosticReadiness, Freshness, ViewGeneration},
    },
    lang::{Outline, SymbolKind},
    modules::{
        contract::{Capability, Outcome},
        host::{Call, HostChannel, NoEffects},
        payload::{
            CallItem, CallsQuery, ContextEvidence, DiagnosticsEvidence, EditProposal, FileEdit,
            Hover, Location, OutlineRequest, RenameAnswer, RenameRequest, SemanticQuery, SourceRef,
            SourceText, WorkspaceSymbol, decode, encode,
        },
        wire::Attachment,
    },
    workspace::{authority::WorktreeRef, observation::SourceObservation},
};

/// The module side of a module-hosted session.
pub struct ModuleRemote {
    /// The instance's channel.
    channel: HostChannel,
    /// Whether the module declared call hierarchy support.
    calls: bool,
    /// The typed `module_unavailable` that failed this session, once one did.
    pub(super) fault: Option<String>,
    /// The module's typed dependency (provider) failure, once one failed this session.
    unavailable: Option<crate::modules::contract::ModuleUnavailable>,
}

impl ModuleRemote {
    /// Asks the module to exit.
    pub(super) async fn shutdown(&mut self) {
        self.channel.shutdown().await;
    }
}

/// One source as the module receives it.
fn source_ref(observation: &SourceObservation, text: &str) -> (SourceRef, Vec<Attachment>) {
    let revision = observation.source_revision().as_str().to_owned();
    if text.len() <= crate::modules::payload::MAX_INLINE_SOURCE {
        let text = if observation.bytes().is_some() {
            SourceText::Inline(text.to_owned())
        } else {
            SourceText::Missing
        };
        return (
            SourceRef {
                path: observation.path().to_path_buf(),
                revision,
                text,
            },
            Vec::new(),
        );
    }
    (
        SourceRef {
            path: observation.path().to_path_buf(),
            revision,
            text: SourceText::Attachment(1),
        },
        vec![Attachment {
            id: 1,
            content_type: "text/plain; charset=utf-8".to_owned(),
            bytes: text.as_bytes().to_vec(),
        }],
    )
}

/// The UTF-8 position of byte `offset` in `text` (clamped to a char boundary at or before it).
fn position(text: &str, offset: u64) -> lsp::Position {
    let mut offset = (offset as usize).min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let before = &text[..offset];
    let line = before.matches('\n').count() as u32;
    let start = before.rfind('\n').map_or(0, |at| at + 1);
    lsp::Position {
        line,
        character: (offset - start) as u32,
    }
}

/// The request's own source, against which a module's ranges are checked.
#[derive(Clone, Copy)]
struct Own<'a> {
    /// Its scope-relative path.
    path: &'a Path,
    /// The revision the request carried.
    revision: &'a str,
    /// Its exact text.
    text: &'a str,
}

/// The closed LSP kind of a product symbol kind.
fn lsp_kind(kind: SymbolKind) -> lsp::SymbolKind {
    match kind {
        SymbolKind::Module => lsp::SymbolKind::MODULE,
        SymbolKind::Namespace => lsp::SymbolKind::NAMESPACE,
        SymbolKind::Struct => lsp::SymbolKind::STRUCT,
        SymbolKind::Enum => lsp::SymbolKind::ENUM,
        SymbolKind::Class | SymbolKind::Impl => lsp::SymbolKind::CLASS,
        SymbolKind::Interface | SymbolKind::Trait => lsp::SymbolKind::INTERFACE,
        SymbolKind::TypeAlias => lsp::SymbolKind::TYPE_PARAMETER,
        SymbolKind::Function | SymbolKind::Test => lsp::SymbolKind::FUNCTION,
        SymbolKind::Method => lsp::SymbolKind::METHOD,
        SymbolKind::Constructor => lsp::SymbolKind::CONSTRUCTOR,
        SymbolKind::Field => lsp::SymbolKind::FIELD,
        SymbolKind::Variant => lsp::SymbolKind::ENUM_MEMBER,
        SymbolKind::Constant => lsp::SymbolKind::CONSTANT,
        SymbolKind::Variable => lsp::SymbolKind::VARIABLE,
        SymbolKind::Other => lsp::SymbolKind::OBJECT,
    }
}

impl LiveSession {
    /// Opens a session whose provider is hosted by the module behind `channel` (already past
    /// `hello`). `calls` reports the module's declared call hierarchy support.
    pub fn open_module(
        mut channel: HostChannel,
        calls: bool,
        worktree: WorktreeRef,
        epoch: u64,
        generation: ViewGeneration,
        settings: ProviderSettings,
        request_timeout: Duration,
    ) -> io::Result<Self> {
        if epoch == 0 || request_timeout.is_zero() || request_timeout > Duration::from_secs(60) {
            return Err(context::invalid(
                "invalid session authority epoch or request deadline",
            ));
        }
        let reader = channel
            .take_reader()
            .ok_or_else(|| io::Error::other("module channel already in use"))?;
        let driver = tokio::spawn(async move {
            let _ = reader.await;
            Ok(())
        });
        let mut advertised = lsp::ServerCapabilities {
            definition_provider: Some(lsp::OneOf::Left(true)),
            references_provider: Some(lsp::OneOf::Left(true)),
            hover_provider: Some(lsp::HoverProviderCapability::Simple(true)),
            document_symbol_provider: Some(lsp::OneOf::Left(true)),
            workspace_symbol_provider: Some(lsp::OneOf::Left(true)),
            rename_provider: Some(lsp::OneOf::Left(true)),
            ..Default::default()
        };
        if calls {
            advertised.call_hierarchy_provider =
                Some(lsp::CallHierarchyServerCapability::Simple(true));
        }
        let session = Session {
            server: super::ServerSocket::new_closed(),
            worktree,
            epoch,
            generation,
            state: super::fresh_state(&settings, generation),
            capabilities: Some(ProviderCapabilities {
                advertised,
                position_encoding: lsp::PositionEncodingKind::UTF8,
                server_info: None,
            }),
            settings,
            budget: super::OutboundBudget::default(),
            options: SessionOptions {
                request_timeout,
                lifetime: Duration::from_secs(300),
            },
            deadline: tokio::time::Instant::now() + Duration::from_secs(60 * 60 * 24 * 3650),
            sequence: 0,
            version: 0,
            module: Some(Box::new(ModuleRemote {
                channel,
                calls,
                fault: None,
                unavailable: None,
            })),
        };
        Ok(Self { session, driver })
    }
}

impl Session {
    /// Whether this session's provider is hosted by a bundled module.
    pub fn is_module(&self) -> bool {
        self.module.is_some()
    }

    /// The fault that retired this session's module channel, if any.
    pub fn module_fault(&self) -> Option<crate::modules::contract::ModuleUnavailable> {
        let remote = self.module.as_ref()?;
        let Some((stage, cause)) = remote.channel.fault() else {
            return remote.unavailable.clone();
        };
        let offer = remote.channel.offer();
        Some(crate::modules::contract::ModuleUnavailable {
            module_id: offer.module_id.clone(),
            module_version: offer.package_version.clone(),
            role: offer.role,
            stage,
            cause,
            instance: Some(offer.instance),
            retry_after_ms: None,
        })
    }

    /// Applies the local source fences and returns the exact text.
    fn module_source<'a>(
        &mut self,
        observation: &SourceObservation,
        bytes: &'a [u8],
    ) -> io::Result<&'a str> {
        if self.state.lock().expect("session lock").terminal {
            return Err(io::Error::other("LSP session is shut down"));
        }
        if observation.worktree() != &self.worktree
            || observation.authority_epoch() != self.epoch
            || observation.sequence() < self.sequence
        {
            return Err(context::invalid(
                "source does not match the current provider view",
            ));
        }
        self.sequence = observation.sequence();
        let text = context::observed_text(observation, bytes)?;
        if !self.state.lock().expect("session lock").active {
            return Err(io::Error::other("provider generation unavailable"));
        }
        Ok(text)
    }

    /// Sends one request and decodes its typed answer; a channel fault retires the generation, a
    /// module refusal is an error naming its code (and stage/cause for `unavailable`).
    async fn module_call<T: DeserializeOwned>(
        &mut self,
        capability: Capability,
        payload: serde_json::Value,
        attachments: Vec<Attachment>,
    ) -> io::Result<T> {
        let budget = self.options.request_timeout;
        self.module_call_within(budget, capability, payload, attachments)
            .await
    }

    /// The hosted provider's status barrier within `budget`: ready, still loading, or a
    /// workspace that failed to load; a module fault is a gone transport.
    pub(super) async fn module_readiness(
        &mut self,
        budget: std::time::Duration,
    ) -> Result<(), super::ReadinessError> {
        use super::ReadinessError;
        // The module waits within the budget; the margin carries its answer back.
        let answer: io::Result<crate::modules::contract::Readiness> = self
            .module_call_within(
                budget + std::time::Duration::from_millis(500),
                Capability::Semantic,
                encode(&SemanticQuery::Readiness {}),
                Vec::new(),
            )
            .await;
        match answer {
            Ok(crate::modules::contract::Readiness::Ready) => Ok(()),
            Ok(crate::modules::contract::Readiness::Warming) => Err(ReadinessError::Loading),
            Ok(crate::modules::contract::Readiness::Degraded) => {
                Err(ReadinessError::WorkspaceError)
            }
            Ok(crate::modules::contract::Readiness::Unavailable) | Err(_) => {
                Err(ReadinessError::Gone)
            }
        }
    }

    /// [`Self::module_call`] within an explicit `budget`.
    async fn module_call_within<T: DeserializeOwned>(
        &mut self,
        budget: std::time::Duration,
        capability: Capability,
        payload: serde_json::Value,
        attachments: Vec<Attachment>,
    ) -> io::Result<T> {
        let remote = self
            .module
            .as_mut()
            .ok_or_else(|| io::Error::other("not a module session"))?;
        let call = Call {
            capability,
            scope_key: self.worktree.worktree_path().display().to_string(),
            revision_key: format!(
                "{}:{}:{}:{}",
                self.generation.backend,
                self.generation.configuration,
                self.generation.toolchain,
                self.generation.view
            ),
            payload,
            attachments,
        };
        let reply = match remote.channel.call(call, budget, &mut NoEffects).await {
            Ok(reply) => reply,
            Err(failure) => {
                self.state.lock().expect("session lock").invalidate();
                remote.fault = Some(failure.to_string());
                return Err(io::Error::other(failure.to_string()));
            }
        };
        match reply.outcome {
            Outcome::Result(value) => decode(value).map_err(|_| {
                self.state.lock().expect("session lock").invalidate();
                io::Error::new(io::ErrorKind::InvalidData, "module reply ill-typed")
            }),
            Outcome::Error(error) => Err(io::Error::other(match error.unavailable {
                Some(unavailable) => {
                    // The module's provider failed: a typed fault that retires this generation.
                    let offer = remote.channel.offer();
                    let typed = crate::modules::contract::ModuleUnavailable {
                        module_id: offer.module_id.clone(),
                        module_version: offer.package_version.clone(),
                        role: offer.role,
                        stage: unavailable.stage,
                        cause: unavailable.cause,
                        instance: Some(offer.instance),
                        retry_after_ms: None,
                    };
                    let fault = typed.to_string();
                    remote.fault = Some(fault.clone());
                    remote.unavailable = Some(typed);
                    self.state.lock().expect("session lock").invalidate();
                    fault
                }
                None => format!("module refused: {:?}", error.code),
            })),
        }
    }

    /// A file's exact text for location conversion: the request's own text for its file, else a
    /// file the core reads now only when its path is relative and normal and it resolves inside
    /// the worktree (no `..`, no symlink out).
    fn module_text(&self, path: &Path, own: Option<Own<'_>>) -> Option<String> {
        if let Some(own) = own
            && own.path == path
        {
            return Some(own.text.to_owned());
        }
        let normal = !path.as_os_str().is_empty()
            && path
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_)));
        if !normal {
            return None;
        }
        let root = self.worktree.worktree_path().canonicalize().ok()?;
        let full = root.join(path).canonicalize().ok()?;
        full.starts_with(&root)
            .then(|| std::fs::read_to_string(full).ok())
            .flatten()
    }

    /// Converts one product location into a provider location with UTF-8 positions. A range
    /// in the request's own file must carry that request's revision, a range elsewhere none (the
    /// core reads that file itself); either must lie inside the text on UTF-8 boundaries, or the
    /// location is dropped rather than clamped.
    fn module_location(&self, location: &Location, own: Option<Own<'_>>) -> Option<lsp::Location> {
        let in_own = own.is_some_and(|own| own.path == location.path);
        match (&location.revision, own) {
            (Some(revision), Some(own)) if in_own && revision == own.revision => {}
            (None, _) if !in_own => {}
            _ => return None,
        }
        let text = self.module_text(&location.path, own)?;
        let (start, end) = (location.start_byte as usize, location.end_byte as usize);
        if start > end
            || end > text.len()
            || !text.is_char_boundary(start)
            || !text.is_char_boundary(end)
        {
            return None;
        }
        let uri =
            lsp::Url::from_file_path(self.worktree.worktree_path().join(&location.path)).ok()?;
        Some(lsp::Location {
            uri,
            range: lsp::Range {
                start: position(&text, location.start_byte),
                end: position(&text, location.end_byte),
            },
        })
    }

    /// Normalized outline of one source, computed by the module's provider and language.
    pub async fn module_outline(
        &mut self,
        observation: &SourceObservation,
        bytes: &[u8],
    ) -> io::Result<Outline> {
        let text = self.module_source(observation, bytes)?;
        let (source, attachments) = source_ref(observation, text);
        self.module_call(
            Capability::Outline,
            encode(&OutlineRequest { source }),
            attachments,
        )
        .await
    }

    /// Hover text at a byte offset.
    pub(super) async fn module_hover(
        &mut self,
        observation: &SourceObservation,
        bytes: &[u8],
        byte_offset: usize,
    ) -> io::Result<Option<String>> {
        let text = self.module_source(observation, bytes)?;
        let (source, attachments) = source_ref(observation, text);
        let hover: Option<Hover> = self
            .module_call(
                Capability::Semantic,
                encode(&SemanticQuery::Hover {
                    source,
                    byte_offset: byte_offset as u64,
                }),
                attachments,
            )
            .await?;
        Ok(hover.map(|hover| hover.contents))
    }

    /// Definitions or references at a byte offset.
    pub(super) async fn module_locations(
        &mut self,
        observation: &SourceObservation,
        bytes: &[u8],
        byte_offset: usize,
        definitions: bool,
    ) -> io::Result<Vec<lsp::Location>> {
        let text = self.module_source(observation, bytes)?.to_owned();
        let (source, attachments) = source_ref(observation, &text);
        let byte_offset = byte_offset as u64;
        let query = if definitions {
            SemanticQuery::Definitions {
                source,
                byte_offset,
            }
        } else {
            SemanticQuery::References {
                source,
                byte_offset,
            }
        };
        let found: Option<Vec<Location>> = self
            .module_call(Capability::Semantic, encode(&query), attachments)
            .await?;
        let own = Some(Own {
            path: observation.path(),
            revision: observation.source_revision().as_str(),
            text: text.as_str(),
        });
        Ok(found
            .unwrap_or_default()
            .iter()
            .filter_map(|location| self.module_location(location, own))
            .collect())
    }

    /// Workspace symbols matching `query`.
    pub(super) async fn module_workspace_symbols(
        &mut self,
        query: &str,
    ) -> io::Result<Vec<lsp::SymbolInformation>> {
        let found: Option<Vec<WorkspaceSymbol>> = self
            .module_call(
                Capability::Semantic,
                encode(&SemanticQuery::WorkspaceSymbols {
                    query: query.to_owned(),
                }),
                Vec::new(),
            )
            .await?;
        #[allow(deprecated)]
        Ok(found
            .unwrap_or_default()
            .iter()
            .filter_map(|symbol| {
                Some(lsp::SymbolInformation {
                    name: symbol.name.clone(),
                    kind: lsp_kind(symbol.kind),
                    tags: None,
                    deprecated: None,
                    location: self.module_location(&symbol.location, None)?,
                    container_name: symbol.container.clone(),
                })
            })
            .collect())
    }

    /// The call hierarchy items at a byte offset, carrying the module's handle in `data`.
    pub(super) async fn module_prepare_calls(
        &mut self,
        observation: &SourceObservation,
        bytes: &[u8],
        byte_offset: usize,
    ) -> io::Result<Vec<lsp::CallHierarchyItem>> {
        if !self.module.as_ref().is_some_and(|remote| remote.calls) {
            return Ok(Vec::new());
        }
        let text = self.module_source(observation, bytes)?.to_owned();
        let (source, attachments) = source_ref(observation, &text);
        let items: Option<Vec<CallItem>> = self
            .module_call(
                Capability::Calls,
                encode(&CallsQuery::Prepare {
                    source,
                    byte_offset: byte_offset as u64,
                }),
                attachments,
            )
            .await?;
        let own = Some(Own {
            path: observation.path(),
            revision: observation.source_revision().as_str(),
            text: text.as_str(),
        });
        Ok(items
            .unwrap_or_default()
            .iter()
            .filter_map(|item| self.module_call_item(item, own))
            .collect())
    }

    /// Converts one product call item, keeping its handle in `data`.
    fn module_call_item(
        &self,
        item: &CallItem,
        own: Option<Own<'_>>,
    ) -> Option<lsp::CallHierarchyItem> {
        let location = self.module_location(&item.location, own)?;
        let selection = self.module_location(&item.selection, own)?;
        Some(lsp::CallHierarchyItem {
            name: item.name.clone(),
            kind: lsp_kind(item.kind),
            tags: None,
            detail: item.detail.clone(),
            uri: location.uri,
            range: location.range,
            selection_range: selection.range,
            data: Some(encode(item)),
        })
    }

    /// Incoming or outgoing calls of one item prepared by this session.
    pub(super) async fn module_calls_for(
        &mut self,
        item: lsp::CallHierarchyItem,
        incoming: bool,
    ) -> io::Result<Vec<(lsp::CallHierarchyItem, Vec<lsp::Range>)>> {
        let item: CallItem = item
            .data
            .and_then(|data| decode(data).ok())
            .ok_or_else(|| context::invalid("call item not prepared by this module"))?;
        let query = if incoming {
            CallsQuery::Incoming { item }
        } else {
            CallsQuery::Outgoing { item }
        };
        let calls: Option<Vec<crate::modules::payload::Call>> = self
            .module_call(Capability::Calls, encode(&query), Vec::new())
            .await?;
        Ok(calls
            .unwrap_or_default()
            .iter()
            .filter_map(|call| {
                let item = self.module_call_item(&call.item, None)?;
                let ranges = call
                    .ranges
                    .iter()
                    .filter_map(|range| {
                        self.module_location(range, None)
                            .map(|location| location.range)
                    })
                    .collect();
                Some((item, ranges))
            })
            .collect())
    }

    /// Rename through the module: the module names the files it touches, the core sends their
    /// exact current text, and the module's proposal over those revisions is validated before it
    /// becomes the workspace edit the caller applies file by file.
    pub(super) async fn module_rename(
        &mut self,
        observation: &SourceObservation,
        bytes: &[u8],
        byte_offset: usize,
        new_name: &str,
    ) -> io::Result<Option<lsp::WorkspaceEdit>> {
        let text = self.module_source(observation, bytes)?.to_owned();
        let (source, mut attachments) = source_ref(observation, &text);
        let mut texts: HashMap<std::path::PathBuf, (String, String)> = HashMap::from([(
            observation.path().to_path_buf(),
            (source.revision.clone(), text.clone()),
        )]);
        let mut sources: Vec<SourceRef> = Vec::new();
        for _ in 0..2 {
            let answer: RenameAnswer = self
                .module_call(
                    Capability::Rename,
                    encode(&RenameRequest {
                        source: source.clone(),
                        byte_offset: byte_offset as u64,
                        new_name: new_name.to_owned(),
                        sources: sources.clone(),
                    }),
                    attachments.clone(),
                )
                .await?;
            match answer {
                RenameAnswer::Refused(reason) => return Err(io::Error::other(reason)),
                RenameAnswer::NeedSources(paths) => {
                    for path in paths {
                        if texts.contains_key(&path) {
                            continue;
                        }
                        let current = self
                            .module_text(&path, None)
                            .ok_or_else(|| context::invalid("rename touches an unreadable file"))?;
                        let revision =
                            format!("read:{}", blake3::hash(current.as_bytes()).to_hex());
                        let id = attachments.len() as u32 + 2;
                        attachments.push(Attachment {
                            id,
                            content_type: "text/plain; charset=utf-8".to_owned(),
                            bytes: current.as_bytes().to_vec(),
                        });
                        sources.push(SourceRef {
                            path: path.clone(),
                            revision: revision.clone(),
                            text: SourceText::Attachment(id),
                        });
                        texts.insert(path, (revision, current));
                    }
                }
                RenameAnswer::Proposal(proposal) => {
                    return self.module_workspace_edit(&proposal, &texts).map(Some);
                }
            }
        }
        Err(io::Error::other("rename did not converge"))
    }

    /// Validates `proposal` against the texts sent and builds the provider workspace edit.
    fn module_workspace_edit(
        &self,
        proposal: &EditProposal,
        texts: &HashMap<std::path::PathBuf, (String, String)>,
    ) -> io::Result<lsp::WorkspaceEdit> {
        proposal
            .validate(|path| {
                texts
                    .get(path)
                    .map(|(revision, text)| (revision.as_str(), text.as_str()))
            })
            .map_err(|refusal| {
                context::invalid(&format!("rename proposal refused: {refusal:?}"))
            })?;
        let mut changes = HashMap::new();
        for FileEdit {
            path, replacements, ..
        } in &proposal.files
        {
            let (_, text) = &texts[path];
            let uri = lsp::Url::from_file_path(self.worktree.worktree_path().join(path))
                .map_err(|()| context::invalid("rename path is not a file URI"))?;
            changes.insert(
                uri,
                replacements
                    .iter()
                    .map(|replacement| lsp::TextEdit {
                        range: lsp::Range {
                            start: position(text, replacement.start_byte),
                            end: position(text, replacement.end_byte),
                        },
                        new_text: replacement.new_text.clone(),
                    })
                    .collect(),
            );
        }
        Ok(lsp::WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        })
    }

    /// The module form of [`Session::context`]: a lexical result over the core's own observation
    /// with only the module's provider evidence overlaid, and diagnostics bound only when the
    /// module saw the same revision.
    pub(super) async fn module_context(
        &mut self,
        observation: &SourceObservation,
        bytes: &[u8],
        query: ContextQuery,
    ) -> io::Result<ContextResult> {
        let mut result =
            context::lexical_context(observation, bytes, query, "semantic operations unavailable")?;
        let text = match self.module_source(observation, bytes) {
            Ok(text) => text.to_owned(),
            Err(error) if error.to_string() == "provider generation unavailable" => {
                result.mode = ContextMode::Lexical {
                    reason: "provider generation unavailable".into(),
                };
                return Ok(result);
            }
            Err(error) => return Err(error),
        };
        let (source, attachments) = source_ref(observation, &text);
        let byte_offset = match query {
            ContextQuery::Symbol { byte_offset } => Some(byte_offset as u64),
            ContextQuery::File => None,
        };
        let reply: io::Result<ContextEvidence> = self
            .module_call(
                Capability::Semantic,
                encode(&SemanticQuery::Context {
                    source,
                    byte_offset,
                }),
                attachments,
            )
            .await;
        let own = Some(Own {
            path: observation.path(),
            revision: observation.source_revision().as_str(),
            text: text.as_str(),
        });
        match reply {
            Ok(evidence) => {
                let convert = |locations: &Option<Vec<Location>>| {
                    locations.as_ref().map(|locations| {
                        locations
                            .iter()
                            .filter_map(|location| self.module_location(location, own))
                            .collect::<Vec<_>>()
                    })
                };
                result.generation = Some(self.generation);
                result.document_version = evidence.document_version;
                result.truncated = evidence.truncated;
                result.definitions = convert(&evidence.definitions);
                result.references = convert(&evidence.references);
                match &evidence.lexical {
                    None => {
                        result.mode = ContextMode::Semantic;
                        result.position_encoding = lsp::PositionEncodingKind::UTF8;
                        result.lexical_matches.clear();
                    }
                    Some(reason) => {
                        result.mode = ContextMode::Lexical {
                            reason: reason.clone(),
                        }
                    }
                }
                self.module_bind_diagnostics(&evidence.diagnostics, observation, &text);
            }
            Err(error) => {
                let mut state = self.state.lock().expect("session lock");
                state.diagnostics.source = None;
                state.diagnostics.document_version = None;
                state.diagnostics.freshness = Freshness::Unknown;
                state.diagnostics.readiness = DiagnosticReadiness::Unknown;
                state.diagnostics.diagnostics.clear();
                result.mode = ContextMode::Lexical {
                    reason: context::prefix(&error.to_string(), 256).into(),
                };
            }
        }
        Ok(result)
    }

    /// Stores the module's diagnostics evidence, bound to `observation` only for its revision.
    fn module_bind_diagnostics(
        &mut self,
        evidence: &DiagnosticsEvidence,
        observation: &SourceObservation,
        text: &str,
    ) {
        let bound = evidence.revision.as_deref() == Some(observation.source_revision().as_str());
        let diagnostics: Vec<lsp::Diagnostic> = if bound {
            evidence
                .diagnostics
                .iter()
                .map(|diagnostic| lsp::Diagnostic {
                    range: lsp::Range {
                        start: position(text, diagnostic.location.start_byte),
                        end: position(text, diagnostic.location.end_byte),
                    },
                    severity: diagnostic
                        .severity
                        .as_deref()
                        .map(|severity| match severity {
                            "error" => lsp::DiagnosticSeverity::ERROR,
                            "warning" => lsp::DiagnosticSeverity::WARNING,
                            "information" => lsp::DiagnosticSeverity::INFORMATION,
                            _ => lsp::DiagnosticSeverity::HINT,
                        }),
                    code: diagnostic.code.clone().map(lsp::NumberOrString::String),
                    source: diagnostic.source.clone(),
                    message: diagnostic.message.clone(),
                    ..Default::default()
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut state = self.state.lock().expect("session lock");
        let snapshot = &mut state.diagnostics;
        snapshot.source = bound
            .then(|| crate::intelligence::freshness::SourceBinding::from_observation(observation));
        snapshot.document_version = None;
        snapshot.readiness = match (bound, evidence.readiness.as_str()) {
            (true, "clean") => DiagnosticReadiness::Clean,
            (true, "reported") => DiagnosticReadiness::Reported,
            _ => DiagnosticReadiness::Unknown,
        };
        snapshot.freshness = match (bound, evidence.freshness.as_str()) {
            (true, "current") => Freshness::Current,
            (true, "provisional") => Freshness::Provisional,
            (true, "stale") => Freshness::Stale,
            _ => Freshness::Unknown,
        };
        snapshot.diagnostics = diagnostics;
        snapshot.diagnostics.truncate(128);
        snapshot.truncated = bound && evidence.truncated;
    }
}
