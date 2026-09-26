//! Symbol-addressed worker jobs: `ide.outline`, `ide.read` and `ide.symbol` (v0.4).
//!
//! Every job observes the source through Workspace exactly like `ide.context`, asks the binding's
//! live language server for document symbols, normalizes them through the language module and
//! renders the compact text the contract specifies. Results are retained with the source
//! observation so a later `ide.edit` may name them as `source_ref`.

use super::*;
use crate::lang::{
    self, Language as Lang, LineRange, Outline, SymbolPath,
    render::{self, Call, SymbolCard, Usage},
};

impl Worker<'_> {
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

    /// `ide.symbol {symbol, usages?, callers?, callees?}`: the symbol card.
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
            definition: Some(render::read_text(
                &file,
                Some(&found.path.to_string()),
                LineRange::new(
                    found.range.start,
                    found.range.end.min(found.range.start + 24),
                ),
                source,
            )),
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
                    .usage_lines(&worktree_root, &found.path, references)
                    .await;
            }
            if callers_depth > 0 {
                let live = self.live_session_for(job, &observed).await?;
                if let Ok(calls) = live
                    .session
                    .incoming_calls(&observed, &bytes, byte_offset)
                    .await
                {
                    card.callers = calls
                        .into_iter()
                        .map(|call| Call {
                            name: call.from.name,
                            file: render::display_path(&worktree_root, &call.from.uri),
                            line: call.from.selection_range.start.line + 1,
                        })
                        .collect();
                }
            }
            if callees_depth > 0 {
                let live = self.live_session_for(job, &observed).await?;
                if let Ok(calls) = live
                    .session
                    .outgoing_calls(&observed, &bytes, byte_offset)
                    .await
                {
                    card.callees = calls
                        .into_iter()
                        .map(|call| Call {
                            name: call.to.name,
                            file: render::display_path(&worktree_root, &call.to.uri),
                            line: call.to.selection_range.start.line + 1,
                        })
                        .collect();
                }
            }
        }
        let authority = self.finish_symbol_job(job, &binding, &observed).await?;
        let text = render::symbol_card_text(&card);
        let (reply, page) =
            ContextPageState::new(text, 0, false, ResultKind::Symbol).next(&job.reference)?;
        self.shared.set_context_page(&job.reference, page);
        Ok((reply, Some(authority), Some(observed)))
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
            if definition.file() == Some(relative.as_path()) {
                // Declaration lines are filtered below by text once the source is known.
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

    /// Shared tail of every symbol job: epoch fence, deadline, authority and liveness checks.
    async fn finish_symbol_job(
        &mut self,
        job: &Job,
        binding: &BindingRef,
        observed: &SourceObservation,
    ) -> Result<AuthorityStamp, FailureCode> {
        let epoch = self
            .shared
            .ledger
            .lock()
            .map_err(|_| FailureCode::Internal)?
            .native_epoch
            .get(binding)
            .copied()
            .unwrap_or(0);
        if epoch != job.native_epoch {
            return Err(FailureCode::SourceUnavailable);
        }
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
