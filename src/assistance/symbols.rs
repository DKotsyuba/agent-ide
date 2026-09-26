//! Symbol-addressed worker jobs: `ide.outline`, `ide.read`, `ide.symbol` and `ide.graph` (v0.4).
//!
//! Every job observes the source through Workspace exactly like `ide.context`, asks the binding's
//! live language server for document symbols, normalizes them through the language module and
//! renders the compact text the contract specifies. Results are retained with the source
//! observation so a later `ide.edit` may name them as `source_ref`.

use super::*;
use crate::lang::{
    self, Language as Lang, LineRange, Outline, SymbolPath, TestId,
    render::{self, Call, SymbolCard, Usage},
};
use std::collections::VecDeque;

impl Worker<'_> {
    /// Finds referencing tests and counts outline tests in the symbol's own file. Rust test names
    /// include source-derived crate modules; other languages keep their outline naming.
    pub(super) async fn tests_referencing_symbol(
        &mut self,
        job: &mut Job,
        requested: &str,
    ) -> Result<(Vec<TestId>, Lang, usize), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let symbol = SymbolPath::parse(requested).map_err(|_| FailureCode::UnknownSymbol)?;
        let file = symbol
            .file()
            .ok_or(FailureCode::UnknownSymbol)?
            .to_path_buf();
        let (observed, bytes) = self.observe(&binding, file).await?;
        let (outline, root) = self.outline_of(job, &observed, &bytes).await?;
        let found = outline.find(&symbol).ok_or(FailureCode::UnknownSymbol)?;
        let mut file_test_count = 0;
        for candidate in &outline.symbols {
            candidate.walk(&mut |candidate| {
                file_test_count += usize::from(candidate.kind == crate::lang::SymbolKind::Test);
            });
        }
        let offset = name_offset(observed_text(&observed, &bytes)?, found)?;
        let refs = self
            .live_session_for(job, &observed)
            .await?
            .session
            .references(&observed, &bytes, offset)
            .await
            .map_err(|_| FailureCode::ProviderUnavailable)?;
        let language = Lang::for_path(observed.path()).ok_or(FailureCode::ProviderUnavailable)?;
        // A test is whatever the outline marks as one, wherever it lives: Rust keeps most unit
        // tests in a `#[cfg(test)] mod tests` of the file under test, so the test-file
        // convention alone would miss them. One outline per referenced file.
        let mut outlines: std::collections::BTreeMap<std::path::PathBuf, crate::lang::Outline> =
            std::collections::BTreeMap::new();
        let mut tests = std::collections::BTreeSet::new();
        for location in refs {
            let Ok(absolute) = location.uri.to_file_path() else {
                continue;
            };
            let Ok(relative) = absolute.strip_prefix(&root) else {
                continue;
            };
            let relative = relative.to_path_buf();
            if !outlines.contains_key(&relative) {
                let (test_observed, test_bytes) = self.observe(&binding, relative.clone()).await?;
                let (test_outline, _) = self.outline_of(job, &test_observed, &test_bytes).await?;
                outlines.insert(relative.clone(), test_outline);
            }
            let test_outline = &outlines[&relative];
            let line = location.range.start.line + 1;
            let mut enclosing = None;
            for candidate in &test_outline.symbols {
                candidate.walk(&mut |candidate| {
                    if candidate.range.start <= line
                        && line <= candidate.range.end
                        && enclosing.is_none_or(|old: &crate::lang::Symbol| {
                            candidate.range.len() < old.range.len()
                        })
                    {
                        enclosing = Some(candidate);
                    }
                });
            }
            if let Some(test) = enclosing
                && test.kind == crate::lang::SymbolKind::Test
            {
                let outline_path = test.path.segments().join("::");
                let name = if language == Lang::Rust {
                    crate::lang::rust::test_id(&relative, &outline_path)
                } else {
                    outline_path
                };
                tests.insert((relative.clone(), name));
            }
        }
        Ok((
            tests
                .into_iter()
                .map(|(file, name)| TestId { file, name })
                .collect(),
            language,
            file_test_count,
        ))
    }

    /// `ide.outline {path}`: the file skeleton.
    pub(super) async fn outline(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let path = job.parameters["path"]
            .as_str()
            .ok_or(FailureCode::SourceUnavailable)?
            .to_owned();
        let authority = self.authority(&binding).await?;
        let root = authority.worktree().worktree_path();
        let requested = root.join(&path);
        if requested.is_dir() {
            let root = std::fs::canonicalize(root).map_err(|_| FailureCode::SourceUnavailable)?;
            let directory =
                std::fs::canonicalize(&requested).map_err(|_| FailureCode::SourceUnavailable)?;
            let relative = directory
                .strip_prefix(&root)
                .map_err(|_| FailureCode::OutsideAllowedRoots)?;
            let text = render::directory_outline(&root, relative)
                .map_err(|_| FailureCode::SourceUnavailable)?;
            let (reply, page) =
                ContextPageState::new(text, 0, false, ResultKind::Outline).next(&job.reference)?;
            self.shared.set_context_page(&job.reference, page);
            return Ok((reply, Some(authority), None));
        }
        let (observed, bytes) = self.observe(&binding, path.clone().into()).await?;
        let (outline, _) = self.outline_of(job, &observed, &bytes).await?;
        let authority = self.finish_symbol_job(job, &binding, &observed).await?;
        let text = render::outline_text(&outline);
        let (reply, page) =
            ContextPageState::new(text, 0, false, ResultKind::Outline).next(&job.reference)?;
        self.shared.set_context_page(&job.reference, page);
        Ok((reply, Some(authority), Some(observed)))
    }

    /// `ide.read {symbol}` or `ide.read {path, lines}`: numbered source with the header.
    pub(super) async fn read(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let (path, range, title) = match job.parameters.get("symbol").and_then(Value::as_str) {
            Some(symbol) => {
                let symbol = SymbolPath::parse(symbol).map_err(|_| FailureCode::UnknownSymbol)?;
                let file = symbol
                    .file()
                    .ok_or(FailureCode::UnknownSymbol)?
                    .to_path_buf();
                let (observed, bytes) = self.observe(&binding, file.clone()).await?;
                let (outline, _) = self.outline_of(job, &observed, &bytes).await?;
                let found = outline.find(&symbol).ok_or(FailureCode::UnknownSymbol)?;
                (file, found.range, symbol.to_string())
            }
            None => {
                let path = job.parameters["path"]
                    .as_str()
                    .ok_or(FailureCode::SourceUnavailable)?
                    .to_owned();
                let range = job
                    .parameters
                    .get("lines")
                    .and_then(Value::as_str)
                    .and_then(crate::assistance::facade::parse_line_range)
                    .ok_or(FailureCode::SourceUnavailable)?;
                (std::path::PathBuf::from(&path), range, path)
            }
        };
        let (observed, bytes) = self.observe(&binding, path.clone()).await?;
        let source = observed_text(&observed, &bytes)?;
        let total = lang::line_count(source);
        if range.start > total {
            return Err(FailureCode::SourceUnavailable);
        }
        let range = LineRange::new(range.start, range.end.min(total));
        let authority = self.finish_symbol_job(job, &binding, &observed).await?;
        let mut text = render::read_text(&path, Some(&title), range, source);
        text.push_str(&format!("source_ref: {}\n", job.reference));
        let (reply, page) =
            ContextPageState::new(text, 0, false, ResultKind::Read).next(&job.reference)?;
        self.shared.set_context_page(&job.reference, page);
        Ok((reply, Some(authority), Some(observed)))
    }

    /// `ide.symbol {symbol, usages?, callers?, callees?, history?}`: the symbol card.
    pub(super) async fn symbol(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let requested = job.parameters["symbol"]
            .as_str()
            .ok_or(FailureCode::UnknownSymbol)?
            .to_owned();
        let symbol = SymbolPath::parse(&requested).map_err(|_| FailureCode::UnknownSymbol)?;
        let want_usages = job
            .parameters
            .get("usages")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let callers_depth = job
            .parameters
            .get("callers")
            .and_then(Value::as_u64)
            .unwrap_or(1);
        let callees_depth = job
            .parameters
            .get("callees")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let want_history = job
            .parameters
            .get("history")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // Resolve the definition file: a bare name goes through workspace symbols first.
        let file = match symbol.file() {
            Some(file) => file.to_path_buf(),
            None => {
                let name = symbol.name().ok_or(FailureCode::UnknownSymbol)?.to_owned();
                match self.locate_by_name(job, &binding, &name).await? {
                    Located::One(file) => file,
                    Located::Many(candidates) => {
                        return self.ambiguous(job, &binding, &requested, candidates).await;
                    }
                }
            }
        };
        let (observed, bytes) = self.observe(&binding, file.clone()).await?;
        let (outline, worktree_root) = self.outline_of(job, &observed, &bytes).await?;
        let found = match symbol.file() {
            Some(_) => outline.find(&symbol).cloned(),
            None => {
                let name = symbol.name().unwrap_or_default();
                let candidates = outline.named(name);
                match candidates.len() {
                    1 => Some(candidates[0].clone()),
                    0 => None,
                    _ => {
                        let candidates = candidates
                            .iter()
                            .map(|candidate| candidate.path.to_string())
                            .collect::<Vec<_>>();
                        return self.ambiguous(job, &binding, &requested, candidates).await;
                    }
                }
            }
        }
        .ok_or(FailureCode::UnknownSymbol)?;
        let source = observed_text(&observed, &bytes)?;
        let byte_offset = name_offset(source, &found)?;
        let mut card = SymbolCard {
            heading: format!(
                "{} — {}, {} (lines {})",
                found.name,
                found.kind.name(),
                found.path,
                found.range
            ),
            signature: Some(found.signature.clone()),
            doc: found.doc.clone(),
            definition: Some(format!("{}  (lines {})", found.path, found.range)),
            ..Default::default()
        };
        {
            let live = self.live_session_for(job, &observed).await?;
            if let Ok(Some(hover)) = live.session.hover(&observed, &bytes, byte_offset).await {
                // The server's hover carries the resolved signature; prefer it over the one-line
                // header when it is a single code line.
                // The server's hover carries the resolved declaration; prefer its first
                // declaration line over the header when it is a single, bounded code line.
                let resolved = hover
                    .lines()
                    .map(str::trim)
                    .find(|line| is_declaration_line(line))
                    .filter(|line| line.len() <= 200);
                if let Some(resolved) = resolved {
                    card.signature = Some(resolved.trim_end_matches(" {").to_owned());
                }
            }
            if want_usages {
                let references = live
                    .session
                    .references(&observed, &bytes, byte_offset)
                    .await
                    .map_err(|_| FailureCode::ProviderUnavailable)?;
                card.usages = self
                    .usage_lines(&worktree_root, &found.path, found.body.start, references)
                    .await;
            }
            if callers_depth > 0 {
                let live = self.live_session_for(job, &observed).await?;
                if let Ok(calls) = live
                    .session
                    .incoming_calls(&observed, &bytes, byte_offset)
                    .await
                {
                    for call in calls {
                        card.callers.push(Call {
                            name: self.call_symbol_path(job, &worktree_root, &call.from).await,
                            file: render::display_path(&worktree_root, &call.from.uri),
                            line: call.from.selection_range.start.line + 1,
                        });
                    }
                }
            }
            if callees_depth > 0 {
                let live = self.live_session_for(job, &observed).await?;
                if let Ok(calls) = live
                    .session
                    .outgoing_calls(&observed, &bytes, byte_offset)
                    .await
                {
                    for call in calls {
                        card.callees.push(Call {
                            name: self.call_symbol_path(job, &worktree_root, &call.to).await,
                            file: render::display_path(&worktree_root, &call.to.uri),
                            line: call.to.selection_range.start.line + 1,
                        });
                    }
                }
            }
        }
        if want_history {
            card.history = history_lines(&worktree_root, &file, found.range).await;
        }
        let authority = self.finish_symbol_job(job, &binding, &observed).await?;
        let text = render::symbol_card_text(&card);
        let (reply, page) =
            ContextPageState::new(text, 0, false, ResultKind::Symbol).next(&job.reference)?;
        self.shared.set_context_page(&job.reference, page);
        Ok((reply, Some(authority), Some(observed)))
    }

    /// `ide.graph {symbol, direction, depth}`: bounded breadth-first call hierarchy.
    pub(super) async fn graph(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let requested = job.parameters["symbol"]
            .as_str()
            .ok_or(FailureCode::UnknownSymbol)?
            .to_owned();
        let symbol = SymbolPath::parse(&requested).map_err(|_| FailureCode::UnknownSymbol)?;
        let file = match symbol.file() {
            Some(file) => file.to_path_buf(),
            None => match self
                .locate_by_name(
                    job,
                    &binding,
                    symbol.name().ok_or(FailureCode::UnknownSymbol)?,
                )
                .await?
            {
                Located::One(file) => file,
                Located::Many(candidates) => {
                    return self.ambiguous(job, &binding, &requested, candidates).await;
                }
            },
        };
        let (observed, bytes) = self.observe(&binding, file.clone()).await?;
        let (outline, worktree_root) = self.outline_of(job, &observed, &bytes).await?;
        let found = match symbol.file() {
            Some(_) => outline.find(&symbol).cloned(),
            None => {
                let candidates = outline.named(symbol.name().unwrap_or_default());
                match candidates.len() {
                    1 => Some(candidates[0].clone()),
                    0 => None,
                    _ => {
                        return self
                            .ambiguous(
                                job,
                                &binding,
                                &requested,
                                candidates
                                    .iter()
                                    .map(|candidate| candidate.path.to_string())
                                    .collect(),
                            )
                            .await;
                    }
                }
            }
        }
        .ok_or(FailureCode::UnknownSymbol)?;
        let root = render::GraphNode {
            path: found.path.to_string(),
            file: file.display().to_string(),
            line: found.range.start,
            is_test: found.kind == lang::SymbolKind::Test,
        };
        let depth = job
            .parameters
            .get("depth")
            .and_then(Value::as_u64)
            .unwrap_or(2) as u8;
        let direction = job
            .parameters
            .get("direction")
            .and_then(Value::as_str)
            .unwrap_or("callers")
            .to_owned();
        let show_tests = job
            .parameters
            .get("tests")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let directions = match direction.as_str() {
            "both" => vec![
                render::GraphDirection::Callers,
                render::GraphDirection::Callees,
            ],
            "callees" => vec![render::GraphDirection::Callees],
            _ => vec![render::GraphDirection::Callers],
        };
        let mut graph = render::CallGraph::new(root);
        let mut indexes = std::collections::HashMap::from([(graph.nodes[0].path.clone(), 0usize)]);
        let mut queue = VecDeque::from([(0usize, file, found, 0u8, directions[0])]);
        if directions.len() == 2 {
            queue.push_back((
                0usize,
                queue[0].1.clone(),
                queue[0].2.clone(),
                0,
                directions[1],
            ));
        }
        while let Some((parent, relative, symbol, level, edge_direction)) = queue.pop_front() {
            if level >= depth || graph.capped {
                continue;
            }
            let (source_observed, source_bytes) = self.observe(&binding, relative.clone()).await?;
            let source = observed_text(&source_observed, &source_bytes)?;
            let offset = name_offset(source, &symbol)?;
            let live = self.live_session_for(job, &source_observed).await?;
            let related_items = match edge_direction {
                render::GraphDirection::Callers => live
                    .session
                    .incoming_calls(&source_observed, &source_bytes, offset)
                    .await
                    .map(|calls| calls.into_iter().map(|call| call.from).collect::<Vec<_>>()),
                render::GraphDirection::Callees => live
                    .session
                    .outgoing_calls(&source_observed, &source_bytes, offset)
                    .await
                    .map(|calls| calls.into_iter().map(|call| call.to).collect::<Vec<_>>()),
            }
            .unwrap_or_default();
            for item in related_items {
                let (node, item_file, item_symbol) =
                    match self.graph_node(job, &worktree_root, item.clone()).await {
                        Ok(resolved) => resolved,
                        // Calls into dependencies or the standard library resolve to files outside
                        // the worktree; they are not graph nodes and must not fail the whole graph.
                        Err(FailureCode::UnknownSymbol) => continue,
                        Err(code) => return Err(code),
                    };
                // Hidden tests stay out of the graph entirely so caps count only rendered
                // nodes; each parent reports what it dropped as one `+N tests` line.
                if node.is_test && !show_tests {
                    *graph
                        .collapsed_tests
                        .entry((parent, edge_direction))
                        .or_default() += 1;
                    continue;
                }
                let (related, is_new) = if let Some(index) = indexes.get(&node.path).copied() {
                    (index, false)
                } else {
                    let Some(index) = graph.add_node(node.clone()) else {
                        break;
                    };
                    indexes.insert(node.path.clone(), index);
                    (index, true)
                };
                if !graph.add_edge(render::GraphEdge {
                    from: parent,
                    to: related,
                    direction: edge_direction,
                }) {
                    break;
                }
                if is_new && level + 1 < depth {
                    queue.push_back((related, item_file, item_symbol, level + 1, edge_direction));
                }
            }
        }
        let authority = self.finish_symbol_job(job, &binding, &observed).await?;
        let text = render::call_graph_text(&graph, &direction, depth);
        let (reply, page) =
            ContextPageState::new(text, 0, false, ResultKind::Graph).next(&job.reference)?;
        self.shared.set_context_page(&job.reference, page);
        Ok((reply, Some(authority), Some(observed)))
    }

    /// Resolves one hierarchy item to its relative source file and normalized outline symbol.
    async fn graph_node(
        &mut self,
        job: &mut Job,
        worktree_root: &Path,
        item: async_lsp::lsp_types::CallHierarchyItem,
    ) -> Result<(render::GraphNode, PathBuf, lang::Symbol), FailureCode> {
        let path = self.call_symbol_path(job, worktree_root, &item).await;
        let absolute = item
            .uri
            .to_file_path()
            .map_err(|_| FailureCode::UnknownSymbol)?;
        let relative = absolute
            .strip_prefix(worktree_root)
            .map_err(|_| FailureCode::UnknownSymbol)?
            .to_path_buf();
        let binding = job.invocation.binding_ref().clone();
        let (observed, bytes) = self.observe(&binding, relative.clone()).await?;
        let (outline, _) = self.outline_of(job, &observed, &bytes).await?;
        let line = item.selection_range.start.line + 1;
        let found = outline
            .symbols
            .iter()
            .flat_map(|symbol| {
                let mut found = Vec::new();
                symbol.walk(&mut |candidate| {
                    if candidate.range.start <= line && line <= candidate.range.end {
                        found.push(candidate.clone());
                    }
                });
                found
            })
            .min_by_key(|candidate| candidate.range.len());
        let Some(found) = found else {
            return Err(FailureCode::UnknownSymbol);
        };
        // Call hierarchy reports struct and enum construction as calls; only callables become
        // graph nodes (tests keep their own kind so `tests: true` can still show them).
        if !matches!(
            found.kind,
            lang::SymbolKind::Function
                | lang::SymbolKind::Method
                | lang::SymbolKind::Constructor
                | lang::SymbolKind::Test
        ) {
            return Err(FailureCode::UnknownSymbol);
        }
        Ok((
            render::GraphNode {
                path,
                file: render::display_path(worktree_root, &item.uri),
                line: found.range.start,
                is_test: found.kind == lang::SymbolKind::Test,
            },
            relative,
            found,
        ))
    }

    /// Document symbols of one observed file through the live session, normalized by the
    /// language module. Returns the outline and the worktree root for path rendering.
    async fn outline_of(
        &mut self,
        job: &mut Job,
        observed: &SourceObservation,
        bytes: &[u8],
    ) -> Result<(Outline, std::path::PathBuf), FailureCode> {
        let language = Lang::for_path(observed.path()).ok_or(FailureCode::ProviderUnavailable)?;
        let support = lang::support(language).ok_or(FailureCode::ProviderUnavailable)?;
        let source = observed_text(observed, bytes)?.to_owned();
        let worktree_root = observed.worktree().worktree_path().to_path_buf();
        let live = self.live_session_for(job, observed).await?;
        let symbols = live
            .session
            .document_symbols(observed, bytes)
            .await
            .map_err(|_| FailureCode::ProviderUnavailable)?;
        Ok((
            support.normalize(observed.path(), &source, symbols),
            worktree_root,
        ))
    }

    /// Resolves a bare symbol name to its definition file through workspace symbols.
    async fn locate_by_name(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        name: &str,
    ) -> Result<Located, FailureCode> {
        // Workspace symbol search needs a session; any file of the language opens it, and the
        // activation root is always inside the worktree.
        let anchor = self.any_source_for_session(job, binding).await?;
        let (observed, _bytes) = self.observe(binding, anchor).await?;
        let worktree_root = observed.worktree().worktree_path().to_path_buf();
        let live = self.live_session_for(job, &observed).await?;
        let mut matches = live
            .session
            .workspace_symbols(name)
            .await
            .map_err(|_| FailureCode::ProviderUnavailable)?
            .into_iter()
            .filter(|symbol| symbol.name == name)
            .filter_map(|symbol| {
                symbol
                    .location
                    .uri
                    .to_file_path()
                    .ok()
                    .and_then(|path| {
                        path.strip_prefix(&worktree_root)
                            .ok()
                            .map(Path::to_path_buf)
                    })
                    .map(|path| (path, symbol.container_name))
            })
            .collect::<Vec<_>>();
        matches.sort();
        matches.dedup();
        match matches.len() {
            0 => Err(FailureCode::UnknownSymbol),
            1 => Ok(Located::One(matches.remove(0).0)),
            _ => Ok(Located::Many(
                matches
                    .iter()
                    .map(|(path, container)| match container {
                        Some(container) => format!("{}#{container}/{name}", path.display()),
                        None => format!("{}#{name}", path.display()),
                    })
                    .collect(),
            )),
        }
    }

    /// Answers an ambiguous name with its candidates instead of guessing.
    async fn ambiguous(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        requested: &str,
        candidates: Vec<String>,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let authority = self.authority(binding).await?;
        self.shared.active(binding)?;
        let mut text = format!(
            "ambiguous_symbol: {requested} matches {} symbols; repeat ide.symbol with one exact path:\n",
            candidates.len()
        );
        for candidate in candidates.iter().take(20) {
            text.push_str(&format!("  {candidate}\n"));
        }
        if candidates.len() > 20 {
            text.push_str(&format!("  … {} more\n", candidates.len() - 20));
        }
        let (reply, page) =
            ContextPageState::new(text, 0, false, ResultKind::Symbol).next(&job.reference)?;
        self.shared.set_context_page(&job.reference, page);
        Ok((reply, Some(authority), None))
    }

    /// Picks the first Rust source of the worktree as the document that opens a session.
    async fn any_source_for_session(
        &mut self,
        _job: &mut Job,
        binding: &BindingRef,
    ) -> Result<std::path::PathBuf, FailureCode> {
        let authority = self.authority(binding).await?;
        let root = authority.worktree().worktree_path().to_path_buf();
        for candidate in ["src/lib.rs", "src/main.rs"] {
            if root.join(candidate).is_file() {
                return Ok(std::path::PathBuf::from(candidate));
            }
        }
        Err(FailureCode::ProviderUnavailable)
    }

    /// Usage lines for reference locations: relative path, line, trimmed text, test flag.
    async fn usage_lines(
        &mut self,
        worktree_root: &Path,
        definition: &SymbolPath,
        definition_line: u32,
        references: Vec<async_lsp::lsp_types::Location>,
    ) -> Vec<Usage> {
        let mut cache: std::collections::BTreeMap<std::path::PathBuf, String> =
            std::collections::BTreeMap::new();
        let mut usages = Vec::new();
        for location in references {
            let Ok(absolute) = location.uri.to_file_path() else {
                continue;
            };
            let Ok(relative) = absolute.strip_prefix(worktree_root) else {
                continue;
            };
            let relative = relative.to_path_buf();
            let line = location.range.start.line + 1;
            // Skip the declaration itself: it is the definition, not a usage.
            if definition.file() == Some(relative.as_path()) && line == definition_line {
                continue;
            }
            let text = match cache.get(&relative) {
                Some(source) => render::line_text(source, line),
                None => {
                    let source = std::fs::read_to_string(&absolute).unwrap_or_default();
                    let text = render::line_text(&source, line);
                    cache.insert(relative.clone(), source);
                    text
                }
            };
            let is_test = Lang::for_path(&relative)
                .and_then(lang::support)
                .is_some_and(|support| support.is_test_file(&relative));
            usages.push(Usage {
                file: relative.display().to_string(),
                line,
                text,
                is_test,
            });
        }
        usages
    }

    /// Resolves a call hierarchy item to its enclosing outline path, falling back to its LSP name.
    async fn call_symbol_path(
        &mut self,
        job: &mut Job,
        worktree_root: &Path,
        item: &async_lsp::lsp_types::CallHierarchyItem,
    ) -> String {
        let Ok(absolute) = item.uri.to_file_path() else {
            return item.name.clone();
        };
        let Ok(relative) = absolute.strip_prefix(worktree_root) else {
            return item.name.clone();
        };
        let binding = job.invocation.binding_ref().clone();
        let Ok((observed, bytes)) = self.observe(&binding, relative.to_path_buf()).await else {
            return item.name.clone();
        };
        let Ok((outline, _)) = self.outline_of(job, &observed, &bytes).await else {
            return item.name.clone();
        };
        let line = item.selection_range.start.line + 1;
        let mut enclosing: Option<&crate::lang::Symbol> = None;
        for symbol in &outline.symbols {
            symbol.walk(&mut |candidate| {
                if candidate.range.start <= line
                    && line <= candidate.range.end
                    && enclosing.is_none_or(|old| candidate.range.len() < old.range.len())
                {
                    enclosing = Some(candidate);
                }
            });
        }
        enclosing.map_or_else(|| item.name.clone(), |symbol| symbol.path.to_string())
    }

    /// Shared tail of every symbol job: deadline, authority, liveness and source checks.
    ///
    /// No native-epoch fence here: a symbol job may wait tens of seconds for a loading
    /// language server, and a Codex host posts a native hint for every shell command the agent
    /// runs meanwhile (its own `sleep` between polls), which would discard a correct result.
    /// What the result depends on is the observed file, and `source_matches` re-reads it.
    async fn finish_symbol_job(
        &mut self,
        job: &Job,
        binding: &BindingRef,
        observed: &SourceObservation,
    ) -> Result<AuthorityStamp, FailureCode> {
        if tokio::time::Instant::now() >= job.deadline {
            return Err(FailureCode::Deadline);
        }
        let authority = self.authority(binding).await?;
        self.shared.active(binding)?;
        if !source_matches(observed) {
            return Err(FailureCode::SourceUnavailable);
        }
        Ok(authority)
    }
}

/// Whether a hover line is a declaration rather than a module path or a code fence.
fn is_declaration_line(line: &str) -> bool {
    const KEYWORDS: [&str; 20] = [
        "pub ",
        "fn ",
        "struct ",
        "enum ",
        "trait ",
        "impl ",
        "type ",
        "const ",
        "static ",
        "mod ",
        "class ",
        "def ",
        "async ",
        "func ",
        "interface ",
        "export ",
        "let ",
        "var ",
        "function ",
        "unsafe ",
    ];
    !line.is_empty()
        && !line.starts_with("```")
        && KEYWORDS.iter().any(|keyword| line.starts_with(keyword))
}

/// Where a bare name resolved to.
enum Located {
    One(std::path::PathBuf),
    Many(Vec<String>),
}

/// Byte offset of the symbol's name on its declaration line, for position-based requests.
fn name_offset(source: &str, symbol: &lang::Symbol) -> Result<usize, FailureCode> {
    let mut offset = 0usize;
    for (index, line) in source.split_inclusive('\n').enumerate() {
        let number = index as u32 + 1;
        if number >= symbol.body.start
            && number <= symbol.range.end
            && let Some(column) = find_word(line, &symbol.name)
        {
            return Ok(offset + column);
        }
        offset += line.len();
    }
    Err(FailureCode::UnknownSymbol)
}

/// Finds `word` in `line` at a word boundary.
fn find_word(line: &str, word: &str) -> Option<usize> {
    let mut start = 0;
    while let Some(found) = line[start..].find(word) {
        let at = start + found;
        let before = line[..at].chars().next_back();
        let after = line[at + word.len()..].chars().next();
        let boundary = |ch: Option<char>| ch.is_none_or(|ch| !(ch.is_alphanumeric() || ch == '_'));
        if boundary(before) && boundary(after) {
            return Some(at);
        }
        start = at + word.len();
    }
    None
}

/// The observed bytes as text; symbol tools need UTF-8 sources.
fn observed_text<'a>(
    observed: &SourceObservation,
    bytes: &'a [u8],
) -> Result<&'a str, FailureCode> {
    if observed.bytes().is_none() {
        return Err(FailureCode::SourceUnavailable);
    }
    std::str::from_utf8(bytes).map_err(|_| FailureCode::SourceUnavailable)
}

// ---------------------------------------------------------------------------------------------
// ide.edit by symbol (v0.4): replace / insert / delete, plus a line-range replace.
// ---------------------------------------------------------------------------------------------

impl Worker<'_> {
    /// `ide.edit` with `symbol` (+ `op`) or `path` + `lines`: splices the file in memory, formats
    /// the candidate when the project has a formatter, then writes it through the ordinary
    /// stale-safe edit path with the fresh observation as the base.
    pub(super) async fn edit_by_symbol(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let operation_id = job.parameters["operation_id"]
            .as_str()
            .ok_or(FailureCode::Internal)?
            .to_owned();
        let op = job
            .parameters
            .get("op")
            .and_then(Value::as_str)
            .unwrap_or("replace")
            .to_owned();
        if op == "rename" {
            return self.rename_symbol(job).await;
        }
        let content = job
            .parameters
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_owned);
        // Resolve the file and the line span the operation touches.
        let (file, splice) = match job.parameters.get("symbol").and_then(Value::as_str) {
            Some(symbol) => {
                let symbol = SymbolPath::parse(symbol).map_err(|_| FailureCode::UnknownSymbol)?;
                let file = symbol
                    .file()
                    .ok_or(FailureCode::UnknownSymbol)?
                    .to_path_buf();
                let (observed, bytes) = self.observe(&binding, file.clone()).await?;
                let source = observed_text(&observed, &bytes)?.to_owned();
                let (outline, _) = self.outline_of(job, &observed, &bytes).await?;
                let splice = match op.as_str() {
                    "insert" => {
                        let where_ = match job.parameters.get("where").and_then(Value::as_str) {
                            Some("before") => lang::InsertWhere::Before,
                            Some("after") => lang::InsertWhere::After,
                            Some("first") => lang::InsertWhere::First,
                            Some("last") => lang::InsertWhere::Last,
                            _ => return Err(FailureCode::Internal),
                        };
                        let support = lang::support(outline.language)
                            .ok_or(FailureCode::ProviderUnavailable)?;
                        let site = support
                            .insert_site(&source, &outline, &symbol, where_)
                            .map_err(|error| match error {
                                lang::LangError::UnknownSymbol(_) => FailureCode::UnknownSymbol,
                                _ => FailureCode::Internal,
                            })?;
                        Splice::Insert(site)
                    }
                    _ => {
                        let found = outline.find(&symbol).ok_or(FailureCode::UnknownSymbol)?;
                        Splice::Replace(found.range)
                    }
                };
                (file, splice)
            }
            None => {
                let path = job.parameters["path"]
                    .as_str()
                    .ok_or(FailureCode::Internal)?
                    .to_owned();
                let range = job
                    .parameters
                    .get("lines")
                    .and_then(Value::as_str)
                    .and_then(crate::assistance::facade::parse_line_range)
                    .ok_or(FailureCode::Internal)?;
                (std::path::PathBuf::from(path), Splice::Replace(range))
            }
        };
        // Observe again right before splicing so the base is the exact text being replaced.
        let (observed, bytes) = self.observe(&binding, file.clone()).await?;
        let source = observed_text(&observed, &bytes)?.to_owned();
        let total = lang::line_count(&source);
        let candidate = match (&op[..], &splice) {
            ("delete", Splice::Replace(range)) => {
                if range.start > total {
                    return Err(FailureCode::UnknownSymbol);
                }
                splice_lines(&source, *range, "")
            }
            ("insert", Splice::Insert(site)) => {
                let content = content.ok_or(FailureCode::Internal)?;
                insert_lines(&source, site, &content)
            }
            (_, Splice::Replace(range)) => {
                let content = content.ok_or(FailureCode::Internal)?;
                if range.start > total {
                    return Err(FailureCode::SourceUnavailable);
                }
                splice_lines(&source, *range, &content)
            }
            _ => return Err(FailureCode::Internal),
        };
        let candidate = self.format_candidate(&observed, &file, candidate).await;
        let request = EditRequest::new(
            &operation_id,
            file.display().to_string(),
            &job.reference,
            &candidate,
        )
        .map_err(|_| FailureCode::Internal)?;
        let prepared = match self.edits.prepare(request.clone()).await {
            Ok(PrepareAdmission::Prepared(prepared)) => prepared,
            Ok(
                PrepareAdmission::Settled(result)
                | PrepareAdmission::ConflictingDuplicate(result)
                | PrepareAdmission::OutcomeUnknown(result),
            ) => {
                let authority = self.authority(&binding).await.ok();
                return Ok((
                    PeerReply::Edit {
                        result,
                        diagnostics: EditDiagnostics::Unknown {},
                    },
                    authority,
                    None,
                ));
            }
            Err(_) => return Err(FailureCode::Internal),
        };
        self.edit_with_source(job, request, prepared, observed)
            .await
    }

    /// Runs the project's stdin formatter over a candidate text; the candidate is returned
    /// unchanged when there is no formatter, it fails, or it takes longer than ten seconds.
    async fn format_candidate(
        &mut self,
        observed: &SourceObservation,
        file: &Path,
        candidate: String,
    ) -> String {
        let Some(language) = Lang::for_path(file) else {
            return candidate;
        };
        let Some(support) = lang::support(language) else {
            return candidate;
        };
        let root = observed.worktree().worktree_path().to_path_buf();
        let Some(project) = support.detect(&root) else {
            return candidate;
        };
        let Some(argv) = support.format_stdin_command(&project, file) else {
            return candidate;
        };
        let Some((program, args)) = argv.split_first() else {
            return candidate;
        };
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .current_dir(&root)
            .env("PATH", formatter_path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let Ok(mut child) = command.spawn() else {
            return candidate;
        };
        let Some(mut stdin) = child.stdin.take() else {
            return candidate;
        };
        let input = candidate.clone();
        let writer = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(input.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
        let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()).await;
        writer.abort();
        match output {
            Ok(Ok(output)) if output.status.success() && !output.stdout.is_empty() => {
                String::from_utf8(output.stdout).unwrap_or(candidate)
            }
            _ => candidate,
        }
    }

    /// `ide.edit {op:"rename", symbol, new_name}`: the language server computes the project-wide
    /// edit; every touched file is written through the stale-safe edit path.
    async fn rename_symbol(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let operation_id = job.parameters["operation_id"]
            .as_str()
            .ok_or(FailureCode::Internal)?
            .to_owned();
        let new_name = job.parameters["new_name"]
            .as_str()
            .ok_or(FailureCode::Internal)?
            .to_owned();
        let symbol = SymbolPath::parse(
            job.parameters["symbol"]
                .as_str()
                .ok_or(FailureCode::UnknownSymbol)?,
        )
        .map_err(|_| FailureCode::UnknownSymbol)?;
        let file = symbol
            .file()
            .ok_or(FailureCode::UnknownSymbol)?
            .to_path_buf();
        let (observed, bytes) = self.observe(&binding, file.clone()).await?;
        let (outline, worktree_root) = self.outline_of(job, &observed, &bytes).await?;
        let found = outline
            .find(&symbol)
            .cloned()
            .ok_or(FailureCode::UnknownSymbol)?;
        let source = observed_text(&observed, &bytes)?;
        let byte_offset = name_offset(source, &found)?;
        let (edit, encoding) = {
            let live = self.live_session_for(job, &observed).await?;
            let encoding = live.session.capabilities().position_encoding.clone();
            let edit = live
                .session
                .rename(&observed, &bytes, byte_offset, &new_name)
                .await
                .map_err(|_| FailureCode::ProviderUnavailable)?
                .ok_or(FailureCode::ProviderUnavailable)?;
            (edit, encoding)
        };
        let grouped = lang::edits::group_workspace_edit(edit);
        if !grouped.unsupported.is_empty() {
            job.failure_detail = Some(grouped.unsupported.join(", "));
            return Err(FailureCode::ProviderUnavailable);
        }
        let mut summary = Vec::new();
        let mut written = 0usize;
        let mut last: Option<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>)> = None;
        for (index, file_edits) in grouped.files.iter().enumerate() {
            let Ok(absolute) = file_edits.uri.to_file_path() else {
                continue;
            };
            let Ok(relative) = absolute.strip_prefix(&worktree_root) else {
                continue;
            };
            let relative = relative.to_path_buf();
            let (observed, bytes) = self.observe(&binding, relative.clone()).await?;
            let source = observed_text(&observed, &bytes)?;
            let candidate = lang::edits::apply_text_edits(source, &file_edits.edits, &encoding)
                .map_err(|_| FailureCode::Internal)?;
            let request = EditRequest::new(
                format!("{operation_id}/{index}"),
                relative.display().to_string(),
                &job.reference,
                &candidate,
            )
            .map_err(|_| FailureCode::Internal)?;
            let prepared = match self.edits.prepare(request.clone()).await {
                Ok(PrepareAdmission::Prepared(prepared)) => prepared,
                _ => return Err(FailureCode::Internal),
            };
            let outcome = self
                .edit_with_source(job, request, prepared, observed)
                .await?;
            if let PeerReply::Edit { result, .. } = &outcome.0 {
                summary.push(format!(
                    "{} ({}, {:?})",
                    relative.display(),
                    file_edits.edits.len(),
                    result.outcome
                ));
                written += 1;
            }
            last = Some(outcome);
        }
        let authority = self.authority(&binding).await?;
        let mut text = format!(
            "rename: {} → {new_name}; {} edits in {written} files\n",
            found.name,
            grouped
                .files
                .iter()
                .map(|file| file.edits.len())
                .sum::<usize>()
        );
        for line in summary.iter().take(30) {
            text.push_str(&format!("  {line}\n"));
        }
        if let Some((PeerReply::Edit { diagnostics, .. }, _, _)) = &last {
            text.push_str(&format!("diagnostics (last file): {diagnostics:?}\n"));
        }
        let (reply, page) =
            ContextPageState::new(text, 0, false, ResultKind::Symbol).next(&job.reference)?;
        self.shared.set_context_page(&job.reference, page);
        Ok((reply, Some(authority), last.and_then(|outcome| outcome.2)))
    }
}

/// What a symbol edit replaces or where it inserts.
enum Splice {
    Replace(LineRange),
    Insert(lang::InsertSite),
}

/// Replaces the inclusive line range with `content` (a trailing newline is added when missing;
/// an empty content deletes the lines).
fn splice_lines(source: &str, range: LineRange, content: &str) -> String {
    let mut out = String::with_capacity(source.len() + content.len());
    let mut replaced = false;
    for (index, line) in source.split_inclusive('\n').enumerate() {
        let number = index as u32 + 1;
        if number >= range.start && number <= range.end {
            if !replaced {
                push_block(&mut out, content);
                replaced = true;
            }
            continue;
        }
        out.push_str(line);
    }
    if !replaced {
        push_block(&mut out, content);
    }
    out
}

/// Inserts `content` before `site.line` with the site's indentation and blank lines.
fn insert_lines(source: &str, site: &lang::InsertSite, content: &str) -> String {
    let mut out = String::with_capacity(source.len() + content.len() + 64);
    let mut inserted = false;
    let block = indent_block(content, &site.indent);
    for (index, line) in source.split_inclusive('\n').enumerate() {
        let number = index as u32 + 1;
        if number == site.line && !inserted {
            for _ in 0..site.blank_before {
                out.push('\n');
            }
            push_block(&mut out, &block);
            for _ in 0..site.blank_after {
                out.push('\n');
            }
            inserted = true;
        }
        out.push_str(line);
    }
    if !inserted {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        for _ in 0..site.blank_before {
            out.push('\n');
        }
        push_block(&mut out, &block);
    }
    out
}

/// Re-indents a block so its least-indented non-blank line sits at `indent`.
fn indent_block(content: &str, indent: &str) -> String {
    let common = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    content
        .lines()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                format!("{indent}{}", &line[common.min(line.len())..])
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn push_block(out: &mut String, content: &str) {
    if content.is_empty() {
        return;
    }
    out.push_str(content);
    if !content.ends_with('\n') {
        out.push('\n');
    }
}

/// PATH for formatters: the toolchain directories the daemon itself was configured with plus the
/// system directories, never the agent's shell environment.
fn formatter_path() -> String {
    let mut parts = vec![];
    if let Ok(home) = std::env::var("HOME") {
        parts.push(format!("{home}/.cargo/bin"));
        parts.push(format!("{home}/.local/bin"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        parts.push(path.to_string_lossy().into_owned());
    }
    parts.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"].map(String::from));
    parts.join(":")
}

/// History entries kept on one symbol card.
const MAX_HISTORY: usize = 3;
/// Bytes of `git log -L` stdout captured before truncation.
const MAX_HISTORY_OUTPUT: u64 = 64 * 1024;
/// Characters per history entry.
const MAX_HISTORY_CHARS: usize = 100;

/// Recent commits touching one definition range, as `sha date subject` lines.
///
/// Runs `/usr/bin/git log -L` against the worktree exactly like every other daemon git call.
/// Anything that goes wrong — the file is untracked, the directory is not a checkout, the child
/// fails or exceeds the three-second budget — yields an empty list: history is an opt-in garnish
/// and must never fail the card.
async fn history_lines(worktree: &Path, file: &Path, range: LineRange) -> Vec<String> {
    // Absolute program path, like every other git call of the daemon; `core.fsmonitor` is forced
    // off because a repository-configured fsmonitor hook would otherwise run unconfined here.
    let mut command = tokio::process::Command::new("/usr/bin/git");
    command
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(["-c", "core.fsmonitor=false"])
        .arg("-C")
        .arg(worktree)
        .args([
            "log",
            "-n",
            "3",
            "--no-merges",
            "--date=short",
            "--format=%h %ad %s",
            "-L",
            &format!("{},{}:{}", range.start, range.end, file.display()),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let Ok(mut child) = command.spawn() else {
        return Vec::new();
    };
    let Some(stdout) = child.stdout.take() else {
        return Vec::new();
    };
    let read = tokio::time::timeout(Duration::from_secs(3), async move {
        let mut bytes = Vec::new();
        use tokio::io::AsyncReadExt;
        if stdout
            .take(MAX_HISTORY_OUTPUT)
            .read_to_end(&mut bytes)
            .await
            .is_err()
        {
            return None;
        }
        child.wait().await.ok().map(|status| (bytes, status))
    })
    .await;
    match read {
        Ok(Some((bytes, status))) if status.success() => {
            parse_history(&String::from_utf8_lossy(&bytes))
        }
        _ => Vec::new(),
    }
}

/// Picks `%h %ad %s` commit headers out of the diff hunks `git log -L` interleaves them with,
/// deduplicated in output order, clipped and capped for the card.
fn parse_history(output: &str) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    for line in output.lines().filter(|line| commit_header(line)) {
        let entry: String = line.chars().take(MAX_HISTORY_CHARS).collect();
        if !entries.contains(&entry) {
            entries.push(entry);
        }
        if entries.len() == MAX_HISTORY {
            break;
        }
    }
    entries
}

/// Matches `^[0-9a-f]{7,} \d{4}-\d{2}-\d{2} ` — the header `--format=%h %ad %s` prints above
/// each commit's diff in `git log -L` output.
fn commit_header(line: &str) -> bool {
    let mut parts = line.splitn(3, ' ');
    let (Some(sha), Some(date), Some(_)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    sha.len() >= 7
        && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
        && date.len() == 10
        && date.bytes().enumerate().all(|(index, byte)| match index {
            4 | 7 => byte == b'-',
            _ => byte.is_ascii_digit(),
        })
}

#[cfg(test)]
mod splice_tests {
    use super::*;

    #[test]
    fn replace_delete_and_insert_keep_the_rest_of_the_file() {
        let source = "a\nb\nc\nd\n";
        assert_eq!(
            splice_lines(source, LineRange::new(2, 3), "X\nY"),
            "a\nX\nY\nd\n"
        );
        assert_eq!(splice_lines(source, LineRange::new(2, 3), ""), "a\nd\n");
        assert_eq!(
            splice_lines(source, LineRange::new(4, 4), "Z\n"),
            "a\nb\nc\nZ\n"
        );
        let site = lang::InsertSite {
            line: 3,
            indent: "    ".into(),
            blank_before: 1,
            blank_after: 0,
        };
        assert_eq!(
            insert_lines(source, &site, "fn g() {\n    x\n}"),
            "a\nb\n\n    fn g() {\n        x\n    }\nc\nd\n"
        );
        let append = lang::InsertSite {
            line: 5,
            indent: String::new(),
            blank_before: 1,
            blank_after: 0,
        };
        assert_eq!(insert_lines(source, &append, "e"), "a\nb\nc\nd\n\ne\n");
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;

    /// Realistic `git log -L` output: one `%h %ad %s` header per commit, each followed by its
    /// diff hunks and function context, plus the trailing file header noise.
    #[test]
    fn parser_keeps_only_deduped_clipped_commit_headers() {
        let output = "\
3f9c2ab1e0d7 2026-09-26 fix: tighten value bound
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,1 +1,1 @@
-pub fn value() -> i32 { dep::shared_value() }
+pub fn value() -> i32 { dep::shared_value() + 0 }
8b41de0c99aa 2026-09-25 feat: cross-crate fixture
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,2 @@
+pub fn value() -> i32 { dep::shared_value() }
 pub fn caller() -> i32 { value() }
abcdef1 2026-1 not a date line
2026-09-25 8b41de0 reversed shape is not a header
short 2026-09-25 nope
";
        assert_eq!(
            parse_history(output),
            vec![
                "3f9c2ab1e0d7 2026-09-26 fix: tighten value bound",
                "8b41de0c99aa 2026-09-25 feat: cross-crate fixture",
            ]
        );
    }

    #[test]
    fn parser_dedupes_clips_and_caps_at_three_entries() {
        let line = format!("{} 2026-09-25 {}", "a1b2c3d", "subject ".repeat(30));
        let output = [line.as_str(), line.as_str(), "e4f5a6b 2026-09-24 second"].join("\n");
        let parsed = parse_history(&output);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].chars().count(), MAX_HISTORY_CHARS);
        assert!(!parsed[0].ends_with('…'));
        let long = parse_history(
            &(0..5)
                .map(|index| format!("0a1b2c{index} 2026-09-25 c{index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        assert_eq!(long.len(), MAX_HISTORY);
    }
}
