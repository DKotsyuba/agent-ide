//! Symbol-addressed worker jobs: `ide.outline`, `ide.read`, `ide.symbol` and `ide.graph` (v0.4).
//!
//! Every job observes the source through Workspace exactly like `ide.context`, asks the binding's
//! live language server for document symbols, normalizes them through the language module and
//! renders the compact text the contract specifies. Results are retained with the source
//! observation so a later `ide.edit` may name them as `source_ref`.

use super::*;
use crate::intelligence::server::ProviderJob as _;
use crate::lang::{
    self, Language as Lang, LineRange, Outline, SymbolPath, TestId,
    render::{self, Call, SymbolCard, Usage},
};
use crate::workspace::{
    authority::WorktreeRef,
    observation::{
        MAX_SOURCE_BYTES, ObservationRef, SourceBytes, SourceCoverage, SourceObservation,
        SourceReadLimits, SourceRevision, read_authorized_source,
    },
};
use std::collections::VecDeque;

impl Worker<'_> {
    /// Finds referencing tests and counts outline tests in the symbol's own file. Test names come
    /// from the language's [`LanguageSupport::test_id`](crate::lang::LanguageSupport::test_id)
    /// (some derive module paths from the file, most keep their outline naming). A bare
    /// name arrives with its definition file already resolved through `locate_by_name` and the
    /// outline matches it at any depth.
    pub(super) async fn tests_referencing_symbol(
        &mut self,
        job: &mut Job,
        requested: &str,
        resolved: Option<&std::path::PathBuf>,
    ) -> Result<(Vec<TestId>, Lang, usize), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let symbol = SymbolPath::parse(requested).map_err(|_| FailureCode::UnknownSymbol)?;
        let file = match symbol.file() {
            Some(file) => file.to_path_buf(),
            None => resolved.cloned().ok_or(FailureCode::UnknownSymbol)?,
        };
        let (observed, bytes) = self.observe(&binding, file).await?;
        let (outline, root, lexical) = self.outline_of(job, &observed, &bytes).await?;
        let found = if symbol.file().is_some() {
            outline
                .find(&symbol)
                .cloned()
                .ok_or_else(|| missing_symbol(job, lexical))?
        } else {
            let name = symbol.name().ok_or(FailureCode::UnknownSymbol)?;
            match outline.named(name).as_slice() {
                [found] => (*found).clone(),
                [] => return Err(missing_symbol(job, lexical)),
                _ => return Err(FailureCode::UnknownSymbol),
            }
        };
        let mut file_test_count = 0;
        for candidate in &outline.symbols {
            candidate.walk(&mut |candidate| {
                file_test_count += usize::from(candidate.kind == crate::lang::SymbolKind::Test);
            });
        }
        let offset = name_offset(observed_text(&observed, &bytes)?, &found)?;
        let refs = self
            .live_session_for(job, &observed)
            .await?
            .session
            .references(&observed, &bytes, offset)
            .await
            .map_err(|_| FailureCode::ProviderUnavailable)?;
        let language = Lang::for_path(observed.path()).ok_or(FailureCode::ProviderUnavailable)?;
        // A test is whatever the outline marks as one, wherever it lives: some languages keep
        // most unit tests in a test module of the file under test, so the test-file convention
        // alone would miss them. One outline per referenced file.
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
                let (test_outline, _, _) =
                    self.outline_of(job, &test_observed, &test_bytes).await?;
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
                let name = language.support().test_id(&relative, &outline_path);
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
        if let Some(code) = no_such_file(observed.state(), &path) {
            return Err(code);
        }
        let (outline, _, lexical) = self.outline_of(job, &observed, &bytes).await?;
        let authority = self.finish_symbol_job(job, &binding, &observed).await?;
        let mut text = match job.parameters.get("kinds").and_then(Value::as_str) {
            Some(requested) => {
                let kinds: Vec<lang::SymbolKind> = requested
                    .split(',')
                    .filter_map(lang::SymbolKind::from_name)
                    .collect();
                render::filtered_outline_text(&outline, &kinds, requested)
            }
            None => render::outline_text(&outline),
        };
        if let Some(why) = lexical.as_ref()
            && let Some(note) = self.lexical_note(observed.path(), why)
        {
            text.push_str(&note);
            text.push('\n');
        }
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
        let requested = job
            .parameters
            .get("symbol")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let sigil = requested.as_deref().and_then(links::sigil_address);
        let mut lexical = None;
        let (path, range, title) = match (requested.as_deref(), sigil) {
            // A sigil address reads the name's first indexed definition.
            (_, Some((namespace, name))) => {
                self.sigil_definition(job, &binding, namespace, name)
                    .await?
            }
            (Some(symbol), None) => {
                let symbol = SymbolPath::parse(symbol).map_err(|_| FailureCode::UnknownSymbol)?;
                let file = symbol
                    .file()
                    .ok_or(FailureCode::UnknownSymbol)?
                    .to_path_buf();
                let (observed, bytes) = self.observe(&binding, file.clone()).await?;
                let (outline, _, from_text) = self.outline_of(job, &observed, &bytes).await?;
                let found = outline
                    .find(&symbol)
                    .ok_or_else(|| missing_symbol(job, from_text.clone()))?;
                lexical = from_text
                    .as_ref()
                    .and_then(|why| self.lexical_note(observed.path(), why));
                (file, found.range, symbol.to_string())
            }
            (None, None) => {
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
        if let Some(code) = no_such_file(observed.state(), &path.to_string_lossy()) {
            return Err(code);
        }
        let source = observed_text(&observed, &bytes)?;
        let total = lang::line_count(source);
        if range.start > total {
            return Err(FailureCode::SourceUnavailable);
        }
        let range = LineRange::new(range.start, range.end.min(total));
        let authority = self.finish_symbol_job(job, &binding, &observed).await?;
        let mut text = render::read_text(&path, Some(&title), range, source);
        if let Some(note) = lexical {
            text.push_str(&note);
            text.push('\n');
        }
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
        // Resolve the definition file: a sigil address goes to the name index; a bare name is
        // looked up in the index and through workspace symbols, and both answers are merged.
        let file = match symbol.file() {
            Some(file) => file.to_path_buf(),
            None => {
                if let Some((namespace, name)) = links::sigil_address(&requested) {
                    let keys = self
                        .indexed_keys(job, &binding, name, Some(namespace))
                        .await?;
                    if keys.is_empty() {
                        job.failure_detail = Some("symbol:not_indexed".to_owned());
                        return Err(FailureCode::UnknownSymbol);
                    }
                    let keys = keys.into_iter().map(|summary| summary.key).collect();
                    return self.name_card(job, &binding, keys).await;
                }
                let name = symbol.name().ok_or(FailureCode::UnknownSymbol)?.to_owned();
                let indexed = self.indexed_keys(job, &binding, &name, None).await?;
                match self.locate_by_name(job, &binding, &name).await {
                    Ok(Located::One(file)) if indexed.is_empty() => file,
                    Ok(Located::Many(candidates)) if indexed.is_empty() => {
                        return self.ambiguous(job, &binding, &requested, candidates).await;
                    }
                    Ok(located) => {
                        let mut candidates = match located {
                            Located::One(file) => vec![format!("{}#{name}", file.display())],
                            Located::Many(candidates) => candidates,
                        };
                        candidates.extend(indexed.iter().map(links::candidate));
                        return self.ambiguous(job, &binding, &requested, candidates).await;
                    }
                    Err(FailureCode::UnknownSymbol | FailureCode::ProviderUnavailable)
                        if !indexed.is_empty() =>
                    {
                        job.failure_detail = None;
                        let keys = indexed.into_iter().map(|summary| summary.key).collect();
                        return self.name_card(job, &binding, keys).await;
                    }
                    Err(code) => return Err(code),
                }
            }
        };
        let (observed, bytes) = self.observe(&binding, file.clone()).await?;
        let (outline, worktree_root, lexical) = self.outline_of(job, &observed, &bytes).await?;
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
        .ok_or_else(|| missing_symbol(job, lexical.clone()))?;
        let source = observed_text(&observed, &bytes)?;
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
        if self.session_server(observed.path()).is_none() {
            // Outlined from source: nothing answers hover, references or call hierarchy. A
            // language with name facts shows index-backed usages instead (below).
            let language = outline.language;
            if want_usages && language.names().is_none() {
                card.usages_note = Some(format!("unavailable ({language} has no language server)"));
            }
            if callers_depth > 0 {
                card.callers_note = Some(format!("unavailable ({language} has no call hierarchy)"));
            }
            if callees_depth > 0 {
                card.callees_note = Some(format!("unavailable ({language} has no call hierarchy)"));
            }
        } else if matches!(lexical.as_ref(), Some(Lexical::Unavailable))
            && let Some(server) = self.session_server(observed.path())
        {
            // The outline above came from source because the registered server's workspace
            // failed to load, so no live session can answer hover, references or call
            // hierarchy. The card's definition facts already come from that outline; every
            // section a live session would answer says why it is missing instead of failing the
            // whole call — outline and read answer from source in exactly this state. A
            // language with name facts shows index-backed usages instead (below).
            if want_usages && outline.language.names().is_none() {
                card.usages_note = Some(format!(
                    "unavailable ({} workspace failed to load)",
                    server.name()
                ));
            }
            if callers_depth > 0 {
                card.callers_note = Some(format!(
                    "unavailable ({} workspace failed to load)",
                    server.name()
                ));
            }
            if callees_depth > 0 {
                card.callees_note = Some(format!(
                    "unavailable ({} workspace failed to load)",
                    server.name()
                ));
            }
        } else {
            let byte_offset = name_offset(source, &found)?;
            let server_name = self
                .session_server(observed.path())
                .map(|server| server.name());
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
                let references = match live
                    .session
                    .references(&observed, &bytes, byte_offset)
                    .await
                {
                    Ok(references) => references,
                    Err(_) => {
                        // The ready session failed this exchange; the stage names the request so
                        // the refusal's reply can say what still answers and how to recover.
                        if let Some(name) = server_name {
                            job.set_stage_failure(
                                &FailureCode::ProviderUnavailable,
                                &format!("{name}: references request failed"),
                            );
                        }
                        return Err(FailureCode::ProviderUnavailable);
                    }
                };
                card.usages = self
                    .usage_lines(
                        job,
                        &worktree_root,
                        &found.path,
                        found.body.start,
                        references,
                    )
                    .await;
                // Some servers answer references with an empty list when nothing names the symbol
                // explicitly (a constructor is only ever called through its class); that zero
                // must be reported, not silently omitted.
                if card.usages.is_empty()
                    && self
                        .session_server(observed.path())
                        .is_some_and(|server| server.reports_empty_references())
                {
                    card.report_empty_usages = true;
                }
            }
            if callers_depth > 0 {
                // A server whose call hierarchy is unreliable (a constructor answers nothing while
                // a plain method answers) would make graphs through callers silently miss call
                // sites; say unavailable instead of printing a partial answer.
                if let Some(server) = self
                    .session_server(observed.path())
                    .filter(|server| !server.call_hierarchy())
                {
                    card.callers_note = Some(format!(
                        "unavailable ({} has no call hierarchy)",
                        server.name()
                    ));
                } else {
                    let calls = match self.live_session_for(job, &observed).await {
                        Ok(live) => match live
                            .session
                            .prepare_call_hierarchy(&observed, &bytes, byte_offset)
                            .await
                        {
                            Err(_) => Err("prepare call hierarchy request failed"),
                            Ok(items) => match items.into_iter().next() {
                                Some(item) => live
                                    .session
                                    .incoming_calls_for(item)
                                    .await
                                    .map_err(|_| "incoming calls request failed"),
                                None => Ok(Vec::new()),
                            },
                        },
                        Err(FailureCode::ProviderLoading) => Err(
                            "callers are not available yet because the language server is still indexing; repeat ide.symbol later",
                        ),
                        Err(FailureCode::ProviderUnavailable) => {
                            Err("the language server could not load the workspace")
                        }
                        Err(other) => return Err(other),
                    };
                    match calls {
                        Ok(calls) => {
                            for call in calls {
                                card.callers.push(Call {
                                    name: self
                                        .call_symbol_path(job, &worktree_root, &call.from)
                                        .await,
                                    file: render::display_path(&worktree_root, &call.from.uri),
                                    line: call.from.selection_range.start.line + 1,
                                });
                            }
                        }
                        Err(reason) => {
                            card.callers_note = Some(format!("unavailable ({reason})"));
                        }
                    }
                }
            }
            if callees_depth > 0 {
                // T163 (extra item): re-observe and re-resolve the position immediately before
                // the request, exactly as `ide.graph`'s callees path does for every node it
                // queries. The card's other sections (hover, usages, callers) already ran their
                // own live-session round trips against the job's one shared deadline; reusing
                // their now-stale `observed`/`byte_offset` here was the one concrete difference
                // from the graph path, which always resolves its position just-in-time.
                let (observed, bytes) = self.observe(&binding, file.clone()).await?;
                let source = observed_text(&observed, &bytes)?;
                let byte_offset = name_offset(source, &found)?;
                let live = self.live_session_for(job, &observed).await?;
                // A requested callees section always answers: a failed request states
                // unavailability and an answered empty list reports zero, so the card never
                // omits what was asked for without saying why.
                match live
                    .session
                    .outgoing_calls(&observed, &bytes, byte_offset)
                    .await
                {
                    Ok(calls) => {
                        for call in &calls {
                            card.callees.push(Call {
                                name: self.call_symbol_path(job, &worktree_root, &call.to).await,
                                file: render::display_path(&worktree_root, &call.to.uri),
                                line: call.to.selection_range.start.line + 1,
                            });
                        }
                        if calls.is_empty() {
                            card.callees_note = Some("0".to_owned());
                        }
                    }
                    Err(_) => {
                        card.callees_note =
                            Some("unavailable (call hierarchy request failed)".to_owned());
                    }
                }
            }
        }
        if outline.language.names().is_some() {
            self.card_links(job, observed.worktree(), &file, &bytes, &found, &mut card)
                .await?;
        }
        if want_history {
            // A plain directory has no commits to list; the requested section still answers.
            if observed.worktree().is_plain_directory() {
                card.history_unavailable = true;
            } else {
                card.history = history_lines(&worktree_root, &file, found.range).await;
            }
        }
        let authority = self.finish_symbol_job(job, &binding, &observed).await?;
        if card.usages.len() > render::MAX_USAGE_LINES {
            card.more_detail = Some(job.reference.clone());
        }
        let text = render::symbol_card_text(&card);
        let (reply, page) = ContextPageState::with_tail(
            text,
            render::hidden_usages_text(&card),
            ResultKind::Symbol,
        )
        .next(&job.reference)?;
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
        let (outline, worktree_root, lexical) = self.outline_of(job, &observed, &bytes).await?;
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
        .ok_or_else(|| missing_symbol(job, lexical))?;
        // A server whose call hierarchy answers nothing for constructors and partial answers for
        // everything else would make the walk below render a misleading graph; answer the fact.
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
        // Cross-language links: the use sites of the names this symbol defines (callers) and the
        // names it uses (callee leaves). Neither touches the index when the file states no fact.
        let (mut link_sites, mut link_targets) = (Vec::new(), Vec::new());
        if outline.language.names().is_some() && depth > 0 {
            if directions.contains(&render::GraphDirection::Callers) {
                link_sites = self
                    .defined_use_sites(job, observed.worktree(), &file, &bytes, &found)
                    .await?;
            }
            if directions.contains(&render::GraphDirection::Callees) {
                link_targets = self
                    .used_names(
                        job,
                        observed.worktree(),
                        &file,
                        &bytes,
                        found.range,
                        MAX_LINK_LEAVES,
                    )
                    .await?;
            }
        }
        // A file no server owns (outlined from source) has no call hierarchy at all.
        let without_hierarchy = match self.session_server(observed.path()) {
            None => Some((outline.language.name(), "")),
            Some(server) if !server.call_hierarchy() => {
                Some((server.name(), "; use ide.symbol usages"))
            }
            Some(_) => None,
        };
        if let Some((owner, hint)) = without_hierarchy
            && link_sites.is_empty()
            && link_targets.is_empty()
        {
            let language = Lang::for_path(observed.path()).map_or("", Lang::name);
            let authority = self.finish_symbol_job(job, &binding, &observed).await?;
            let text = format!(
                "graph: callers/callees unavailable for {language} ({owner} has no call hierarchy){hint}\n"
            );
            let (reply, page) =
                ContextPageState::new(text, 0, false, ResultKind::Graph).next(&job.reference)?;
            self.shared.set_context_page(&job.reference, page);
            return Ok((reply, Some(authority), Some(observed)));
        }
        let root = render::GraphNode {
            path: found.path.to_string(),
            file: file.display().to_string(),
            line: found.range.start,
            is_test: found.kind == lang::SymbolKind::Test,
            tag: None,
        };
        let mut graph = render::CallGraph::new(root);
        let mut indexes = std::collections::HashMap::from([(graph.nodes[0].path.clone(), 0usize)]);
        let mut queue = VecDeque::new();
        if without_hierarchy.is_none() {
            for direction in &directions {
                queue.push_back((0usize, file.clone(), found.clone(), 0u8, *direction));
            }
        }
        // Link callers: each use site becomes its enclosing outline symbol (or its file), and a
        // callable one continues through the ordinary call hierarchy.
        let authority = self.authority(&binding).await?;
        let mut outlines: std::collections::HashMap<PathBuf, Option<(Outline, bool)>> =
            std::collections::HashMap::new();
        for (site_file, line, language) in link_sites {
            if !outlines.contains_key(&site_file) {
                let outline = self.scanned_outline(job, &authority, &site_file).await;
                outlines.insert(site_file.clone(), outline);
            }
            let (enclosing, served) = match &outlines[&site_file] {
                Some((outline, served)) => (innermost(outline, line).cloned(), *served),
                None => (None, false),
            };
            let node = match &enclosing {
                Some(symbol) => render::GraphNode {
                    path: symbol.path.to_string(),
                    file: site_file.display().to_string(),
                    line: symbol.range.start,
                    is_test: symbol.kind == lang::SymbolKind::Test,
                    tag: Some(language.name().to_owned()),
                },
                None => render::GraphNode {
                    path: site_file.display().to_string(),
                    file: site_file.display().to_string(),
                    line,
                    is_test: false,
                    tag: Some(language.name().to_owned()),
                },
            };
            if node.is_test && !show_tests {
                *graph
                    .collapsed_tests
                    .entry((0, render::GraphDirection::Callers))
                    .or_default() += 1;
                continue;
            }
            let (related, is_new) = match indexes.get(&node.path).copied() {
                Some(index) => (index, false),
                None => {
                    let Some(index) = graph.add_node(node.clone()) else {
                        break;
                    };
                    indexes.insert(node.path.clone(), index);
                    (index, true)
                }
            };
            if !graph.add_edge(render::GraphEdge {
                from: 0,
                to: related,
                direction: render::GraphDirection::Callers,
                link: true,
            }) {
                break;
            }
            if let Some(symbol) = enclosing
                && is_new
                && served
                && depth > 1
                && matches!(
                    symbol.kind,
                    lang::SymbolKind::Function
                        | lang::SymbolKind::Method
                        | lang::SymbolKind::Constructor
                )
                && self
                    .session_server(&site_file)
                    .is_some_and(|server| server.call_hierarchy())
            {
                queue.push_back((
                    related,
                    site_file,
                    symbol,
                    1,
                    render::GraphDirection::Callers,
                ));
            }
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
                    link: false,
                }) {
                    break;
                }
                if is_new && level + 1 < depth {
                    queue.push_back((related, item_file, item_symbol, level + 1, edge_direction));
                }
            }
        }
        // Callee leaves: the names the root uses, never expanded.
        for target in link_targets {
            let node = render::GraphNode {
                path: target.display,
                file: target
                    .definition
                    .as_ref()
                    .map(|(file, _)| file.display().to_string())
                    .unwrap_or_default(),
                line: target.definition.as_ref().map_or(0, |(_, line)| *line),
                is_test: false,
                tag: Some(match target.definition {
                    Some(_) => target.label.to_owned(),
                    None => format!("{}, no indexed {}", target.label, target.define_word),
                }),
            };
            let related = match indexes.get(&node.path).copied() {
                Some(index) => index,
                None => {
                    let Some(index) = graph.add_node(node.clone()) else {
                        break;
                    };
                    indexes.insert(node.path.clone(), index);
                    index
                }
            };
            if !graph.add_edge(render::GraphEdge {
                from: 0,
                to: related,
                direction: render::GraphDirection::Callees,
                link: true,
            }) {
                break;
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
        let (outline, _, _) = self.outline_of(job, &observed, &bytes).await?;
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
                tag: None,
            },
            relative,
            found,
        ))
    }

    /// Document symbols of one observed file through the live session, normalized by the
    /// language module, or the language's source outline when no server owns the file. While a
    /// registered server is still loading (a cold workspace load), and when it is unavailable
    /// (its workspace failed to load, or no launch configures it), a language that opted in
    /// ([`LanguageSupport::outline_while_loading`](crate::lang::LanguageSupport::outline_while_loading))
    /// answers from its source outline at once instead of parking; a file it cannot outline,
    /// and every other language, keeps the loading answer and its parking retry, or the
    /// unavailable refusal. Returns the outline, the worktree root for path rendering, and why
    /// the outline came from the text alone (replies then say so in one compact line; an
    /// address it does not contain waits for the server while it loads — see
    /// [`missing_symbol`]).
    async fn outline_of(
        &mut self,
        job: &mut Job,
        observed: &SourceObservation,
        bytes: &[u8],
    ) -> Result<(Outline, std::path::PathBuf, Option<Lexical>), FailureCode> {
        let language = Lang::for_path(observed.path()).ok_or(FailureCode::ProviderUnavailable)?;
        let support = language.support();
        let source = observed_text(observed, bytes)?.to_owned();
        let worktree_root = observed.worktree().worktree_path().to_path_buf();
        let Some(server) = self.session_server(observed.path()) else {
            // No registered server owns the file: a language that outlines from its text still
            // answers; any other keeps the provider-unavailable refusal.
            return support
                .outline_from_source(observed.path(), &source)
                .map(|outline| (outline, worktree_root, None))
                .ok_or(FailureCode::ProviderUnavailable);
        };
        let live = match self.live_session_for(job, observed).await {
            Ok(live) => live,
            Err(FailureCode::ProviderLoading) if support.outline_while_loading() => {
                // The registered server is loading: the source outline answers now (the park
                // `live_session_for` set is lifted) and the caller's reply marks it lexical; a
                // file that does not scan cleanly keeps the park, exactly as before.
                let parked = job.park_until.take();
                return match support.outline_from_source(observed.path(), &source) {
                    Some(outline) => Ok((outline, worktree_root, Some(Lexical::Loading))),
                    None => {
                        job.park_until = parked;
                        Err(FailureCode::ProviderLoading)
                    }
                };
            }
            Err(FailureCode::ProviderUnavailable) if support.outline_while_loading() => {
                // The registered server failed (its workspace would not load) or no launch
                // configures it: the exact source outline answers at once, marked lexical, and
                // nothing is parked — a later call asks the server again, so one that recovers
                // wins. A file that does not scan cleanly keeps the refusal, exactly as before,
                // with the stage `live_session_for` already named. The lexical answer succeeds,
                // so its failure detail must not leak into a later refusal of this job.
                return match support.outline_from_source(observed.path(), &source) {
                    Some(outline) => {
                        job.failure_detail = None;
                        Ok((outline, worktree_root, Some(Lexical::Unavailable)))
                    }
                    None => Err(FailureCode::ProviderUnavailable),
                };
            }
            Err(other) => return Err(other),
        };
        let symbols = match live.session.document_symbols(observed, bytes).await {
            Ok(symbols) => symbols,
            Err(error) => {
                // The session passed readiness but this exchange failed (a server error reply
                // while it reloads its workspace, or a dying transport): a language that
                // outlines from its text answers at once, marked lexical with the cause in its
                // footer, exactly as the unavailable branch. A file its lexer refuses keeps the
                // refusal, now with the stage naming the failed request.
                if support.outline_while_loading()
                    && let Some(outline) = support.outline_from_source(observed.path(), &source)
                {
                    let cause = exchange_cause(&error);
                    return Ok((outline, worktree_root, Some(Lexical::Exchange { cause })));
                }
                job.set_stage_failure(
                    &FailureCode::ProviderUnavailable,
                    &format!("{}: documentSymbols request failed", server.name()),
                );
                return Err(FailureCode::ProviderUnavailable);
            }
        };
        Ok((
            support.normalize(observed.path(), &source, symbols),
            worktree_root,
            None,
        ))
    }

    /// One compact line marking a reply that was built from the lexical outline because the
    /// file's registered server did not answer it (`why`: still loading, unavailable, or its
    /// documentSymbols exchange failed): the outline is exact, so the call needs no repeat, but
    /// semantic facts (usages, callers) are not included. `None` when no server owns the file
    /// (nothing is loading, failed or refused).
    fn lexical_note(&self, path: &Path, why: &Lexical) -> Option<String> {
        let server = self.session_server(path)?;
        let state = match why {
            Lexical::Loading => "still indexing".to_owned(),
            Lexical::Unavailable => "unavailable".to_owned(),
            Lexical::Exchange { cause } => format!("request failed: {cause}"),
        };
        Some(format!(
            "outline: from source, exact ({} {state}; no need to repeat)",
            server.name()
        ))
    }

    /// Resolves a bare symbol name to its definition file.
    ///
    /// Every language present in the worktree opens its own session — any source file of that
    /// language opens it — and a mixed worktree searches every language session before deciding
    /// uniqueness, so a name two languages share is ambiguous. The search asks the provider's workspace symbols first; a provider whose
    /// workspace search stays empty or cannot answer (some servers never list unopened project
    /// files) falls back to scanning the bounded file list of that language's outlines. Only
    /// when no session produced any answer at all is the name reported provider-unavailable.
    pub(super) async fn locate_by_name(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        name: &str,
    ) -> Result<Located, FailureCode> {
        let binding = binding.clone();
        let authority = self.authority(&binding).await?;
        let root = authority.worktree().worktree_path().to_path_buf();
        let files = collect_language_files(&root);
        if files.is_empty() {
            job.failure_detail = Some("symbol:anchor_missing".to_owned());
            return Err(FailureCode::ProviderUnavailable);
        }
        // (relative file, candidate line) in language and provider order.
        let mut matches: Vec<(std::path::PathBuf, String)> = Vec::new();
        // True once at least one language produced a definite empty-or-nonempty answer.
        let mut answered = false;
        for language_files in files.values() {
            let Some(anchor) = language_files.first() else {
                continue;
            };
            let Ok(anchor_read) = read_authorized_source(
                authority.worktree(),
                anchor,
                SourceReadLimits::new(1024, MAX_SOURCE_BYTES).map_err(|_| FailureCode::Internal)?,
            ) else {
                continue;
            };
            let anchor_source = scan_observation(
                authority.worktree(),
                authority.epoch(),
                self.source_sequence,
                anchor,
                anchor_read.contents(),
            )?;
            let live = match self.live_session_for(job, &anchor_source).await {
                Ok(live) => live,
                Err(_) => continue,
            };
            // Workspace symbol search is exact-name and answers in one round trip.
            let workspace_hits: Vec<(std::path::PathBuf, Option<String>)> =
                match live.session.workspace_symbols(name).await {
                    Ok(found) => {
                        answered = true;
                        found
                            .into_iter()
                            .filter(|symbol| symbol.name == name)
                            .filter_map(|symbol| {
                                symbol
                                    .location
                                    .uri
                                    .to_file_path()
                                    .ok()
                                    .and_then(|path| {
                                        path.strip_prefix(&root).ok().map(Path::to_path_buf)
                                    })
                                    .map(|path| (path, symbol.container_name))
                            })
                            .collect()
                    }
                    Err(_) => Vec::new(),
                };
            if !workspace_hits.is_empty() {
                matches.extend(workspace_hits.into_iter().map(|(path, container)| {
                    let candidate = match container {
                        Some(container) => format!("{}#{container}/{name}", path.display()),
                        None => format!("{}#{name}", path.display()),
                    };
                    (path, candidate)
                }));
                continue;
            }
            // Scanned paths are read without registering them as editable source.
            for file in language_files {
                let Ok(read) = read_authorized_source(
                    authority.worktree(),
                    file,
                    SourceReadLimits::new(1024, MAX_SOURCE_BYTES)
                        .map_err(|_| FailureCode::Internal)?,
                ) else {
                    continue;
                };
                let bytes = read.contents();
                if !contains_bare_name(bytes, name.as_bytes()) {
                    continue;
                }
                let Ok(observed) = scan_observation(
                    authority.worktree(),
                    authority.epoch(),
                    self.source_sequence,
                    file,
                    bytes,
                ) else {
                    continue;
                };
                let language = Lang::for_path(file).ok_or(FailureCode::ProviderUnavailable)?;
                let support = language.support();
                let Ok(live) = self.live_session_for(job, &observed).await else {
                    continue;
                };
                if let Ok(symbols) = live.session.document_symbols(&observed, bytes).await {
                    answered = true;
                    let source = String::from_utf8_lossy(bytes);
                    let outline = support.normalize(file, &source, symbols);
                    for candidate in outline.named(name) {
                        let candidate = format!("{}#{}", file.display(), candidate.path);
                        matches.push((file.clone(), candidate));
                    }
                }
            }
        }
        // Every language was searched: a name several languages share is ambiguous.
        matches.dedup_by(|a, b| a.1 == b.1);
        if !answered {
            job.failure_detail = Some("symbol:workspace_symbols".to_owned());
            return Err(FailureCode::ProviderUnavailable);
        }
        match matches.len() {
            0 => {
                job.failure_detail = Some("symbol:outline_scan".to_owned());
                Err(FailureCode::UnknownSymbol)
            }
            1 => Ok(Located::One(matches.remove(0).0)),
            _ => Ok(Located::Many(
                matches
                    .into_iter()
                    .map(|(_, candidate)| candidate)
                    .collect(),
            )),
        }
    }

    /// The outline of `file` read without registering it against the binding (a graph node's
    /// file) and whether the server answered it (`false` for a text outline, including the
    /// lexical one answered while the server loads, so no call-hierarchy walk starts from it),
    /// falling back to the language's text outline when the server cannot answer, or `None`
    /// when it cannot be read or outlined.
    async fn scanned_outline(
        &mut self,
        job: &mut Job,
        authority: &AuthorityStamp,
        file: &Path,
    ) -> Option<(Outline, bool)> {
        let limits = SourceReadLimits::new(1024, MAX_SOURCE_BYTES).ok()?;
        let read = read_authorized_source(authority.worktree(), file, limits).ok()?;
        let observed = scan_observation(
            authority.worktree(),
            authority.epoch(),
            self.source_sequence,
            file,
            read.contents(),
        )
        .ok()?;
        match self.outline_of(job, &observed, read.contents()).await {
            // A lexical outline (the server still loading, or unavailable) is not the server's.
            Ok((outline, _, lexical)) => Some((outline, lexical.is_none())),
            // The server could not outline it (its project config lives below the worktree
            // root): the language's text outline still names the enclosing declaration.
            Err(_) => Lang::for_path(file)?
                .support()
                .outline_from_source(file, observed_text(&observed, read.contents()).ok()?)
                .map(|outline| (outline, false)),
        }
    }

    /// Answers an ambiguous name with its candidates instead of guessing.
    pub(super) async fn ambiguous(
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
        for candidate in candidates.iter().take(MAX_CANDIDATE_LINES) {
            text.push_str(&format!("  {candidate}\n"));
        }
        let hidden = candidates.len().saturating_sub(MAX_CANDIDATE_LINES);
        let tail = (hidden > 0).then(|| {
            text.push_str(&format!(
                "  … {hidden} more (ide.inspect {})\n",
                job.reference
            ));
            let total = candidates.len();
            let mut tail = format!(
                "candidates {}–{total} of {total}:\n",
                MAX_CANDIDATE_LINES + 1
            );
            for candidate in &candidates[MAX_CANDIDATE_LINES..] {
                tail.push_str(&format!("  {candidate}\n"));
            }
            tail
        });
        let (reply, page) =
            ContextPageState::with_tail(text, tail, ResultKind::Symbol).next(&job.reference)?;
        self.shared.set_context_page(&job.reference, page);
        Ok((reply, Some(authority), None))
    }

    /// Usage lines for reference locations: relative path, line, trimmed text, test flag.
    ///
    /// A usage is a test when its file is one by the language's test-file conventions or when the
    /// file's outline places it inside a test symbol (an inline `#[cfg(test)] mod tests`), the
    /// same classification callers and graphs use; one outline per file answers every row.
    async fn usage_lines(
        &mut self,
        job: &mut Job,
        worktree_root: &Path,
        definition: &SymbolPath,
        definition_line: u32,
        references: Vec<async_lsp::lsp_types::Location>,
    ) -> Vec<Usage> {
        let mut cache: std::collections::BTreeMap<std::path::PathBuf, String> =
            std::collections::BTreeMap::new();
        let mut outlines: std::collections::HashMap<std::path::PathBuf, Option<Outline>> =
            std::collections::HashMap::new();
        let binding = job.invocation.binding_ref().clone();
        let authority = self.authority(&binding).await.ok();
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
            let mut is_test = Lang::for_path(&relative)
                .is_some_and(|language| language.support().is_test_file(&relative));
            if !is_test
                && authority.is_some()
                && !outlines.contains_key(&relative)
                && let Some(authority) = authority.as_ref()
            {
                let outline = self
                    .scanned_outline(job, authority, &relative)
                    .await
                    .map(|(outline, _)| outline);
                outlines.insert(relative.clone(), outline);
            }
            if !is_test {
                is_test = outlines.get(&relative).is_some_and(|outline| {
                    outline
                        .as_ref()
                        .is_some_and(|outline| inside_test(outline, line))
                });
            }
            usages.push(Usage {
                file: relative.display().to_string(),
                line,
                text,
                is_test,
                tag: None,
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
        let Ok((outline, _, _)) = self.outline_of(job, &observed, &bytes).await else {
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

/// Builds an ephemeral provider input that carries no persisted or registered source authority.
///
/// `sequence` is the worker's current source sequence: a live session refuses observations older
/// than the last one it synchronized, so a fixed sequence would fail once the session has served
/// any registered observation.
fn scan_observation(
    worktree: &WorktreeRef,
    authority_epoch: u64,
    sequence: u64,
    path: &Path,
    bytes: &[u8],
) -> Result<SourceObservation, FailureCode> {
    SourceObservation::new(
        worktree.clone(),
        authority_epoch,
        sequence.max(1),
        ObservationRef::new("symbol-scan").map_err(|_| FailureCode::Internal)?,
        path.to_path_buf(),
        Some(SourceBytes::from_bytes(bytes)),
        SourceRevision::new(blake3::hash(bytes).to_hex().to_string())
            .map_err(|_| FailureCode::Internal)?,
        SourceCoverage::Complete,
        crate::workspace::observation::ObservedState::Present,
    )
    .map_err(|_| FailureCode::Internal)
}

/// Returns whether `bytes` contains `name` as a complete ASCII identifier word.
fn contains_bare_name(bytes: &[u8], name: &[u8]) -> bool {
    bytes
        .windows(name.len())
        .enumerate()
        .any(|(index, candidate)| {
            candidate == name
                && (index == 0 || !is_identifier_byte(bytes[index - 1]))
                && (index + name.len() == bytes.len()
                    || !is_identifier_byte(bytes[index + name.len()]))
        })
}

/// Treats ASCII letters, digits, and underscore as identifier bytes for scan pre-filtering.
fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
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

/// Name leaves one graph node lists.
const MAX_LINK_LEAVES: usize = 10;

/// Files a rename reply names inline before counting the rest as `+N more`.
const RENAME_SUMMARY_FILES: usize = 5;

/// The innermost symbol of `outline` holding `line`.
pub(super) fn innermost(outline: &Outline, line: u32) -> Option<&lang::Symbol> {
    let mut found: Option<&lang::Symbol> = None;
    for symbol in &outline.symbols {
        symbol.walk(&mut |candidate| {
            if candidate.range.start <= line
                && line <= candidate.range.end
                && found.is_none_or(|old| candidate.range.len() <= old.range.len())
            {
                found = Some(candidate);
            }
        });
    }
    found
}

/// Whether the outline places 1-based `line` inside a test symbol at any nesting — a test module
/// or a test itself — the same classification callers and graphs use, so a reference from inside
/// an inline test module counts as a test wherever the file itself is not a test file.
pub(super) fn inside_test(outline: &Outline, line: u32) -> bool {
    let mut level: &[lang::Symbol] = &outline.symbols;
    loop {
        let Some(found) = level
            .iter()
            .filter(|symbol| symbol.range.start <= line && line <= symbol.range.end)
            .min_by_key(|symbol| symbol.range.len())
        else {
            return false;
        };
        if found.kind == lang::SymbolKind::Test {
            return true;
        }
        level = &found.children;
    }
}

/// Candidates an ambiguity reply prints before the rest moves behind its `detail_ref`.
const MAX_CANDIDATE_LINES: usize = 20;

/// Where a bare name resolved to.
pub(super) enum Located {
    One(std::path::PathBuf),
    Many(Vec<String>),
}

/// Directories the session-anchor walk never enters: VCS internals, virtual environments,
/// dependency installs and build output. None of them open a language session.
const ANCHOR_SKIPPED_DIRECTORIES: [&str; 7] = crate::intelligence::names::SKIPPED_DIRECTORIES;
/// Maximum directories the bounded session-anchor walk visits.
const ANCHOR_MAX_DIRECTORIES: usize = 64;
/// Source files kept per language for the bare-name outline scan that covers providers whose
/// workspace symbol search stays empty (some servers never list unopened project files).
const ANCHOR_SCAN_FILES_PER_LANGUAGE: usize = 64;

/// First source files of each language in the worktree, from one bounded breadth-first walk
/// over the top level plus two nested levels of ordinary directories, skipping ignored trees.
/// Deterministic order; every language keeps up to [`ANCHOR_SCAN_FILES_PER_LANGUAGE`] files.
fn collect_language_files(
    root: &Path,
) -> std::collections::BTreeMap<Lang, Vec<std::path::PathBuf>> {
    let mut files: std::collections::BTreeMap<Lang, Vec<std::path::PathBuf>> =
        std::collections::BTreeMap::new();
    let mut queue = VecDeque::from([(root.to_path_buf(), 0u8)]);
    let mut visited = 0usize;
    while let Some((directory, depth)) = queue.pop_front() {
        visited += 1;
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if depth < 2
                    && !name.starts_with('.')
                    && !ANCHOR_SKIPPED_DIRECTORIES.contains(&name.as_ref())
                {
                    queue.push_back((path, depth + 1));
                }
            } else if let Some(language) = Lang::for_path(&path) {
                let list = files.entry(language).or_default();
                if list.len() < ANCHOR_SCAN_FILES_PER_LANGUAGE {
                    list.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
                }
            }
        }
        if visited >= ANCHOR_MAX_DIRECTORIES {
            break;
        }
    }
    files
}

/// Why an outline came from the file's text alone: its registered server is still loading, it
/// is unavailable (its workspace failed to load, or no launch configures it), or its ready
/// session failed the documentSymbols exchange itself. `None` marks the server's own answer —
/// and a file no server owns, which the replies do not mark.
#[derive(Clone)]
pub(super) enum Lexical {
    /// The registered server has not finished loading its workspace yet.
    Loading,
    /// The registered server failed or is absent; it will not answer this call.
    Unavailable,
    /// The registered session passed readiness but its documentSymbols exchange failed; the
    /// bounded cause names why in the outline footer.
    Exchange {
        /// First line of the failed exchange's error, cut to [`EXCHANGE_CAUSE_LIMIT`] bytes.
        cause: String,
    },
}

/// Longest cause of a failed documentSymbols exchange an outline footer quotes; the footer is
/// one compact line, so a longer error is cut at this byte ceiling.
const EXCHANGE_CAUSE_LIMIT: usize = 120;

/// The bounded first line of a failed documentSymbols exchange, for the outline footer.
fn exchange_cause(error: &std::io::Error) -> String {
    crate::intelligence::context::prefix(
        error.to_string().lines().next().unwrap_or_default(),
        EXCHANGE_CAUSE_LIMIT,
    )
    .to_owned()
}

/// The failure for an address the job's outline does not contain (`lexical`: why that outline
/// came from the text). A server outline proves the symbol absent: `unknown_symbol`. A lexical
/// outline cannot — an item it names differently or does not report may still exist — so while
/// the server loads the call waits for it exactly as any symbol tool waits: parked for a retry,
/// or `provider_loading` at once for an edit (see
/// [`park_while_loading`](super::providers::park_while_loading)). An unavailable server will
/// not answer either — and a session whose exchange failed just did not — so the miss refuses
/// `provider_unavailable` at once instead of parking.
pub(super) fn missing_symbol(job: &mut Job, lexical: Option<Lexical>) -> FailureCode {
    match lexical {
        None => FailureCode::UnknownSymbol,
        Some(Lexical::Unavailable | Lexical::Exchange { .. }) => FailureCode::ProviderUnavailable,
        Some(Lexical::Loading) => {
            super::providers::park_while_loading(job);
            FailureCode::ProviderLoading
        }
    }
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

/// Maps a registered path's observed state to the closed `no_such_file` failure with its bounded
/// requested path, `None` while the path is present. A missing draft is a legitimate observation,
/// never a `source_unavailable` collapse (T163).
fn no_such_file(
    state: crate::workspace::observation::ObservedState,
    path: &str,
) -> Option<FailureCode> {
    (state == crate::workspace::observation::ObservedState::Missing)
        .then(|| FailureCode::NoSuchFile(bounded_utf8_prefix(path, MAX_NO_SUCH_FILE_PATH_BYTES)))
}

#[cfg(test)]
mod no_such_file_tests {
    use super::*;
    use crate::workspace::observation::ObservedState;

    #[test]
    fn missing_state_names_the_bounded_requested_path() {
        assert_eq!(
            no_such_file(ObservedState::Missing, "src/assistance/host_bindng.rs"),
            Some(FailureCode::NoSuchFile(
                "src/assistance/host_bindng.rs".to_owned()
            ))
        );
    }

    #[test]
    fn present_state_names_no_failure() {
        assert_eq!(no_such_file(ObservedState::Present, "src/lib.rs"), None);
    }

    #[test]
    fn an_oversize_path_is_bounded_to_the_reason_limit() {
        let long = "a".repeat(MAX_NO_SUCH_FILE_PATH_BYTES + 50);
        let Some(FailureCode::NoSuchFile(bounded)) = no_such_file(ObservedState::Missing, &long)
        else {
            panic!("expected NoSuchFile");
        };
        assert_eq!(bounded.len(), MAX_NO_SUCH_FILE_PATH_BYTES);
    }
}

// ---------------------------------------------------------------------------------------------
// ide.edit by symbol (v0.4): replace / insert / delete, plus a line-range replace.
// ---------------------------------------------------------------------------------------------

impl Worker<'_> {
    /// `ide.edit` with `symbol` (+ `op`) or `path` + `lines`: splices the file in memory, formats
    /// the candidate when the project has a formatter, then writes it through the ordinary
    /// stale-safe edit path with the observation the splice was resolved on as the base.
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
        // Resolve the file and the line span the operation touches. The symbol form carries the
        // observation it resolved on out of the branch: its line span is only meaningful for those
        // exact bytes.
        let (file, splice, lexical, resolved) = match job
            .parameters
            .get("symbol")
            .and_then(Value::as_str)
        {
            Some(symbol) => {
                let symbol = SymbolPath::parse(symbol).map_err(|_| FailureCode::UnknownSymbol)?;
                let file = symbol
                    .file()
                    .ok_or(FailureCode::UnknownSymbol)?
                    .to_path_buf();
                let (observed, bytes) = self.observe(&binding, file.clone()).await?;
                let source = observed_text(&observed, &bytes)?.to_owned();
                let (outline, _, from_text) = self.outline_of(job, &observed, &bytes).await?;
                let lexical = from_text
                    .as_ref()
                    .and_then(|why| self.lexical_note(observed.path(), why));
                let splice = match op.as_str() {
                    "insert" => {
                        let where_ = match job.parameters.get("where").and_then(Value::as_str) {
                            Some("before") => lang::InsertWhere::Before,
                            Some("after") => lang::InsertWhere::After,
                            Some("first") => lang::InsertWhere::First,
                            Some("last") => lang::InsertWhere::Last,
                            _ => return Err(FailureCode::Internal),
                        };
                        let support = outline.language.support();
                        let site = support
                            .insert_site(&source, &outline, &symbol, where_)
                            .map_err(|error| match error {
                                lang::LangError::UnknownSymbol(_) => missing_symbol(job, from_text),
                                _ => FailureCode::Internal,
                            })?;
                        Splice::Insert(site)
                    }
                    _ => {
                        let found = outline
                            .find(&symbol)
                            .ok_or_else(|| missing_symbol(job, from_text))?;
                        Splice::Replace(found.range)
                    }
                };
                (file, splice, lexical, Some((observed, bytes)))
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
                (
                    std::path::PathBuf::from(path),
                    Splice::Replace(range),
                    None,
                    None,
                )
            }
        };
        // The line-range form observes right before splicing so the base is the exact text being
        // replaced; the symbol form splices on — and bases the write on — the observation it
        // resolved the symbol on, so a file that changed since that read is refused stale_source
        // at the write instead of splicing version-one line numbers into different bytes.
        let (observed, bytes) = match resolved {
            Some(resolved) => resolved,
            None => self.observe(&binding, file.clone()).await?,
        };
        let source = observed_text(&observed, &bytes)?.to_owned();
        let path = file.display().to_string();
        // An explicit `source_ref` (mandatory for the line-range form) must name a retained
        // same-path observation whose bytes are still the file's current ones: the splice's line
        // numbers and the symbol's range are only meaningful for the content the caller read.
        // Anything else is refused before any write, exactly as a changed full-file base is.
        let base = match job.parameters.get("source_ref").and_then(Value::as_str) {
            Some(reference) => {
                let retained = self.shared.ledger.lock().ok().and_then(|ledger| {
                    ledger
                        .details
                        .get(reference)
                        .and_then(|detail| admitted_edit_source(detail, &binding, reference, &path))
                });
                match retained {
                    Some(retained) if retained.bytes() == observed.bytes() => retained,
                    _ => {
                        let authority = self.authority(&binding).await.ok();
                        return Ok((
                            PeerReply::Edit {
                                result: EditResult {
                                    operation_id,
                                    path,
                                    outcome: ChangesEditOutcome::StaleSource,
                                    source_ref: None,
                                },
                                diagnostics: EditDiagnostics::Unknown {},
                                note: None,
                                operation: None,
                            },
                            authority,
                            None,
                        ));
                    }
                }
            }
            None => observed.clone(),
        };
        let total = lang::line_count(&source);
        let candidate = match (&op[..], &splice) {
            ("delete", Splice::Replace(range)) => {
                if range.start > total {
                    job.failure_detail = Some(format!(
                        "edit:range_past_end: line {} is past the end of {path} ({total} lines)",
                        range.start
                    ));
                    return Err(FailureCode::UnknownSymbol);
                }
                delete_symbol_lines(&source, *range)
            }
            ("insert", Splice::Insert(site)) => {
                let content = content.ok_or(FailureCode::Internal)?;
                insert_lines(&source, site, &content)
            }
            (_, Splice::Replace(range)) => {
                let content = content.ok_or(FailureCode::Internal)?;
                if range.start > total {
                    job.failure_detail = Some(format!(
                        "edit:range_past_end: line {} is past the end of {path} ({total} lines)",
                        range.start
                    ));
                    return Err(FailureCode::UnknownSymbol);
                }
                splice_lines(&source, *range, &content)
            }
            _ => return Err(FailureCode::Internal),
        };
        let spliced_lines = lang::line_count(&candidate);
        let formatted = self.format_candidate(&observed, &file, candidate).await;
        job.format_note = formatted_note(
            spliced_lines,
            &formatted,
            &job.reference,
            splice_end(&splice),
        );
        if let Some(note) = lexical {
            // The symbol was resolved from the lexical outline while the server loads; the
            // reply says so next to the formatter's line-movement note.
            job.format_note = Some(match job.format_note.take() {
                Some(existing) => format!("{note}\n{existing}"),
                None => note,
            });
        }
        let candidate = formatted;
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
                        note: None,

                        operation: None,
                    },
                    authority,
                    None,
                ));
            }
            Err(_) => return Err(FailureCode::Internal),
        };
        self.edit_with_source(job, request, prepared, base, true)
            .await
    }

    /// `ide.edit {path, content}` without `source_ref`: creates a file that does not exist yet.
    ///
    /// The worker observes `path` itself (the same confined reader as every other observation);
    /// only an observed absence is a valid base, since `ide.read` and `ide.outline` answer
    /// `no_such_file` there and so cannot mint a `source_ref`. The candidate is formatted with the
    /// project formatter and written through the ordinary stale-safe edit path with that missing
    /// observation as the base, so the reply is `edit: created` with the project check, and a file
    /// that appears between the observation and the write is refused `stale_source`, never
    /// overwritten. An existing path is refused as invalid parameters naming `source_ref`, before
    /// any receipt or write, so a retry with a real `source_ref` may reuse the `operation_id`.
    pub(super) async fn create_file(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let text = |field: &str| {
            job.parameters
                .get(field)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or(FailureCode::Internal)
        };
        let (operation_id, path, content) =
            (text("operation_id")?, text("path")?, text("content")?);
        let file = std::path::PathBuf::from(&path);
        let (observed, _) = self.observe(&binding, file.clone()).await?;
        if observed.bytes().is_some() {
            let authority = self.authority(&binding).await.ok();
            return Ok((
                PeerReply::InvalidParameters {
                    message: crate::assistance::facade::ParameterError::InvalidField {
                        field: "source_ref",
                        rule: crate::assistance::facade::FieldRule::ReplaceSourceRef,
                    }
                    .message(AssistanceTool::Edit),
                },
                authority,
                None,
            ));
        }
        let candidate = self.format_candidate(&observed, &file, content).await;
        let request = EditRequest::new(operation_id, path, &job.reference, candidate)
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
                        note: None,
                        operation: None,
                    },
                    authority,
                    None,
                ));
            }
            Err(_) => return Err(FailureCode::Internal),
        };
        self.edit_with_source(job, request, prepared, observed, true)
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
        let support = language.support();
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
        // Rename is server-only: while the server loads, park and retry before the address is
        // resolved, so a lexical outline never answers for it (not even `unknown_symbol`).
        // Parked by hand: `live_session_for` never parks an edit.
        match self.live_session_for(job, &observed).await {
            Ok(_) => {}
            Err(FailureCode::ProviderLoading) => {
                job.park_until = Some(tokio::time::Instant::now() + Duration::from_millis(300));
                return Err(FailureCode::ProviderLoading);
            }
            Err(other) => return Err(other),
        }
        let (outline, worktree_root, lexical) = self.outline_of(job, &observed, &bytes).await?;
        let found = outline
            .find(&symbol)
            .cloned()
            .ok_or_else(|| missing_symbol(job, lexical))?;
        let source = observed_text(&observed, &bytes)?;
        let byte_offset = name_offset(source, &found)?;
        let (edit, encoding) = {
            // The server may have gone back to loading since the check above.
            let live = match self.live_session_for(job, &observed).await {
                Ok(live) => live,
                Err(FailureCode::ProviderLoading) => {
                    job.park_until = Some(tokio::time::Instant::now() + Duration::from_millis(300));
                    return Err(FailureCode::ProviderLoading);
                }
                Err(other) => return Err(other),
            };
            let encoding = live.session.capabilities().position_encoding.clone();
            let edit = live
                .session
                .rename(&observed, &bytes, byte_offset, &new_name)
                .await
                .map_err(|_| {
                    job.failure_detail = Some(format!("edit:rename_request_failed:{}", found.name));
                    FailureCode::ProviderUnavailable
                })?
                .ok_or_else(|| {
                    job.failure_detail = Some(format!("edit:rename_no_edits:{}", found.name));
                    FailureCode::ProviderUnavailable
                })?;
            (edit, encoding)
        };
        let grouped = lang::edits::group_workspace_edit(edit);
        if !grouped.unsupported.is_empty() {
            job.failure_detail = Some(format!("edit:rename_unsupported_edits:{}", found.name));
            return Err(FailureCode::ProviderUnavailable);
        }
        let mut summary: Vec<(String, usize)> = Vec::new();
        let mut last: Option<(PeerReply, Option<SourceObservation>)> = None;
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
            // Never await the project check per file: a parked rename would resume as one
            // file's plain edit reply with the remaining files unwritten.
            let (outcome, _, source) = self
                .edit_with_source(job, request, prepared, observed, false)
                .await?;
            if let PeerReply::Edit { .. } = &outcome {
                summary.push((relative.display().to_string(), file_edits.edits.len()));
            }
            last = Some((outcome, source));
        }
        // One rename answers once, as an edit reply the Edit tool accepts: the operation word
        // names the rename, the note lists every touched file with its site count, bounded like
        // every other list, and the diagnostics are the last written file's.
        let Some((
            PeerReply::Edit {
                result,
                diagnostics,
                ..
            },
            source,
        )) = last
        else {
            job.failure_detail = Some("edit:rename_no_edits".to_owned());
            return Err(FailureCode::ProviderUnavailable);
        };
        let sites: usize = summary.iter().map(|(_, count)| count).sum();
        let mut note = format!(
            "renamed {} → {new_name}; {sites} sites in {} files",
            found.name,
            summary.len()
        );
        if !summary.is_empty() {
            note.push_str(": ");
            note.push_str(
                &summary
                    .iter()
                    .take(RENAME_SUMMARY_FILES)
                    .map(|(file, count)| format!("{file} ({count})"))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            let hidden = summary.len().saturating_sub(RENAME_SUMMARY_FILES);
            if hidden > 0 {
                note.push_str(&format!("; +{hidden} more"));
            }
        }
        let authority = self.authority(&binding).await?;
        Ok((
            PeerReply::Edit {
                result,
                diagnostics,
                note: Some(note),
                operation: Some("renamed".to_owned()),
            },
            Some(authority),
            source,
        ))
    }
}

/// What a symbol edit replaces or where it inserts.
enum Splice {
    Replace(LineRange),
    Insert(lang::InsertSite),
}

/// Last pre-format line the operation touched: everything after it shifts when the formatter
/// moves lines.
fn splice_end(splice: &Splice) -> u32 {
    match splice {
        Splice::Replace(range) => range.end,
        Splice::Insert(site) => site.line,
    }
}

/// States the formatter's line movement when it changed the file's line count, so a later
/// line-addressed edit re-reads instead of reusing the pre-format line numbers.
fn formatted_note(
    spliced_lines: u32,
    formatted: &str,
    reference: &str,
    anchor: u32,
) -> Option<String> {
    let shift = i64::from(lang::line_count(formatted)) - i64::from(spliced_lines);
    (shift != 0).then(|| {
        format!(
            "formatted: {shift:+} lines after line {anchor}; use source_ref {reference} for the next edit"
        )
    })
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

/// Deletes a symbol's lines and merges the blank runs on either side into the narrower one, so
/// the surviving neighbours keep the file's own spacing (one or two blank lines, none next to
/// an opening or closing bracket line) and a file boundary keeps none. This makes an
/// insert followed by a delete of the same symbol restore the original bytes.
fn delete_symbol_lines(source: &str, range: LineRange) -> String {
    let mut lines: Vec<&str> = source.split_inclusive('\n').collect();
    let start = (range.start.saturating_sub(1) as usize).min(lines.len());
    let end = (range.end as usize).min(lines.len());
    lines.drain(start..end);
    let blank = |line: &&&str| line.trim().is_empty();
    let left = lines[..start].iter().rev().take_while(blank).count();
    let right = lines[start..].iter().take_while(blank).count();
    let keep = if left == start || start + right == lines.len() {
        0
    } else {
        left.min(right)
    };
    lines.drain(start - left + keep..start + right);
    lines.concat()
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

/// PATH for formatters: the registered languages' home tool directories (see
/// [`LanguageDescriptor::home_tool_dirs`](crate::lang::LanguageDescriptor::home_tool_dirs)), the
/// daemon's own configured PATH, then the system directories — never the agent's shell
/// environment.
fn formatter_path() -> String {
    let mut parts = vec![];
    if let Ok(home) = std::env::var("HOME") {
        for language in crate::lang::registered() {
            for dir in language.descriptor().home_tool_dirs {
                parts.push(format!("{home}/{dir}"));
            }
        }
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

    #[test]
    fn inserting_then_deleting_restores_middle_and_end_layouts() {
        for (source, site, range) in [
            (
                "a\n\nb\n",
                lang::InsertSite {
                    line: 3,
                    indent: String::new(),
                    blank_before: 1,
                    blank_after: 1,
                },
                LineRange::new(4, 4),
            ),
            (
                "a\n\nb\n",
                lang::InsertSite {
                    line: 2,
                    indent: String::new(),
                    blank_before: 1,
                    blank_after: 1,
                },
                LineRange::new(3, 3),
            ),
            (
                "a\n",
                lang::InsertSite {
                    line: 2,
                    indent: String::new(),
                    blank_before: 1,
                    blank_after: 0,
                },
                LineRange::new(3, 3),
            ),
        ] {
            let inserted = insert_lines(source, &site, "probe");
            assert_eq!(delete_symbol_lines(&inserted, range), source);
        }
    }

    #[test]
    fn deleting_between_neighbours_keeps_the_narrower_separator() {
        assert_eq!(
            delete_symbol_lines("a\n\nprobe\n\n\nb\n", LineRange::new(3, 3)),
            "a\n\nb\n"
        );
        // Two blank lines between top-level definitions stay two.
        assert_eq!(
            delete_symbol_lines("a\n\n\nprobe\n\n\nb\n", LineRange::new(4, 4)),
            "a\n\n\nb\n"
        );
        // The first and last members next to bracket lines leave no blank line behind.
        let body = "impl X {\n    fn a() {}\n\n    fn b() {}\n}\n";
        assert_eq!(
            delete_symbol_lines(body, LineRange::new(2, 2)),
            "impl X {\n    fn b() {}\n}\n"
        );
        assert_eq!(
            delete_symbol_lines(body, LineRange::new(4, 4)),
            "impl X {\n    fn a() {}\n}\n"
        );
        // Adjacent neighbours stay adjacent; a deleted first symbol leaves no leading blank.
        assert_eq!(
            delete_symbol_lines("a\nprobe\nb\n", LineRange::new(2, 2)),
            "a\nb\n"
        );
        assert_eq!(
            delete_symbol_lines("probe\n\nb\n", LineRange::new(1, 1)),
            "b\n"
        );
    }

    /// Keeps identifier matches while rejecting names embedded in larger ASCII identifiers.
    #[test]
    fn bare_name_prefilter_matches_whole_words() {
        assert!(contains_bare_name(b"def value(): pass", b"value"));
        assert!(contains_bare_name(b"value = 1", b"value"));
        assert!(!contains_bare_name(b"def my_value(): pass", b"value"));
        assert!(!contains_bare_name(b"def value2(): pass", b"value"));
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
