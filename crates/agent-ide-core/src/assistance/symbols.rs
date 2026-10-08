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
            && let Some(note) = self.lexical_note(job, observed.path(), why)
        {
            text.push_str(&note);
            text.push('\n');
        }
        let (reply, page) =
            ContextPageState::new(text, 0, false, ResultKind::Outline).next(&job.reference)?;
        self.shared.set_context_page(&job.reference, page);
        Ok((reply, Some(authority), Some(observed)))
    }

    /// `ide.read {symbol}`, `ide.read {path, lines}` or `ide.read {path}`: numbered source with
    /// the header.
    ///
    /// `path` alone reads the whole file (`(empty file)` when it has no lines), paged by bytes
    /// like any read. Any readable text file answers the `path` forms, whatever its type; a file
    /// no IDE language analyzes ends with [`NO_ANALYSIS_NOTE`]. A binary or unreadable file
    /// is refused as `source_unavailable` with a `read:not_text:` or `read:source_unavailable:`
    /// detail (a soft refusal naming native tools, never `internal`); a `lines` range past the
    /// end is refused `read:line_range`. The `symbol` form still needs an outline, so a file no
    /// IDE language reads answers `unsupported_file`.
    pub(super) async fn read(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        // Batch reads (v0.7): several symbols across files, or several ranges of one file — one
        // reply in request order, one `source_ref` valid for every file it included.
        if job.parameters.get("symbols").is_some() {
            let requested = job.parameters["symbols"]
                .as_array()
                .ok_or(FailureCode::Internal)?
                .clone();
            return self.read_batch(job, &requested, ReadBatch::Symbols).await;
        }
        if job.parameters.get("ranges").is_some() {
            let requested = job.parameters["ranges"]
                .as_array()
                .ok_or(FailureCode::Internal)?
                .clone();
            return self.read_batch(job, &requested, ReadBatch::Ranges).await;
        }
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
                let (path, range, title) = self
                    .sigil_definition(job, &binding, namespace, name)
                    .await?;
                (path, Some(range), title)
            }
            (Some(symbol), None) => {
                let symbol = SymbolPath::parse(symbol).map_err(|_| FailureCode::UnknownSymbol)?;
                let file = symbol
                    .file()
                    .ok_or(FailureCode::UnknownSymbol)?
                    .to_path_buf();
                let (observed, bytes) =
                    self.observe(&binding, file.clone())
                        .await
                        .inspect_err(|code| {
                            if matches!(code, FailureCode::SourceUnavailable) {
                                job.failure_detail =
                                    Some(format!("read:source_unavailable:{}", file.display()));
                            }
                        })?;
                let (outline, _, from_text) = self.outline_of(job, &observed, &bytes).await?;
                let found = outline
                    .find(&symbol)
                    .ok_or_else(|| missing_symbol(job, from_text.clone()))?;
                lexical = from_text
                    .as_ref()
                    .and_then(|why| self.lexical_note(job, observed.path(), why));
                (file, Some(found.range), symbol.to_string())
            }
            (None, None) => {
                let path = job.parameters["path"]
                    .as_str()
                    .ok_or(FailureCode::SourceUnavailable)?
                    .to_owned();
                // `path` alone (no `lines`) reads the whole file.
                let range = match job.parameters.get("lines") {
                    Some(lines) => Some(
                        lines
                            .as_str()
                            .and_then(crate::assistance::facade::parse_line_range)
                            .ok_or(FailureCode::SourceUnavailable)?,
                    ),
                    None => None,
                };
                (std::path::PathBuf::from(&path), range, path)
            }
        };
        let (observed, bytes) = self
            .observe(&binding, path.clone())
            .await
            .inspect_err(|code| {
                if matches!(code, FailureCode::SourceUnavailable) {
                    job.failure_detail =
                        Some(format!("read:source_unavailable:{}", path.display()));
                }
            })?;
        if let Some(code) = no_such_file(observed.state(), &path.to_string_lossy()) {
            return Err(code);
        }
        let source = readable_text(job, &observed, &bytes, &path)?;
        let total = lang::line_count(source);
        if let Some(range) = range
            && (range.start > total || range.end > total)
        {
            job.failure_detail = Some(format!(
                "read:line_range:file has {total} lines; requested {}-{}",
                range.start, range.end
            ));
            return Err(FailureCode::SourceUnavailable);
        }
        let authority = self.finish_symbol_job(job, &binding, &observed).await?;
        let mut text = match range {
            Some(range) => render::read_text(
                &path,
                Some(&title),
                LineRange::new(range.start, range.end.min(total)),
                source,
            ),
            None => whole_file_text(&path, &title, source),
        };
        if let Some(note) = lexical {
            text.push_str(&note);
            text.push('\n');
        }
        if Lang::for_path(&path).is_none() {
            text.push_str(NO_ANALYSIS_NOTE);
        }
        text.push_str(&format!("source_ref: {}\n", job.reference));
        let (reply, page) =
            ContextPageState::new(text, 0, false, ResultKind::Read).next(&job.reference)?;
        self.shared.set_context_page(&job.reference, page);
        Ok((reply, Some(authority), Some(observed)))
    }

    /// `ide.read {symbols}` or `ide.read {path, ranges}` (v0.7): one block per item in request
    /// order, one `source_ref` valid for every file it included, so a batch `ide.edit` on any of
    /// them needs no re-read. Unknown symbols and missing files are reported per item without
    /// failing the rest; an exactly duplicated address renders once; a block is never cut
    /// mid-body — what the reply budget could not hold is listed with the exact follow-up call,
    /// and only files whose blocks were all delivered get an edit base retained.
    ///
    /// A `symbols` item may also be a bare file path: for a file no IDE language reads it renders
    /// the whole text (as `ide.read {path}`), while a symbol address into such a file is one soft
    /// item (`<file> has no code symbols; read it with ide.read {path, lines|ranges}`), in
    /// whichever order the two come. A binary or unreadable file is one `not a readable text
    /// file` item, a missing file one `no such file` item. When any delivered file is one no IDE
    /// language analyzes the reply carries [`NO_ANALYSIS_NOTE`] once, ahead of `source_ref`.
    /// Failures that are not about one file (a malformed address, a deadline) still fail the call.
    async fn read_batch(
        &mut self,
        job: &mut Job,
        requested: &[Value],
        form: ReadBatch,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        // One observation, one outline and one finish per distinct file; items reference files
        // by index so request order survives.
        let mut files: Vec<(PathBuf, SourceObservation, String, Option<Outline>)> = Vec::new();
        // (rendered block, the file whose observation it retains, the address exactly as
        // requested) — duplicates never become items, so an item's own address is what the
        // continuation footer must name, never an index back into `requested`.
        let mut items: Vec<(String, Option<usize>, String)> = Vec::new();
        let mut duplicates: Vec<String> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for entry in requested {
            let Some(address) = entry.as_str() else {
                return Err(FailureCode::Internal);
            };
            if seen.contains(&address.to_owned()) {
                if !duplicates.contains(&address.to_owned()) {
                    duplicates.push(address.to_owned());
                }
                continue;
            }
            seen.push(address.to_owned());
            match form {
                ReadBatch::Symbols => {
                    let Ok(symbol) = SymbolPath::parse_item(address) else {
                        return Err(FailureCode::UnknownSymbol);
                    };
                    let Some(file) = symbol.file().map(Path::to_path_buf) else {
                        return Err(FailureCode::UnknownSymbol);
                    };
                    // A bare path of a file no IDE language reads has no outline to resolve: it
                    // reads as text, like `ide.read {path}`.
                    let text_file = symbol.segments().is_empty() && Lang::for_path(&file).is_none();
                    let index = match self
                        .read_batch_file(job, &binding, &mut files, file.clone(), !text_file)
                        .await
                    {
                        Ok(Some(index)) => index,
                        Ok(None) => {
                            items.push((
                                format!("{NOT_TEXT_REFUSAL}: {}\n", file.display()),
                                None,
                                address.to_owned(),
                            ));
                            continue;
                        }
                        Err(
                            code @ (FailureCode::NoSuchFile(_) | FailureCode::UnsupportedFile(_)),
                        ) => {
                            items.push((
                                batch_file_refusal(&code, &file.display().to_string()),
                                None,
                                address.to_owned(),
                            ));
                            continue;
                        }
                        Err(code) => return Err(code),
                    };
                    let (_, _, source, outline) = &files[index];
                    if text_file {
                        items.push((
                            whole_file_text(&file, address, source),
                            Some(index),
                            address.to_owned(),
                        ));
                        continue;
                    }
                    // A file read earlier in the batch without an outline (a bare path or a range
                    // item) has no symbols to resolve, whatever order the items came in.
                    let Some(outline) = outline else {
                        items.push((
                            batch_file_refusal(
                                &FailureCode::UnsupportedFile(String::new()),
                                &file.display().to_string(),
                            ),
                            None,
                            address.to_owned(),
                        ));
                        continue;
                    };
                    match outline.find(&symbol) {
                        Some(found) => {
                            let text = render::read_text(&file, Some(address), found.range, source);
                            items.push((text, Some(index), address.to_owned()));
                        }
                        None => items.push((
                            format!(
                                "no such symbol: {address} — check ide.outline \
                                 {{\"path\":\"{}\"}}\n",
                                file.display()
                            ),
                            None,
                            address.to_owned(),
                        )),
                    }
                }
                ReadBatch::Ranges => {
                    let path = job.parameters["path"]
                        .as_str()
                        .ok_or(FailureCode::Internal)?
                        .to_owned();
                    let range = crate::assistance::facade::parse_line_range(address)
                        .ok_or(FailureCode::Internal)?;
                    let file = std::path::PathBuf::from(&path);
                    let index = match self
                        .read_batch_file(
                            job,
                            &binding,
                            &mut files,
                            file.clone(),
                            // A language file keeps the outline its ranges read always took,
                            // which registers it with the language session before any edit; a
                            // file no language reads has none to take.
                            Lang::for_path(&file).is_some(),
                        )
                        .await
                    {
                        Ok(Some(index)) => index,
                        Ok(None) => {
                            items.push((
                                format!("{NOT_TEXT_REFUSAL}: {path}\n"),
                                None,
                                address.to_owned(),
                            ));
                            continue;
                        }
                        Err(
                            code @ (FailureCode::NoSuchFile(_) | FailureCode::UnsupportedFile(_)),
                        ) => {
                            items.push((
                                batch_file_refusal(&code, &path),
                                None,
                                address.to_owned(),
                            ));
                            continue;
                        }
                        Err(code) => return Err(code),
                    };
                    let (_, _, source, _) = &files[index];
                    let total = lang::line_count(source);
                    if range.start > total {
                        items.push((
                            format!(
                                "no such range: lines {range} is past the end of {path} \
                                 ({total} lines)\n"
                            ),
                            None,
                            address.to_owned(),
                        ));
                        continue;
                    }
                    let clamped = LineRange::new(range.start, range.end.min(total));
                    let text = render::read_text(&file, None, clamped, source);
                    items.push((text, Some(index), address.to_owned()));
                }
            }
        }
        // Which files' blocks were delivered decides both the `source_ref` line's file list and
        // which observations stay retained as edit bases.
        let reference = job.reference.clone();
        let fits = |text: &str| {
            content::fits(
                &PeerReply::Complete {
                    kind: ResultKind::Read,
                    text: text.to_owned(),
                    detail_ref: Some(reference.clone()),
                    truncated: true,
                    continuation: false,
                },
                content::Envelope::WithStructured,
            )
        };
        let source_line = |delivered: &[usize]| {
            let names: Vec<String> = delivered
                .iter()
                .map(|&index| files[index].0.display().to_string())
                .collect();
            // Text read of a file no IDE language analyzes: say so once, ahead of the reference.
            let note = if delivered
                .iter()
                .any(|&index| Lang::for_path(&files[index].0).is_none())
            {
                NO_ANALYSIS_NOTE
            } else {
                ""
            };
            if names.len() > 1 {
                format!(
                    "{note}source_ref: {} (valid for {})\n",
                    job.reference,
                    names.join(", ")
                )
            } else {
                format!("{note}source_ref: {}\n", job.reference)
            }
        };
        // The continuation footer names exactly the cut items' own requested addresses and the
        // exact call that fetches them — one quoted entry per address, so the call is well formed.
        let follow_up = |items: &[(String, Option<usize>, String)], cut: &[usize]| -> String {
            let names: Vec<&str> = cut
                .iter()
                .map(|&index| items[index].2.as_str())
                .take(MAX_LANDING_SENTENCES)
                .collect();
            let hidden = cut.len().saturating_sub(MAX_LANDING_SENTENCES);
            match form {
                ReadBatch::Symbols => {
                    let list = names
                        .iter()
                        .map(|name| format!("\"{name}\""))
                        .collect::<Vec<_>>()
                        .join(",");
                    let mut footer = format!(
                        "not included (over the reply budget): {} — call ide.read \
                         {{\"symbols\":[{list}]}}",
                        names.join(", ")
                    );
                    if hidden > 0 {
                        footer.push_str(&format!("; +{hidden} more"));
                    }
                    footer.push('\n');
                    footer
                }
                ReadBatch::Ranges => {
                    let path = job.parameters["path"].as_str().unwrap_or_default();
                    let call = names
                        .iter()
                        .map(|name| format!("\"{name}\""))
                        .collect::<Vec<_>>()
                        .join(",");
                    let mut footer = format!(
                        "not included (over the reply budget): {} — call ide.read \
                         {{\"path\":\"{path}\",\"ranges\":[{call}]}}",
                        names.join(", ")
                    );
                    if hidden > 0 {
                        footer.push_str(&format!("; +{hidden} more"));
                    }
                    footer.push('\n');
                    footer
                }
            }
        };
        // Compose whole blocks in request order until one more would not fit; never split a
        // block. A first block larger than the whole budget pages by bytes like any read.
        let mut included = 0usize;
        let mut cut: Vec<usize> = Vec::new();
        let mut delivered: Vec<usize> = Vec::new();
        for index in 0..items.len() {
            let (_, file, _) = &items[index];
            let mut candidate_delivered = delivered.clone();
            if let Some(file) = file
                && !candidate_delivered.contains(file)
            {
                candidate_delivered.push(*file);
            }
            let candidate = items[..=index]
                .iter()
                .map(|(text, ..)| text.as_str())
                .collect::<String>()
                + &source_line(&candidate_delivered);
            if fits(&candidate) {
                included = index + 1;
                delivered = candidate_delivered;
            } else {
                cut.extend(index..items.len());
                break;
            }
        }
        let mut text = items[..included]
            .iter()
            .map(|(text, ..)| text.as_str())
            .collect::<String>();
        text.push_str(&source_line(&delivered));
        if !duplicates.is_empty() {
            text.push_str(&duplicates_footer(&duplicates));
        }
        if !cut.is_empty() {
            text.push_str(&follow_up(&items, &cut));
        }
        // The footers may push the whole reply past the budget: give blocks back until it fits.
        while included > 0 && !fits(&text) {
            included -= 1;
            if let Some((_, Some(file), _)) = items.get(included)
                && !items[..included]
                    .iter()
                    .any(|(_, delivered, _)| *delivered == Some(*file))
            {
                delivered.retain(|&index| index != *file);
            }
            cut.insert(0, included);
            text = items[..included]
                .iter()
                .map(|(text, ..)| text.as_str())
                .collect::<String>();
            text.push_str(&source_line(&delivered));
            text.push_str(&follow_up(&items, &cut));
        }
        // Retain one edit base per delivered file: the first returns through the job tuple, the
        // rest ride the same detail as extra sources. A file whose every block was cut gets none.
        let mut sources: Vec<SourceObservation> = delivered
            .iter()
            .map(|&index| files[index].1.clone())
            .collect();
        let authority = self.authority(&binding).await.ok();
        if included == 0 && items.iter().any(|(_, file, _)| file.is_some()) {
            // One block larger than the whole budget: deliver it alone through the ordinary
            // byte paging (editable once its last page has been delivered, T16B).
            let index = items
                .iter()
                .position(|(_, file, _)| file.is_some())
                .unwrap_or_default();
            let file = items[index].1.expect("selected item carries a file");
            let mut oversized = items[index].0.clone();
            oversized.push_str(&source_line(&[file]));
            let (reply, page) = ContextPageState::new(oversized, 0, false, ResultKind::Read)
                .next(&job.reference)?;
            self.shared.set_context_page(&job.reference, page);
            return Ok((reply, authority, Some(files[file].1.clone())));
        }
        let job_source = (!sources.is_empty()).then(|| sources.remove(0));
        self.shared.add_edit_sources(&job.reference, sources);
        let truncated = !cut.is_empty();
        let (reply, page) =
            ContextPageState::new(text, 0, false, ResultKind::Read).next(&job.reference)?;
        self.shared.set_context_page(&job.reference, page);
        let reply = match reply {
            PeerReply::Complete {
                kind,
                text,
                detail_ref,
                ..
            } => PeerReply::Complete {
                kind,
                text,
                detail_ref,
                truncated,
                continuation: false,
            },
            other => other,
        };
        Ok((reply, authority, job_source))
    }

    /// Observes, outlines and finishes one file of a batch read once: every item of that file
    /// shares the observation, the outline and the deadline/authority/source checks.
    ///
    /// `outline` is false for a bare-path whole-file text item and for the ranges of a file no
    /// IDE language reads: neither needs one, so any readable file answers them. The file is
    /// then retained with `None` for its outline, and a later symbol item of the same file must
    /// not look it up. The ranges of a language file still take the outline, as ever. Returns `Ok(None)` for a file that
    /// is binary or has no observed bytes (the caller reports it as one soft item), `Ok(Some(index))`
    /// into `files` otherwise, and `NoSuchFile`/`UnsupportedFile` errors the caller also turns into
    /// items; every other error fails the whole call.
    async fn read_batch_file(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        files: &mut Vec<(PathBuf, SourceObservation, String, Option<Outline>)>,
        path: PathBuf,
        outline: bool,
    ) -> Result<Option<usize>, FailureCode> {
        if let Some(index) = files.iter().position(|(file, ..)| *file == path) {
            return Ok(Some(index));
        }
        let (observed, bytes) = self
            .observe(binding, path.clone())
            .await
            .inspect_err(|code| {
                if matches!(code, FailureCode::SourceUnavailable) {
                    job.failure_detail =
                        Some(format!("read:source_unavailable:{}", path.display()));
                }
            })?;
        let display = path.display().to_string();
        if let Some(code) = no_such_file(observed.state(), &display) {
            return Err(code);
        }
        let Ok(source) = readable_text(job, &observed, &bytes, &path).map(str::to_owned) else {
            return Ok(None);
        };
        let outline = match outline {
            true => Some(self.outline_of(job, &observed, &bytes).await?.0),
            false => None,
        };
        self.finish_symbol_job(job, binding, &observed).await?;
        files.push((path, observed, source, outline));
        Ok(Some(files.len() - 1))
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
                        let mut candidates = candidates
                            .iter()
                            .map(|candidate| candidate.path.to_string())
                            .collect::<Vec<_>>();
                        candidates.sort();
                        candidates.dedup();
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
            // An outline that answered from source because its documentSymbols exchange failed
            // was resolved against a ready session: hover and references may still answer. When
            // the session or the references exchange then fails, the card keeps the definition
            // facts the source outline gave it and every section a live session would answer
            // names the failure — the same shape as the unavailable branch above — instead of
            // failing the whole call.
            let exchange = matches!(lexical.as_ref(), Some(Lexical::Exchange { .. }));
            let live = match self.live_session_for(job, &observed).await {
                Ok(live) => Some(live),
                Err(FailureCode::ProviderUnavailable) if exchange => None,
                Err(code) => return Err(code),
            };
            let mut degraded = live
                .is_none()
                .then(|| "workspace failed to load".to_owned());
            let mut references: Option<Vec<async_lsp::lsp_types::Location>> = None;
            if let Some(live) = live {
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
                    match live
                        .session
                        .references(&observed, &bytes, byte_offset)
                        .await
                    {
                        Ok(found) => references = Some(found),
                        // This branch runs only after the outline's documentSymbols exchange
                        // failed, which already marked the session failed.
                        Err(_) if exchange => {
                            degraded = Some("references request failed".to_owned());
                        }
                        Err(_) => {
                            self.providers.note_session_fault();
                            // The ready session failed this exchange; the stage names the request
                            // so the refusal's reply can say what still answers and how to
                            // recover.
                            if let Some(name) = server_name {
                                job.set_stage_failure(
                                    &FailureCode::ProviderUnavailable,
                                    &format!(
                                        "{name}: references request failed{}",
                                        super::providers::session_fallback_clause(
                                            outline.language.support().outline_while_loading()
                                        )
                                    ),
                                );
                            }
                            return Err(FailureCode::ProviderUnavailable);
                        }
                    }
                }
            }
            if let Some(references) = references {
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
            if let Some(reason) = degraded.as_deref() {
                let note =
                    |reason: &str| server_name.map(|name| format!("unavailable ({name} {reason})"));
                if want_usages && outline.language.names().is_none() {
                    card.usages_note = note(reason);
                }
                if callers_depth > 0 {
                    card.callers_note = note(reason);
                }
                if callees_depth > 0 {
                    card.callees_note = note(reason);
                }
            }
            if degraded.is_none() && callers_depth > 0 {
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
                    let mut faulted = false;
                    let calls = match self.live_session_for(job, &observed).await {
                        Ok(live) => match live
                            .session
                            .prepare_call_hierarchy(&observed, &bytes, byte_offset)
                            .await
                        {
                            Err(_) => {
                                faulted = true;
                                Err("prepare call hierarchy request failed")
                            }
                            Ok(items) => match items.into_iter().next() {
                                Some(item) => {
                                    live.session.incoming_calls_for(item).await.map_err(|_| {
                                        faulted = true;
                                        "incoming calls request failed"
                                    })
                                }
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
                    if faulted {
                        self.providers.note_session_fault();
                    }
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
            if degraded.is_none() && callees_depth > 0 {
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
                        self.providers.note_session_fault();
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
            let mut faulted = false;
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
            .unwrap_or_else(|_| {
                faulted = true;
                Vec::new()
            });
            if faulted {
                self.providers.note_session_fault();
            }
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
        // A file no registered language reads (`Cargo.toml`, a script) has no outline at all: the
        // caller asked the wrong tool, which a provider-unavailable answer would hide.
        let language = Lang::for_path(observed.path()).ok_or_else(|| {
            FailureCode::UnsupportedFile(bounded_utf8_prefix(
                &observed.path().to_string_lossy(),
                MAX_NO_SUCH_FILE_PATH_BYTES,
            ))
        })?;
        let support = language.support();
        let source = observed_text(observed, bytes)?.to_owned();
        let worktree_root = observed.worktree().worktree_path().to_path_buf();
        let Some(server) = self.session_server(observed.path()) else {
            // No registered server owns the file: a language that outlines from its text still
            // answers; any other keeps the provider-unavailable refusal.
            return match support.outline_from_source(observed.path(), &source) {
                Some(outline) => Ok((outline, worktree_root, None)),
                None => {
                    job.set_stage_failure(
                        &FailureCode::ProviderUnavailable,
                        &format!(
                            "no {} server serves this file and its source outline refused it{}",
                            language.name(),
                            super::providers::session_fallback_clause(false)
                        ),
                    );
                    Err(FailureCode::ProviderUnavailable)
                }
            };
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
                    None => {
                        // The stage `live_session_for` named promises a source outline this file
                        // does not have: say native reads instead, keeping the server's cause.
                        let promise =
                            format!("{})", super::providers::session_fallback_clause(true));
                        match job.failure_detail.as_mut() {
                            Some(detail) if detail.ends_with(&promise) => {
                                detail.truncate(detail.len() - promise.len());
                                detail.push_str(&format!(
                                    "; the source outline refused this file{})",
                                    super::providers::session_fallback_clause(false)
                                ));
                            }
                            Some(_) => {}
                            None => job.set_stage_failure(
                                &FailureCode::ProviderUnavailable,
                                &format!(
                                    "{} is unavailable and the source outline refused this file{}",
                                    server.name(),
                                    super::providers::session_fallback_clause(false)
                                ),
                            ),
                        }
                        Err(FailureCode::ProviderUnavailable)
                    }
                };
            }
            Err(FailureCode::ResolutionUnverified)
                if support.outline_when_resolution_unverified() =>
            {
                // The server could not verify this file's project inputs (a tsconfig policy the
                // exact-resolution rules refuse, a config outside the observed set): the file
                // itself still scans, so its outline answers from source exactly as a language
                // without a server answers, with the refusal reason in the footer. A file that
                // does not scan cleanly keeps the refusal.
                let cause = job
                    .failure_detail
                    .clone()
                    .unwrap_or_else(|| "semantic project resolution is unverified".to_owned());
                return match support.outline_from_source(observed.path(), &source) {
                    Some(outline) => {
                        job.failure_detail = None;
                        Ok((outline, worktree_root, Some(Lexical::Unverified { cause })))
                    }
                    None => Err(FailureCode::ResolutionUnverified),
                };
            }
            Err(other) => return Err(other),
        };
        let symbols = match live.session.document_symbols(observed, bytes).await {
            Ok(symbols) => symbols,
            Err(error) => {
                self.providers.note_session_fault();
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
                // This refusal is reached only for a language without source outlines or a file
                // its scanner refuses, so no source outline answers for this call: the stage
                // says so instead of promising one.
                job.set_stage_failure(
                    &FailureCode::ProviderUnavailable,
                    &format!(
                        "{}: documentSymbols request failed; use native reads",
                        server.name()
                    ),
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
    /// file's registered server did not answer it (`why`: still loading, unavailable, its
    /// documentSymbols exchange failed, or it refused the file's project inputs): the outline
    /// is exact, so the call needs no repeat, but semantic facts (usages, callers) are not
    /// included. `None` when no server owns the file (nothing is loading, failed or refused).
    /// A note makes the call a degraded success in the journal (QW-4): `job`'s call is marked.
    fn lexical_note(&self, job: &Job, path: &Path, why: &Lexical) -> Option<String> {
        let server = self.session_server(path)?;
        let state = match why {
            Lexical::Loading => "still indexing".to_owned(),
            Lexical::Unavailable => "unavailable".to_owned(),
            Lexical::Exchange { cause } => format!("request failed: {cause}"),
            Lexical::Unverified { cause } => format!("project resolution unverified: {cause}"),
        };
        self.shared.mark_degraded(&job.reference);
        Some(format!(
            "{}{} {state}; no need to repeat)",
            crate::telemetry::adapters::LEXICAL_OUTLINE_NOTE,
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
            job.set_stage_failure(
                &FailureCode::ProviderUnavailable,
                "symbols: no language file found to search",
            );
            return Err(FailureCode::ProviderUnavailable);
        }
        // (relative file, rendered candidate, is impl) in language and provider order.
        let mut matches: Vec<(std::path::PathBuf, String, bool)> = Vec::new();
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
                    Err(_) => {
                        self.providers.note_session_fault();
                        Vec::new()
                    }
                };
            if !workspace_hits.is_empty() {
                matches.extend(workspace_hits.into_iter().map(|(path, container)| {
                    let candidate = match container {
                        Some(container) => format!("{}#{container}/{name}", path.display()),
                        None => format!("{}#{name}", path.display()),
                    };
                    (path, candidate, false)
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
                let Some(language) = Lang::for_path(file) else {
                    continue;
                };
                let support = language.support();
                let Ok(live) = self.live_session_for(job, &observed).await else {
                    continue;
                };
                let outcome = live.session.document_symbols(&observed, bytes).await;
                if outcome.is_err() {
                    self.providers.note_session_fault();
                }
                if let Ok(symbols) = outcome {
                    answered = true;
                    let source = String::from_utf8_lossy(bytes);
                    let outline = support.normalize(file, &source, symbols);
                    for candidate in outline.named(name) {
                        let path = format!("{}#{}", file.display(), candidate.path);
                        let implementation = candidate.kind == crate::lang::SymbolKind::Impl;
                        matches.push((file.clone(), path, implementation));
                    }
                }
            }
        }
        // Every language was searched: a name several languages share is ambiguous.
        deduplicate_symbol_candidates(&mut matches);
        if !answered {
            job.set_stage_failure(
                &FailureCode::ProviderUnavailable,
                "symbols: no language server answered the workspace search",
            );
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
                    .map(|(_, candidate, _)| candidate)
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

/// Sorts same-name resolutions by rendered path, preferring declarations to impl blocks, then
/// removes duplicate rendered paths so one exact address is never reported twice.
fn deduplicate_symbol_candidates(matches: &mut Vec<(std::path::PathBuf, String, bool)>) {
    matches.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.2.cmp(&b.2)));
    matches.dedup_by(|a, b| a.1 == b.1);
}

#[cfg(test)]
mod symbol_candidate_tests {
    use super::deduplicate_symbol_candidates;
    use std::path::PathBuf;

    /// A declaration and its impl sharing one rendered address collapse to the declaration.
    #[test]
    fn candidate_addresses_are_unique_and_prefer_the_type() {
        let path = PathBuf::from("src/model.rs");
        let mut candidates = vec![
            (path.clone(), "src/model.rs#Outcome".to_owned(), true),
            (path, "src/model.rs#Outcome".to_owned(), false),
        ];
        deduplicate_symbol_candidates(&mut candidates);
        assert_eq!(candidates.len(), 1);
        assert!(!candidates[0].2);
    }
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
    /// The registered server refused to verify this file's project inputs, a terminal state for
    /// the file; the bounded cause names the refused input in the outline footer.
    Unverified {
        /// The server's bounded refusal reason, cut to [`EXCHANGE_CAUSE_LIMIT`] bytes.
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
/// `provider_unavailable` at once instead of parking. A source outline that answered because
/// the server refused the file's project inputs scanned exactly what it reported, so a miss
/// there names the address unknown rather than refusing again.
pub(super) fn missing_symbol(job: &mut Job, lexical: Option<Lexical>) -> FailureCode {
    match lexical {
        None | Some(Lexical::Unverified { .. }) => FailureCode::UnknownSymbol,
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

/// The text of a whole file for `ide.read {path}` and a bare-path batch item: every line
/// numbered, or `(empty file)` for a file with no lines. `title` heads the block.
fn whole_file_text(file: &Path, title: &str, source: &str) -> String {
    match lang::line_count(source) {
        0 => format!("{title}  (empty file)\n"),
        total => render::read_text(file, Some(title), LineRange::new(1, total), source),
    }
}

/// Reply line a text read of a file no IDE language analyzes ends with, ahead of `source_ref`.
const NO_ANALYSIS_NOTE: &str = "note: no code analysis for this format; showing its text only (ide.outline and ide.symbol do not read it)\n";

/// Item line of a batch read whose file is binary or could not be read as text.
const NOT_TEXT_REFUSAL: &str = "not a readable text file (binary, not UTF-8 or unreadable; inspect it \
with a native tool such as `file` or `xxd`)";

/// The observed bytes of a read as text, for any readable text file whatever its type.
///
/// A file that is binary (not UTF-8, or holding a NUL byte) or whose bytes were not observed is
/// refused as
/// `source_unavailable` with a `read:` detail naming the path, which the reply renders as a soft
/// refusal pointing at native tools — never as `internal`. Unlike [`observed_text`], which the
/// edit paths share, it records that detail on `job`.
fn readable_text<'a>(
    job: &mut Job,
    observed: &SourceObservation,
    bytes: &'a [u8],
    path: &Path,
) -> Result<&'a str, FailureCode> {
    observed_text(observed, bytes)
        .and_then(|text| {
            if bytes.contains(&0) {
                Err(FailureCode::SourceUnavailable)
            } else {
                Ok(text)
            }
        })
        .inspect_err(|_| {
            job.failure_detail = Some(format!("read:not_text:{}", path.display()));
        })
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

/// The item line for one file a batch read could not use: a symbol address into a file no IDE
/// language reads gets the soft hint to read its text, a missing file `no such file`.
fn batch_file_refusal(code: &FailureCode, file: &str) -> String {
    match code {
        FailureCode::UnsupportedFile(_) => {
            format!("{file} has no code symbols; read it with ide.read {{path, lines|ranges}}\n")
        }
        _ => format!("no such file: {file}\n"),
    }
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
    /// stale-safe edit path with the observation the splice was resolved on as the base. A
    /// line-range request using a moved edit result is refused until the current file is re-read.
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
                    .and_then(|why| self.lexical_note(job, observed.path(), why));
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
                    ledger.details.get(reference).and_then(|detail| {
                        let source = admitted_edit_source(detail, &binding, reference, &path)?;
                        let movement =
                            admitted_edit_line_movement(detail, &binding, reference, &path);
                        Some((source, movement))
                    })
                });
                match retained {
                    Some((_, Some((line, delta)))) if job.parameters.get("symbol").is_none() => {
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
                                note: Some(format!(
                                    "stale_source (edit:lines_moved); the edit that produced {reference} moved lines after line {line} by {delta:+}; line numbers must come from a read of the current file — re-read with ide.read {{path, lines}}"
                                )),
                                operation: None,
                            },
                            authority,
                            None,
                        ));
                    }
                    Some((retained, _)) if retained.bytes() == observed.bytes() => retained,
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
        // A single-change call is a batch of one through the same applier, so its range-past-end
        // refusal uses the batch grammar; only its success reply stays legacy.
        let single =
            single_change(&op, &splice, content.as_deref()).ok_or(FailureCode::Internal)?;
        let (candidate, single_landing) =
            match apply_changes(&source, &path, std::slice::from_ref(&single)) {
                Ok(applied) => applied,
                Err(sentence) => {
                    job.failure_detail = Some(format!(
                        "edit:refused: {}",
                        crate::assistance::reply::bounded_utf8_prefix(
                            &sentence.detail(),
                            MAX_REFUSAL_DETAIL_BYTES
                        )
                    ));
                    return Err(FailureCode::EditRefused);
                }
            };
        let formatted = self.format_candidate(&observed, &file, candidate).await;
        // Pre-write structural gate, shared with batches and file creation: a candidate that
        // does not parse is not written; a base that already failed is edited with a note.
        let gated = self
            .gate_candidate(job, &observed, &file, &source, &formatted)
            .await;
        let pre_existing = match gated {
            Ok(note) => note,
            Err((line, message)) => {
                job.failure_detail = Some(format!(
                    "edit:refused: {}",
                    crate::assistance::reply::bounded_utf8_prefix(
                        &format!(
                            "1 of 1 changes refused, nothing written — {}",
                            syntax_sentence(
                                &formatted,
                                std::slice::from_ref(&single),
                                &single_landing,
                                line,
                                &message
                            )
                        ),
                        MAX_REFUSAL_DETAIL_BYTES
                    )
                ));
                return Err(FailureCode::EditRefused);
            }
        };
        job.format_note = line_shift_note(&source, &formatted, splice_start(&splice));
        if let Some(note_line) = pre_existing {
            job.format_note = Some(match job.format_note.take() {
                Some(existing) => format!("{existing}\n{note_line}"),
                None => note_line,
            });
        }
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

    /// `ide.edit {operation_id, path, source_ref?, changes}`: many changes to one file in one
    /// call. Every address resolves against the bytes `source_ref` names (or, for a symbol-only
    /// batch, the fresh observation) before anything is applied; application is bottom-up so
    /// each change's final range is exact; the formatted candidate must parse; then one atomic
    /// stale-safe write carries the whole batch. A refused batch writes nothing and consumes
    /// neither the `operation_id` nor a receipt, so the same id retries. Line-addressed entries
    /// using an edit result that moved lines are refused until the current file is re-read.
    #[allow(clippy::too_many_lines)]
    pub(super) async fn edit_changes(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let operation_id = job.parameters["operation_id"]
            .as_str()
            .ok_or(FailureCode::Internal)?
            .to_owned();
        let path = job.parameters["path"]
            .as_str()
            .ok_or(FailureCode::Internal)?
            .to_owned();
        let file = std::path::PathBuf::from(&path);
        let (observed, bytes) = self.observe(&binding, file.clone()).await?;
        if let Some(code) = no_such_file(observed.state(), &path) {
            return Err(code);
        }
        let source = observed_text(&observed, &bytes)?.to_owned();
        // Exactly the single form's rule: an explicit `source_ref` must name a retained
        // same-path observation whose bytes are still the file's current ones; without one the
        // fresh observation is the base (only a symbol-only batch may omit it — the facade
        // refused the rest).
        let base = match job.parameters.get("source_ref").and_then(Value::as_str) {
            Some(reference) => {
                let retained = self.shared.ledger.lock().ok().and_then(|ledger| {
                    ledger.details.get(reference).and_then(|detail| {
                        let source = admitted_edit_source(detail, &binding, reference, &path)?;
                        let movement =
                            admitted_edit_line_movement(detail, &binding, reference, &path);
                        Some((source, movement))
                    })
                });
                match retained {
                    Some((_, Some((line, delta))))
                        if job.parameters["changes"].as_array().is_some_and(|entries| {
                            entries.iter().any(|entry| entry.get("lines").is_some())
                        }) =>
                    {
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
                                note: Some(format!(
                                    "stale_source (edit:lines_moved); the edit that produced {reference} moved lines after line {line} by {delta:+}; line numbers must come from a read of the current file — re-read with ide.read {{path, lines}}"
                                )),
                                operation: None,
                            },
                            authority,
                            None,
                        ));
                    }
                    Some((retained, _)) if retained.bytes() == observed.bytes() => retained,
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
        let entries = job.parameters["changes"]
            .as_array()
            .ok_or(FailureCode::Internal)?;
        let requests = parse_change_requests(entries)?;
        // Only `symbol` and `within` addresses resolve against an outline: a batch of `lines`
        // and `old` entries edits a file whose language outlines nowhere here (a probe-checked
        // language without its server) exactly like the single line-range form does, instead of
        // refusing provider_unavailable for addresses it never uses.
        let needs_outline = requests.iter().any(|request| {
            matches!(request, ChangeRequest::Symbol { .. })
                || matches!(
                    request,
                    ChangeRequest::Old {
                        within: Some(_),
                        ..
                    }
                )
        });
        let mut outline: Option<Outline> = None;
        let lexical = if needs_outline {
            let (resolved, _, from_text) = self.outline_of(job, &observed, &bytes).await?;
            outline = Some(resolved);
            from_text
                .as_ref()
                .and_then(|why| self.lexical_note(job, observed.path(), why))
        } else {
            None
        };
        let refused = |job: &mut Job, refusal: Refusal| -> FailureCode {
            job.failure_detail = Some(format!(
                "edit:refused: {}",
                crate::assistance::reply::bounded_utf8_prefix(
                    &refusal.detail(),
                    MAX_REFUSAL_DETAIL_BYTES
                )
            ));
            FailureCode::EditRefused
        };
        let changes = match resolve_changes(&source, outline.as_ref(), &path, &requests) {
            Ok(changes) => changes,
            Err(refusal) => return Err(refused(job, refusal)),
        };
        let (spliced, landings) = match apply_changes(&source, &path, &changes) {
            Ok(applied) => applied,
            Err(refusal) => {
                return Err(refused(job, refusal));
            }
        };
        let candidate = self
            .format_candidate(&observed, &file, spliced.clone())
            .await;
        // Pre-write structural gate: a candidate that does not parse is not written; a file that
        // already failed the same check before the edit is still edited, with a note.
        let gated = self
            .gate_candidate(job, &observed, &file, &source, &candidate)
            .await;
        let pre_existing = match gated {
            Ok(note) => note,
            Err((line, message)) => {
                let mut refusal = Refusal {
                    sentences: vec![syntax_sentence(
                        &candidate, &changes, &landings, line, &message,
                    )],
                    refused: std::collections::BTreeSet::new(),
                    total: changes.len(),
                };
                if let Some(attribution) = syntax_change(&changes, &landings, line) {
                    let number = match attribution {
                        Attribution::Within(number) | Attribution::JustAfter(number, _) => number,
                    };
                    refusal.refused.insert(number);
                } else {
                    refusal
                        .refused
                        .extend(changes.iter().map(|change| change.number));
                }
                return Err(refused(job, refusal));
            }
        };
        let align = if candidate != spliced {
            lang::text::align_lines(
                &spliced.split_inclusive('\n').collect::<Vec<_>>(),
                &candidate.split_inclusive('\n').collect::<Vec<_>>(),
            )
        } else {
            None
        };
        let mut note = landing_note(&changes, &landings, align.as_ref());
        let anchor = changes
            .iter()
            .map(|change| change.base.end)
            .max()
            .unwrap_or(0);
        if let Some(shift) = line_shift_note(
            &source,
            &candidate,
            changes
                .iter()
                .map(|change| change.base.start)
                .min()
                .unwrap_or(anchor),
        ) {
            note.push('\n');
            note.push_str(&shift);
        }
        if let Some(note_line) = pre_existing {
            note.push('\n');
            note.push_str(&note_line);
        }
        if let Some(lexical) = lexical {
            note = format!("{lexical}\n{note}");
        }
        job.format_note = Some(note);
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
        // The same pre-write structural gate as every edit: new content that does not parse is
        // not written (an empty base can never have carried a pre-existing error).
        let gated = self
            .gate_candidate(job, &observed, &file, "", &candidate)
            .await;
        if let Err((line, message)) = gated {
            job.failure_detail = Some(format!(
                "edit:refused: {}",
                crate::assistance::reply::bounded_utf8_prefix(
                    &format!(
                        "1 of 1 changes refused, nothing written — {}",
                        syntax_sentence(&candidate, &[], &[], line, &message)
                    ),
                    MAX_REFUSAL_DETAIL_BYTES
                )
            ));
            return Err(FailureCode::EditRefused);
        }
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
        match run_stdin(&argv, &root, &candidate, Duration::from_secs(10)).await {
            Some(output) if output.status.success() && !output.stdout.is_empty() => {
                String::from_utf8(output.stdout).unwrap_or(candidate)
            }
            _ => candidate,
        }
    }

    /// The stdin-probe programs the launcher declaration of `language`'s server configures, from
    /// this job's target; `None` when no declaration configures that language or it names none.
    /// Opaque paths — core never interprets them (see [`lang::ProbePrograms`]).
    fn configured_probe_programs(&self, job: &Job, language: Lang) -> Option<lang::ProbePrograms> {
        let launch = job
            .target
            .providers
            .iter()
            .find(|launch| launch.language == language)?;
        language.server()?.probe_programs(launch)
    }

    /// The structural verdict for one text: the in-process check first, else its stdin probe run
    /// from the project root with the daemon's formatter PATH and the launcher-configured probe
    /// programs of the file's language. No registered support, checker or runnable probe yields
    /// [`SyntaxVerdict::Unchecked`] — never a refusal — with the reason naming the step that
    /// could not prove a checker, for the gate's error-log event.
    async fn syntax_verdict_of(
        &self,
        job: &Job,
        observed: &SourceObservation,
        file: &Path,
        source: &str,
    ) -> (lang::SyntaxVerdict, Option<&'static str>) {
        let Some(language) = Lang::for_path(file) else {
            return (
                lang::SyntaxVerdict::Unchecked,
                Some("no language owns the file"),
            );
        };
        let support = language.support();
        let verdict = support.syntax_verdict(file, source);
        if verdict != lang::SyntaxVerdict::Unchecked {
            return (verdict, None);
        }
        let root = observed.worktree().worktree_path().to_path_buf();
        let Some(project) = support.detect(&root) else {
            return (
                lang::SyntaxVerdict::Unchecked,
                Some("no project detected for the language"),
            );
        };
        let configured = self.configured_probe_programs(job, language);
        let Some(argv) = support.syntax_probe_command(&project, &root, file, configured.as_ref())
        else {
            let why = if configured.is_none() {
                "no configured probe and no project-local checker"
            } else {
                "the configured probe's module is not on disk and no project-local checker exists"
            };
            return (lang::SyntaxVerdict::Unchecked, Some(why));
        };
        let Some(output) = run_stdin(&argv, &root, source, Duration::from_secs(10)).await else {
            return (
                lang::SyntaxVerdict::Unchecked,
                Some("the probe did not start or finish in time"),
            );
        };
        let first = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .or(String::from_utf8_lossy(&output.stderr).lines().next())
            .unwrap_or_default()
            .to_owned();
        (
            lang::SyntaxVerdict::from_probe(output.status.success(), &first),
            None,
        )
    }

    /// Gates one edit candidate before any write: the formatted candidate must parse. A failure
    /// the base text already had is pre-existing, so the edit proceeds with a note naming it; a
    /// failure on a clean base refuses with the error's line and message for the caller's
    /// per-change attribution. `Ok(Some(note))` proceeds with that note line. One error-log
    /// event records the gate's outcome for every edit — `gate: clean`, `gate: failed …`, or
    /// `gate: unchecked: <reason>` — so a gate that silently cannot engage is diagnosable from
    /// `~/.agent-ide/logs`.
    async fn gate_candidate(
        &mut self,
        job: &Job,
        observed: &SourceObservation,
        file: &Path,
        base: &str,
        candidate: &str,
    ) -> Result<Option<String>, (u32, String)> {
        let (verdict, unchecked) = self.syntax_verdict_of(job, observed, file, candidate).await;
        let record_gate = |outcome: crate::errorlog::Outcome, detail: String| {
            crate::errorlog::record(
                crate::errorlog::Method::Edit,
                outcome,
                crate::errorlog::Fields {
                    worktree: Some(observed.worktree().worktree_path()),
                    correlation: Some(&job.reference),
                    detail: Some(&detail),
                    ..crate::errorlog::Fields::default()
                },
            );
        };
        let lang::SyntaxVerdict::Failed { line, message } = verdict else {
            match verdict {
                lang::SyntaxVerdict::Clean => record_gate(
                    crate::errorlog::Outcome::Completed,
                    "gate: clean".to_owned(),
                ),
                _ => record_gate(
                    crate::errorlog::Outcome::Unavailable,
                    format!("gate: unchecked: {}", unchecked.unwrap_or("unknown")),
                ),
            }
            return Ok(None);
        };
        if let lang::SyntaxVerdict::Failed {
            line: base_line, ..
        } = self.syntax_verdict_of(job, observed, file, base).await.0
        {
            record_gate(
                crate::errorlog::Outcome::Completed,
                format!(
                    "gate: failed line {line}: {message} (base already failed line {base_line}; applied)"
                ),
            );
            return Ok(Some(format!(
                "note: {} already had a syntax error (line {base_line}) before this edit; edit applied",
                file.display()
            )));
        }
        record_gate(
            crate::errorlog::Outcome::Refused,
            format!("gate: failed line {line}: {message}"),
        );
        Err((line, message))
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

/// The single-change form's one resolved change, built from the address the legacy path already
/// resolved, so routing it through [`apply_changes`] yields a byte-identical candidate and the
/// same refusals in the batch grammar. `Err(())` marks a shape the legacy path also refused as
/// internal.
fn single_change(op: &str, splice: &Splice, content: Option<&str>) -> Option<ResolvedChange> {
    let change = |base, action| {
        Some(ResolvedChange {
            number: 1,
            base,
            action,
            // The single form's success reply stays legacy, so its landing sentence is unused.
            address: ChangeAddress::Lines(base),
        })
    };
    match (op, splice) {
        ("insert", Splice::Insert(site)) => change(
            LineRange::new(site.line, site.line),
            ChangeAction::Insert(site.clone(), content.unwrap_or_default().to_owned()),
        ),
        ("delete", Splice::Replace(range)) => change(*range, ChangeAction::Delete),
        (_, Splice::Replace(range)) => change(
            *range,
            ChangeAction::Replace(content.unwrap_or_default().to_owned()),
        ),
        _ => None,
    }
}

/// First source line changed by a line replacement or insertion.
fn splice_start(splice: &Splice) -> u32 {
    match splice {
        Splice::Replace(range) => range.start,
        Splice::Insert(site) => site.line,
    }
}

/// Reports an edit's net line-count change from its first changed line for later line-addressed edits.
fn line_shift_note(source: &str, edited: &str, first_changed_line: u32) -> Option<String> {
    let shift = i64::from(lang::line_count(edited)) - i64::from(lang::line_count(source));
    (shift != 0).then(|| format!("lines after {first_changed_line} moved {shift:+}"))
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

/// Inserts `content` before `site.line` with the site's indentation and blank lines. Blank lines
/// already on either side of the insertion point count toward the site's spacing, so inserting
/// next to a neighbour that is already separated does not double the separation.
fn insert_lines(source: &str, site: &lang::InsertSite, content: &str) -> String {
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let at = (site.line.saturating_sub(1) as usize).min(lines.len());
    let (before, after) = insert_gaps(source, site);
    let mut out = String::with_capacity(source.len() + content.len() + 64);
    let block = indent_block(content, &site.indent);
    if at == lines.len() && !source.is_empty() && !source.ends_with('\n') {
        out.push_str(source);
        out.push('\n');
    } else {
        lines[..at].iter().for_each(|line| out.push_str(line));
    }
    out.push_str(&"\n".repeat(before));
    push_block(&mut out, &block);
    if at < lines.len() {
        out.push_str(&"\n".repeat(after));
        lines[at..].iter().for_each(|line| out.push_str(line));
    }
    out
}

/// The blank lines an insert at `site` adds before and after its block: the site's spacing less
/// the blank lines already on that side of the insertion point.
fn insert_gaps(source: &str, site: &lang::InsertSite) -> (usize, usize) {
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let at = (site.line.saturating_sub(1) as usize).min(lines.len());
    let blank = |line: &&&str| line.trim().is_empty();
    (
        usize::from(site.blank_before)
            .saturating_sub(lines[..at].iter().rev().take_while(blank).count()),
        usize::from(site.blank_after).saturating_sub(lines[at..].iter().take_while(blank).count()),
    )
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

/// Runs one bounded stdin probe — the project's formatter or a language's syntax checker —
/// writing `input` to its stdin and waiting at most `timeout`; `None` when it cannot start or
/// does not finish in time (a late child is killed on drop). Runs from `root` with the daemon's
/// formatter PATH; stderr is kept so a checker's own error line can be read as the probe's head.
async fn run_stdin(
    argv: &[String],
    root: &Path,
    input: &str,
    timeout: Duration,
) -> Option<std::process::Output> {
    let (program, args) = argv.split_first()?;
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(root)
        .env("PATH", formatter_path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let Ok(mut child) = command.spawn() else {
        return None;
    };
    let mut stdin = child.stdin.take()?;
    let input = input.as_bytes().to_vec();
    let writer = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(&input).await;
        let _ = stdin.shutdown().await;
    });
    let output = tokio::time::timeout(timeout, child.wait_with_output()).await;
    writer.abort();
    output.ok()?.ok()
}

// ---------------------------------------------------------------------------------------------
// ide.edit changes (v0.7): one call, many changes — every address resolves against the base
// bytes before anything is applied, application is bottom-up so each change's final range is
// exact by construction, and the formatted candidate must parse before the one atomic write.
// A single-change call is a batch of one through the same code; only its reply stays legacy.
// ---------------------------------------------------------------------------------------------

/// What one resolved change does to the base bytes.
#[derive(Debug)]
enum ChangeAction {
    /// Replace the change's base lines with this text (empty text deletes them).
    Replace(String),
    /// Delete the change's base lines, merging the surrounding blank runs.
    Delete,
    /// Insert this code before the site's line, with the site's indentation and blanks.
    Insert(lang::InsertSite, String),
    /// Replace exactly the base bytes `[start, end)` with this text — the `old`-text form,
    /// whose match may start or end mid-line and include line terminators.
    Splice {
        start: usize,
        end: usize,
        new: String,
    },
}

impl ChangeAction {
    /// The verb the landing sentence uses.
    fn verb(&self) -> &'static str {
        match self {
            Self::Replace(content) if content.is_empty() => "deleted",
            Self::Replace(_) => "replaced",
            Self::Delete => "deleted",
            Self::Insert(..) => "inserted",
            Self::Splice { new, .. } if new.is_empty() => "deleted",
            Self::Splice { .. } => "replaced",
        }
    }
}

/// How the reply names one change's address.
#[derive(Debug)]
enum ChangeAddress {
    /// `lines 12-20` (base-version numbers).
    Lines(LineRange),
    /// `src/x.rs#Foo/bar`.
    Symbol(String),
    /// `src/x.rs#Foo/bar` inserted relative to an anchor named by its last path segment.
    Insert {
        /// The inserted symbol's address as requested.
        path: String,
        /// The anchor's last path segment (`Foo` in `src/x.rs#Foo`).
        anchor: String,
        /// `before` / `after` / `first in` / `last in`.
        where_: String,
    },
    /// `old text at line 88`.
    OldText { line: u32 },
}

/// One change resolved against the base bytes: its base span, how to apply it and how to name it.
#[derive(Debug)]
struct ResolvedChange {
    /// 1-based change number in the request.
    number: usize,
    /// Base span the address resolved to. For inserts this is the zero-width anchor
    /// `[site.line, site.line]`, ordered by `site.line`.
    base: LineRange,
    action: ChangeAction,
    address: ChangeAddress,
}

impl ResolvedChange {
    /// Whether this change inserts (a zero-width span that no other span may strictly contain).
    fn inserts(&self) -> bool {
        matches!(self.action, ChangeAction::Insert(..))
    }

    /// One change's landing sentence: `change 2: src/x.rs#Foo/bar inserted after #Foo (now 26–33)`.
    fn landing(&self, now: Option<LineRange>) -> String {
        let now = now
            .map(|range| format!(" (now {range})"))
            .unwrap_or_default();
        match &self.address {
            ChangeAddress::Lines(range) => format!(
                "change {}: lines {range} {}{now}",
                self.number,
                self.action.verb()
            ),
            ChangeAddress::Symbol(path) => {
                format!("change {}: {path} {}{now}", self.number, self.action.verb())
            }
            ChangeAddress::Insert {
                path,
                anchor,
                where_,
            } => format!(
                "change {}: {path} inserted {where_} #{anchor}{now}",
                self.number
            ),
            ChangeAddress::OldText { line } => format!(
                "change {}: old text at line {line} {}{now}",
                self.number,
                self.action.verb()
            ),
        }
    }
}

/// The per-change refusals of one batch that was not applied: each sentence names its change and
/// the exact fix; the reply states how many of how many were refused and that nothing was written.
#[derive(Debug)]
struct Refusal {
    /// Sentences in request order, joined with `; ` by [`Self::detail`].
    sentences: Vec<String>,
    /// The change numbers named, for the refused count.
    refused: std::collections::BTreeSet<usize>,
    /// Total requested changes.
    total: usize,
}

impl Refusal {
    /// Records one sentence for change `number`.
    fn push(&mut self, number: usize, sentence: String) {
        self.refused.insert(number);
        self.sentences.push(sentence);
    }

    /// The whole refusal detail after `edit:refused:` — `2 of 3 changes refused, nothing
    /// written — change 2: …; change 3: …`.
    fn detail(&self) -> String {
        format!(
            "{} of {} changes refused, nothing written — {}",
            self.refused.len(),
            self.total,
            self.sentences.join("; ")
        )
    }
}

/// One `changes` entry after the facade's shape validation, before resolution.
#[derive(Debug)]
enum ChangeRequest {
    /// Replace base lines with new content.
    Lines { range: LineRange, content: String },
    /// Replace, insert next to, or delete a symbol of the edit's file.
    Symbol {
        /// The address exactly as requested (the reply quotes it).
        requested: String,
        path: SymbolPath,
        op: &'static str,
        where_: Option<String>,
        content: Option<String>,
    },
    /// Replace exact text (matched once, optionally inside `within`'s symbol) with new text.
    Old {
        old: String,
        new: String,
        within: Option<String>,
    },
}

/// Parses the facade-validated `changes` entries; anything malformed here is an internal error,
/// because the facade already refused every other shape.
fn parse_change_requests(entries: &[Value]) -> Result<Vec<ChangeRequest>, FailureCode> {
    let mut requests = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(entry) = entry.as_object() else {
            return Err(FailureCode::Internal);
        };
        let text = |field: &str| {
            entry
                .get(field)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or(FailureCode::Internal)
        };
        let request = if let Some(lines) = entry.get("lines").and_then(Value::as_str) {
            ChangeRequest::Lines {
                range: crate::assistance::facade::parse_line_range(lines)
                    .ok_or(FailureCode::Internal)?,
                content: text("content")?,
            }
        } else if let Some(symbol) = entry.get("symbol").and_then(Value::as_str) {
            ChangeRequest::Symbol {
                requested: symbol.to_owned(),
                path: SymbolPath::parse(symbol).map_err(|_| FailureCode::Internal)?,
                op: match entry.get("op").and_then(Value::as_str) {
                    Some("insert") => "insert",
                    Some("delete") => "delete",
                    _ => "replace",
                },
                where_: entry
                    .get("where")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                content: entry
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }
        } else {
            ChangeRequest::Old {
                old: text("old")?,
                new: entry
                    .get("new")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                within: entry
                    .get("within")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }
        };
        requests.push(request);
    }
    Ok(requests)
}

/// One `old`-text match outcome inside its scope.
enum OldMatch {
    /// The unique match's byte span.
    One { start: usize, end: usize },
    /// No match; the closest n-line window and its first differing line, if anything is similar.
    NotFound { closest: Option<(LineRange, u32)> },
    /// Several matches; the first line of each.
    Many(Vec<u32>),
}

/// 1-based line number spans of `source` as `(start byte, end byte excluding the terminator)`.
fn line_byte_spans(source: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0;
    for line in source.split_inclusive('\n') {
        let end = start + line.len();
        spans.push((start, end - usize::from(line.ends_with('\n'))));
        start = end;
    }
    spans
}

/// The 1-based line `byte` sits on among `spans` (see [`line_byte_spans`]): a byte at a line's
/// end — its terminator, or the end of a final unterminated line — belongs to that line, and the
/// position past a terminated file belongs to its last line.
fn line_of_byte(spans: &[(usize, usize)], byte: usize) -> u32 {
    1 + spans
        .iter()
        .position(|&(start, end)| byte >= start && byte <= end)
        .unwrap_or(spans.len())
        .min(spans.len().saturating_sub(1)) as u32
}

/// Finds `old` in `source`, wholly inside `scope`'s lines when given: exactly one match, none
/// (with the closest similarly scored line window), or several (with each match's first line).
fn find_old(source: &str, old: &str, scope: Option<LineRange>) -> OldMatch {
    let spans = line_byte_spans(source);
    let total = spans.len() as u32;
    let (window_start, window_end) = match scope {
        Some(range) => (
            spans[(range.start.clamp(1, total.max(1)) - 1) as usize].0,
            spans[(range.end.clamp(1, total.max(1)) - 1) as usize].1,
        ),
        None => (0, source.len()),
    };
    let matches: Vec<(usize, usize)> = source[window_start..window_end]
        .match_indices(old)
        .map(|(offset, _)| (window_start + offset, window_start + offset + old.len()))
        .collect();
    if matches.len() > 1 {
        let lines = matches
            .iter()
            .map(|&(start, _)| {
                1 + spans
                    .iter()
                    .position(|&(line_start, line_end)| start >= line_start && start < line_end + 1)
                    .unwrap_or(0) as u32
            })
            .collect();
        return OldMatch::Many(lines);
    }
    if let Some(&(start, end)) = matches.first() {
        return OldMatch::One { start, end };
    }
    // Ignore edge blank lines in `old`; score windows by equal trimmed lines, then shared-prefix characters.
    let lines = source.lines().collect::<Vec<_>>();
    let scope = scope.unwrap_or(LineRange::new(1, total.max(1)));
    let mut old_lines = old.lines().map(str::trim).collect::<Vec<_>>();
    while old_lines.first() == Some(&"") {
        old_lines.remove(0);
    }
    while old_lines.last() == Some(&"") {
        old_lines.pop();
    }
    let count = old_lines.len().max(1);
    let start = (scope.start.max(1) as usize - 1).min(lines.len());
    let end = (scope.end as usize).min(lines.len());
    let Some(last_start) = end.checked_sub(count).filter(|last| start <= *last) else {
        return OldMatch::NotFound { closest: None };
    };
    let first = start..=last_start;
    let mut best: Option<(usize, usize, usize)> = None;
    for index in first {
        let window = &lines[index..index + count];
        let equal = window
            .iter()
            .zip(&old_lines)
            .filter(|(a, b)| a.trim() == b.trim())
            .count();
        let prefix = window
            .iter()
            .zip(&old_lines)
            .map(|(a, b)| {
                a.trim()
                    .chars()
                    .zip(b.trim().chars())
                    .take_while(|(x, y)| x == y)
                    .count()
            })
            .sum();
        if best
            .is_none_or(|(_, best_equal, best_prefix)| (equal, prefix) > (best_equal, best_prefix))
        {
            best = Some((index, equal, prefix));
        }
    }
    let Some((index, _equal, _prefix)) =
        best.filter(|(_, equal, prefix)| *equal > 0 || *prefix > 0)
    else {
        return OldMatch::NotFound { closest: None };
    };
    let difference = (0..count)
        .find(|offset| lines[index + offset].trim() != old_lines[*offset])
        .unwrap_or(0);
    OldMatch::NotFound {
        closest: Some((
            LineRange::new(index as u32 + 1, (index + count) as u32),
            (index + difference + 1) as u32,
        )),
    }
}

/// Resolves every change against the base bytes and the base outline: a symbol an earlier change
/// would create is simply not in the base outline, so a later change cannot address it. All
/// refusals are collected so one reply names every failed change. `outline` is `None` only when
/// no request addresses a symbol, so no arm below ever consults it then.
fn resolve_changes(
    source: &str,
    outline: Option<&Outline>,
    path: &str,
    requests: &[ChangeRequest],
) -> Result<Vec<ResolvedChange>, Refusal> {
    let spans = line_byte_spans(source);
    let line_of = |byte: usize| line_of_byte(&spans, byte);
    let mut refusal = Refusal {
        sentences: Vec::new(),
        refused: std::collections::BTreeSet::new(),
        total: requests.len(),
    };
    let mut resolved = Vec::with_capacity(requests.len());
    for (index, request) in requests.iter().enumerate() {
        let number = index + 1;
        let no_symbol = |requested: &str, refusal: &mut Refusal| {
            let file = requested.split_once('#').map_or(path, |(file, _)| file);
            refusal.push(
                number,
                format!(
                    "change {number}: no symbol {requested}; check ide.outline {{\"path\":\"{file}\"}}"
                ),
            );
        };
        match request {
            ChangeRequest::Lines { range, content } => {
                resolved.push(ResolvedChange {
                    number,
                    base: *range,
                    action: ChangeAction::Replace(content.clone()),
                    address: ChangeAddress::Lines(*range),
                });
            }
            ChangeRequest::Symbol {
                requested,
                path: symbol,
                op,
                where_,
                content,
            } => {
                if symbol.file().is_some_and(|file| file != Path::new(path)) {
                    no_symbol(requested, &mut refusal);
                    continue;
                }
                if *op == "insert" {
                    let outline = outline.expect("an insert entry resolves against the outline");
                    let support = outline.language.support();
                    let where_ = match where_.as_deref() {
                        Some("before") => lang::InsertWhere::Before,
                        Some("after") => lang::InsertWhere::After,
                        Some("first") => lang::InsertWhere::First,
                        Some("last") => lang::InsertWhere::Last,
                        _ => {
                            refusal.push(
                                number,
                                format!("change {number}: {requested} needs \"where\""),
                            );
                            continue;
                        }
                    };
                    match support.insert_site(source, outline, symbol, where_) {
                        Ok(site) => {
                            let anchor = symbol
                                .segments()
                                .last()
                                .cloned()
                                .unwrap_or_default();
                            resolved.push(ResolvedChange {
                                number,
                                base: LineRange::new(site.line, site.line),
                                action: ChangeAction::Insert(site, content.clone().unwrap_or_default()),
                                address: ChangeAddress::Insert {
                                    path: requested.clone(),
                                    anchor,
                                    where_: match where_ {
                                        lang::InsertWhere::First => "first in".to_owned(),
                                        lang::InsertWhere::Last => "last in".to_owned(),
                                        lang::InsertWhere::Before => "before".to_owned(),
                                        lang::InsertWhere::After => "after".to_owned(),
                                    },
                                },
                            });
                        }
                        Err(lang::LangError::UnknownSymbol(_)) => no_symbol(requested, &mut refusal),
                        Err(lang::LangError::NotAContainer(_)) => refusal.push(
                            number,
                            format!(
                                "change {number}: {requested} is not a container for \"first\"/\"last\""
                            ),
                        ),
                        Err(_) => refusal.push(
                            number,
                            format!("change {number}: {requested} cannot place an insert here"),
                        ),
                    }
                } else {
                    let Some(found) = outline
                        .expect("a symbol entry resolves against the outline")
                        .find(symbol)
                    else {
                        no_symbol(requested, &mut refusal);
                        continue;
                    };
                    resolved.push(ResolvedChange {
                        number,
                        base: found.range,
                        action: if *op == "delete" {
                            ChangeAction::Delete
                        } else {
                            ChangeAction::Replace(content.clone().unwrap_or_default())
                        },
                        address: ChangeAddress::Symbol(requested.clone()),
                    });
                }
            }
            ChangeRequest::Old { old, new, within } => {
                let scope = match within {
                    Some(within) => match SymbolPath::parse(within)
                        .ok()
                        .filter(|symbol| symbol.file().is_none_or(|file| file == Path::new(path)))
                        .and_then(|symbol| {
                            outline
                                .expect("a `within` address resolves against the outline")
                                .find(&symbol)
                        }) {
                        Some(found) => Some(found.range),
                        None => {
                            refusal.push(
                                number,
                                format!(
                                    "change {number}: no symbol {within}; check ide.outline \
                                     {{\"path\":\"{path}\"}}"
                                ),
                            );
                            continue;
                        }
                    },
                    None => None,
                };
                match find_old(source, old, scope) {
                    OldMatch::One { start, end } => {
                        // The match replaces exactly its own bytes: it may start and end
                        // mid-line and include line terminators, so it applies as a byte
                        // splice; the base line span is what the match's first and last byte
                        // touch, and the landing lines are computed from the byte positions.
                        let first = line_of(start);
                        let last = line_of(end.saturating_sub(1));
                        resolved.push(ResolvedChange {
                            number,
                            base: LineRange::new(first, last),
                            action: ChangeAction::Splice {
                                start,
                                end,
                                new: new.clone(),
                            },
                            address: ChangeAddress::OldText { line: first },
                        });
                    }
                    OldMatch::Many(lines) => refusal.push(
                        number,
                        format!(
                            "change {number}: old text matches {} places (lines {}); add \
                             \"within\":\"<symbol>\" or more surrounding lines",
                            lines.len(),
                            lines
                                .iter()
                                .map(u32::to_string)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    ),
                    OldMatch::NotFound { closest } => refusal.push(
                        number,
                        match closest {
                            Some((range, difference)) => format!(
                                "change {number}: old text not found; closest lines {}-{}; first difference at line {difference}: \"{}\"",
                                range.start,
                                range.end,
                                clip_bytes(
                                    lang::slice_lines(source, LineRange::new(difference, difference))
                                        .trim(),
                                    MAX_REFUSAL_EXCERPT_BYTES
                                )
                            ),
                            None => format!("change {number}: old text not found; no similar text"),
                        },
                    ),
                }
            }
        }
    }
    if refusal.refused.is_empty() {
        Ok(resolved)
    } else {
        Err(refusal)
    }
}

/// Clips one excerpt for a refusal sentence at a byte boundary, appending `…` when it was cut.
fn clip_bytes(text: &str, limit: usize) -> String {
    let mut clipped = crate::assistance::reply::bounded_utf8_prefix(text, limit);
    if clipped.len() < text.len() {
        clipped.push('…');
    }
    clipped
}

/// Which form one batch read takes: several symbol addresses, or several ranges of one file.
#[derive(Clone, Copy, PartialEq)]
enum ReadBatch {
    /// `ide.read {symbols: […]}` — cross-file symbol bodies, and bare-path whole-file text of
    /// files no IDE language reads.
    Symbols,
    /// `ide.read {path, ranges: […]}` — several ranges of the one file.
    Ranges,
}

/// The batch-read duplicates footer: every duplicated address named once, eight inline then
/// `+N more` like the cut footer. The `+N more` form needs more than eight duplicated addresses,
/// which one call's 16-entry bound cannot produce today — the footer stays honest if that bound
/// ever rises.
fn duplicates_footer(duplicates: &[String]) -> String {
    let shown = duplicates
        .iter()
        .take(MAX_LANDING_SENTENCES)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let hidden = duplicates.len().saturating_sub(MAX_LANDING_SENTENCES);
    let mut footer = format!("duplicates shown once: {shown}");
    if hidden > 0 {
        footer.push_str(&format!("; +{hidden} more"));
    }
    footer.push('\n');
    footer
}

/// Bytes of one quoted excerpt inside a refusal sentence.
const MAX_REFUSAL_EXCERPT_BYTES: usize = 80;
/// Change sentences a success reply lists inline before `+N more`.
const MAX_LANDING_SENTENCES: usize = 8;
/// Bytes the whole refusal detail is clipped to, one line.
const MAX_REFUSAL_DETAIL_BYTES: usize = 512;

/// How a syntax error relates to the change it is attributed to.
enum Attribution {
    /// The error line is inside the change's final range.
    Within(usize),
    /// The error line sits just after the change's final range — a parser reports an
    /// unterminated construct a line or two after the edit that opened it.
    JustAfter(usize, LineRange),
}

/// The change a syntax error at `line` is attributed to: the change whose final range contains
/// `line`, else the nearest change that ended before it — which is the only change when there
/// is one — so an unterminated construct opened by an edit is named for that edit. Ranges are
/// post-splice; a formatter that moved lines can shift the attribution, which is cosmetic — the
/// excerpt disambiguates.
fn syntax_change(
    changes: &[ResolvedChange],
    landings: &[Option<LineRange>],
    line: u32,
) -> Option<Attribution> {
    let landed: Vec<(usize, LineRange)> = changes
        .iter()
        .zip(landings)
        .filter_map(|(change, now)| now.map(|range| (change.number, range)))
        .collect();
    if let Some(&(number, _)) = landed
        .iter()
        .find(|(_, range)| range.start <= line && line <= range.end)
    {
        return Some(Attribution::Within(number));
    }
    let nearest = landed
        .iter()
        .filter(|(_, range)| range.end < line)
        .max_by_key(|(_, range)| (range.end, range.start));
    match (nearest, landed.len() == 1) {
        (Some(&(number, range)), _) => Some(Attribution::JustAfter(number, range)),
        (None, true) => {
            let (number, range) = landed[0];
            Some(Attribution::JustAfter(number, range))
        }
        (None, false) => None,
    }
}

/// The syntax refusal sentence for a candidate that does not parse: the change that produced the
/// error (or `outside the edited ranges`), the checker's message, and a few candidate lines
/// around the error.
fn syntax_sentence(
    candidate: &str,
    changes: &[ResolvedChange],
    landings: &[Option<LineRange>],
    line: u32,
    message: &str,
) -> String {
    let lines = candidate.lines().collect::<Vec<_>>();
    let from = line.saturating_sub(2).max(1);
    let to = (line + 2).min(lines.len() as u32);
    let excerpt = lines
        .get((from - 1) as usize..to as usize)
        .map(|lines| lines.join("\\n"))
        .unwrap_or_default();
    let (who, where_) = match syntax_change(changes, landings, line) {
        Some(Attribution::Within(number)) => (format!("change {number} produced"), String::new()),
        Some(Attribution::JustAfter(number, range)) => {
            let lines = if range.start == range.end {
                format!("its line {}", range.start)
            } else {
                format!("its lines {range}")
            };
            (
                format!("change {number} produced"),
                format!(" (just after {lines})"),
            )
        }
        None => (
            "the candidate produced".to_owned(),
            " outside the edited ranges".to_owned(),
        ),
    };
    format!(
        "{who} a syntax error at line {line}{where_}: \"{}\" (candidate lines {from}-{to}: \
         \"{}\"); candidate not written",
        clip_bytes(message, MAX_REFUSAL_EXCERPT_BYTES),
        clip_bytes(&excerpt, MAX_REFUSAL_EXCERPT_BYTES)
    )
}

/// Applies resolved changes and returns the candidate with each change's exact final line range
/// (post-splice, pre-format). Application is bottom-up — descending base start; at equal starts
/// a span change applies before an insert anchored at that line (so the inserted text ends up
/// immediately before the span's result), and two inserts at one anchor apply in reverse array
/// order so they keep their array order in the file — so every change's base numbers are still
/// valid when it applies; its landing span is recorded where the application left it and then
/// shifted by every later application above it, so the reported range is where the change's
/// text sits in the final file. Overlapping spans, an insert strictly inside another change's
/// span, and a range past the end of the file are refused with nothing applied.
fn apply_changes(
    source: &str,
    path: &str,
    changes: &[ResolvedChange],
) -> Result<(String, Vec<Option<LineRange>>), Refusal> {
    let total = lang::line_count(source);
    let mut refusal = Refusal {
        sentences: Vec::new(),
        refused: std::collections::BTreeSet::new(),
        total: changes.len(),
    };
    for change in changes {
        if !change.inserts() && change.base.start > total {
            refusal.push(
                change.number,
                format!(
                    "change {}: lines {} is past the end of {path} ({total} lines); re-read the file",
                    change.number, change.base
                ),
            );
        }
    }
    if !refusal.refused.is_empty() {
        return Err(refusal);
    }
    for (earlier, other) in changes.iter().enumerate() {
        for change in &changes[earlier + 1..] {
            let clash = match (&other.action, &change.action) {
                // Exact-text changes clash only when their matched bytes intersect: separate
                // matches on one line (two substrings of one string literal) apply independently.
                (
                    ChangeAction::Splice { start, end, .. },
                    ChangeAction::Splice {
                        start: other_start,
                        end: other_end,
                        ..
                    },
                ) => start < other_end && other_start < end,
                _ => match (other.inserts(), change.inserts()) {
                    (true, true) => false,
                    (true, false) => {
                        change.base.start < other.base.start && other.base.start <= change.base.end
                    }
                    (false, true) => {
                        other.base.start < change.base.start && change.base.start <= other.base.end
                    }
                    (false, false) => {
                        other.base.start <= change.base.end && change.base.start <= other.base.end
                    }
                },
            };
            if clash {
                refusal.refused.insert(other.number);
                refusal.refused.insert(change.number);
                refusal.sentences.push(format!(
                    "changes {} and {} overlap (base lines {} and {}); merge them or narrow the \
                     ranges",
                    other.number, change.number, other.base, change.base
                ));
            }
        }
    }
    if !refusal.refused.is_empty() {
        return Err(refusal);
    }
    let mut order: Vec<usize> = (0..changes.len()).collect();
    order.sort_by(|&a, &b| {
        changes[b]
            .base
            .start
            .cmp(&changes[a].base.start)
            // Exact-text changes sharing a line apply right to left, so each one's base byte
            // offsets still address the buffer when it applies.
            .then_with(|| match (&changes[a].action, &changes[b].action) {
                (ChangeAction::Splice { start: a, .. }, ChangeAction::Splice { start: b, .. }) => {
                    b.cmp(a)
                }
                _ => std::cmp::Ordering::Equal,
            })
            // At one start line the span change applies first and an insert anchored there
            // applies after it, so the inserted text ends up immediately before the span's
            // result whichever order the array listed them in; two inserts at one anchor keep
            // their array order (the later array entry applies first, above the other).
            .then(changes[a].inserts().cmp(&changes[b].inserts()))
            .then(b.cmp(&a))
    });
    let mut buffer = source.to_owned();
    // Each change's span is recorded where its application left it, then shifted by every later
    // application above it: application runs bottom-up, so a change that changes the line count
    // moves the already-applied changes below it, and the reply must report where each change's
    // text finally sits, not where it sat when it applied.
    let mut recorded: Vec<(usize, Option<LineRange>)> = Vec::with_capacity(changes.len());
    for &index in &order {
        let change = &changes[index];
        let before = lang::line_count(&buffer);
        let applied = match &change.action {
            ChangeAction::Replace(content) => {
                buffer = splice_lines(&buffer, change.base, content);
                (!content.is_empty()).then(|| {
                    let produced = lang::line_count(content);
                    LineRange::new(change.base.start, change.base.start + produced - 1)
                })
            }
            ChangeAction::Delete => {
                buffer = delete_symbol_lines(&buffer, change.base);
                None
            }
            ChangeAction::Insert(site, content) => {
                let (blank_before, _) = insert_gaps(&buffer, site);
                buffer = insert_lines(&buffer, site, content);
                let block = indent_block(content, &site.indent);
                let start = change.base.start + u32::try_from(blank_before).unwrap_or(0);
                Some(LineRange::new(start, start + lang::line_count(&block) - 1))
            }
            ChangeAction::Splice { start, end, new } => {
                // Applications above this span only touch bytes below it, so the base byte
                // offsets still address the buffer; the clamp covers the one same-batch case
                // where a symbol delete's blank merge has consumed the span's tail bytes.
                let length = buffer.len();
                let (start, end) = ((*start).min(length), (*end).min(length));
                buffer.replace_range(start..end, new);
                (!new.is_empty()).then(|| {
                    let spans = line_byte_spans(&buffer);
                    LineRange::new(
                        line_of_byte(&spans, start),
                        line_of_byte(&spans, (start + new.len()).saturating_sub(1)),
                    )
                })
            }
        };
        let shift = i64::from(lang::line_count(&buffer)) - i64::from(before);
        for (applied_index, span) in &mut recorded {
            // An exact-text change applied earlier further right on this change's line moves
            // with this change's line count, too.
            let right_of_this = matches!(
                (&change.action, &changes[*applied_index].action),
                (ChangeAction::Splice { start, .. }, ChangeAction::Splice { start: applied, .. })
                    if applied > start
            );
            // An insert lands before its anchor line, so a span at that line moves too.
            let moves = match (&change.action, span.as_ref()) {
                (ChangeAction::Insert(..), Some(span)) => span.start >= change.base.start,
                (_, Some(span)) if right_of_this => span.start >= change.base.start,
                (_, Some(span)) => span.start > change.base.end,
                (_, None) => false,
            };
            if moves && let Some(span) = span {
                *span = LineRange::new(
                    (i64::from(span.start) + shift).max(1) as u32,
                    (i64::from(span.end) + shift).max(1) as u32,
                );
            }
        }
        recorded.push((index, applied));
    }
    recorded.sort_by_key(|(index, _)| *index);
    let landings = recorded.into_iter().map(|(_, span)| span).collect();
    Ok((buffer, landings))
}

/// The success reply's per-change landing summary, `+N more` past the first
/// [`MAX_LANDING_SENTENCES`] entries, each range remapped through the formatter's line movement
/// when the formatter moved lines.
fn landing_note(
    changes: &[ResolvedChange],
    landings: &[Option<LineRange>],
    align: Option<&lang::text::LineAlign>,
) -> String {
    let rendered = changes
        .iter()
        .zip(landings)
        .map(|(change, now)| {
            change.landing(now.map(|range| align.map_or(range, |align| align.map_range(range))))
        })
        .take(MAX_LANDING_SENTENCES)
        .collect::<Vec<_>>()
        .join("; ");
    let count = changes.len();
    let noun = if count == 1 { "change" } else { "changes" };
    let mut note = format!("{count} {noun} applied: {rendered}");
    let hidden = changes.len().saturating_sub(MAX_LANDING_SENTENCES);
    if hidden > 0 {
        note.push_str(&format!("; +{hidden} more"));
    }
    note
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
mod batch_tests {
    use super::*;

    /// The gamma outline of `text`, so symbol addresses resolve like a real language's would.
    fn gamma_outline(text: &str) -> Outline {
        crate::lang::testing::install();
        crate::lang::testing::GAMMA
            .support()
            .outline_from_source(Path::new("a.gamma"), text)
            .expect("gamma outlines from source")
    }

    /// A resolved change by hand, for the overlap matrix (gamma cannot place inserts).
    fn change(number: usize, base: LineRange, action: ChangeAction) -> ResolvedChange {
        ResolvedChange {
            number,
            base,
            action,
            address: ChangeAddress::Lines(base),
        }
    }

    /// A zero-width insert change anchored at `line`.
    fn insert_at(number: usize, line: u32, content: &str) -> ResolvedChange {
        ResolvedChange {
            number,
            base: LineRange::new(line, line),
            action: ChangeAction::Insert(
                lang::InsertSite {
                    line,
                    indent: String::new(),
                    blank_before: 0,
                    blank_after: 0,
                },
                content.to_owned(),
            ),
            address: ChangeAddress::Lines(LineRange::new(line, line)),
        }
    }

    /// Lines, symbols and exact text in one batch: base coordinates, bottom-up application and
    /// the exact `now` ranges.
    #[test]
    fn mixed_batch_applies_bottom_up_with_exact_landings() {
        let source = "sym card\n1\nsym btn\n2\nend\nmark\nend\n";
        let outline = gamma_outline(source);
        let requests = [
            ChangeRequest::Lines {
                range: LineRange::new(1, 2),
                content: "sym card\n7\n8".to_owned(),
            },
            ChangeRequest::Symbol {
                requested: "a.gamma#card/btn".to_owned(),
                path: SymbolPath::parse("a.gamma#card/btn").unwrap(),
                op: "replace",
                where_: None,
                content: Some("sym btn\nthree\nend".to_owned()),
            },
            ChangeRequest::Old {
                old: "mark".to_owned(),
                new: "mark,two".to_owned(),
                within: None,
            },
        ];
        let changes =
            resolve_changes(source, Some(&outline), "a.gamma", &requests).expect("all resolve");
        let (candidate, now) = apply_changes(source, "a.gamma", &changes).expect("no overlaps");
        assert_eq!(
            candidate,
            "sym card\n7\n8\nsym btn\nthree\nend\nmark,two\nend\n"
        );
        // The lines change grew by one line, so everything below it lands one line lower than
        // its base numbers: the reply reports where each change's text sits in the final file.
        assert_eq!(now[0], Some(LineRange::new(1, 3)), "lines 1-2 became 1-3");
        assert_eq!(now[1], Some(LineRange::new(4, 6)), "btn spans 4-6");
        assert_eq!(
            now[2],
            Some(LineRange::new(7, 7)),
            "mark,two stays one line, shifted down one"
        );
    }

    /// Two inserts at one anchor apply in array order; an insert at a span's boundary is fine,
    /// strictly inside is an overlap, and touching spans never clash.
    #[test]
    fn overlap_matrix_accepts_touching_and_same_anchor_inserts() {
        let source = "a\nb\nc\nd\ne\nf\n";
        // Touching replaces are fine and land exactly.
        let touching = vec![
            change(
                1,
                LineRange::new(1, 2),
                ChangeAction::Replace("X".to_owned()),
            ),
            change(
                2,
                LineRange::new(3, 4),
                ChangeAction::Replace("Y".to_owned()),
            ),
        ];
        let (candidate, _) = apply_changes(source, "a.gamma", &touching).unwrap();
        assert_eq!(candidate, "X\nY\ne\nf\n");
        // Intersecting replaces are refused, naming both changes and both base spans.
        let error = apply_changes(
            source,
            "a.gamma",
            &[
                change(
                    1,
                    LineRange::new(1, 3),
                    ChangeAction::Replace("X".to_owned()),
                ),
                change(
                    2,
                    LineRange::new(3, 5),
                    ChangeAction::Replace("Y".to_owned()),
                ),
            ],
        )
        .unwrap_err();
        assert_eq!(
            error.detail(),
            "2 of 2 changes refused, nothing written — changes 1 and 2 overlap (base lines 1–3 and 3–5); merge them or narrow the ranges"
        );
        let error = apply_changes(
            source,
            "a.gamma",
            &[
                change(
                    1,
                    LineRange::new(1, 3),
                    ChangeAction::Replace("X".to_owned()),
                ),
                change(
                    2,
                    LineRange::new(3, 5),
                    ChangeAction::Replace("Y".to_owned()),
                ),
                change(
                    3,
                    LineRange::new(6, 6),
                    ChangeAction::Replace("Z".to_owned()),
                ),
            ],
        )
        .unwrap_err();
        assert_eq!(error.refused, [1, 2].into());
        assert!(
            error
                .detail()
                .starts_with("2 of 3 changes refused, nothing written")
        );
        // An insert strictly inside another span is refused; at either boundary it applies —
        // after the span change when they share its first line, before the span's result.
        for (line, allowed) in [(3, false), (2, true), (5, true)] {
            let result = apply_changes(
                source,
                "a.gamma",
                &[
                    change(
                        1,
                        LineRange::new(2, 4),
                        ChangeAction::Replace("X".to_owned()),
                    ),
                    insert_at(2, line, "i"),
                ],
            );
            assert_eq!(result.is_ok(), allowed, "insert at {line}");
            if allowed {
                let (candidate, now) = result.unwrap();
                let (expected, insert_now) = if line == 2 {
                    ("a\ni\nX\ne\nf\n", LineRange::new(2, 2))
                } else {
                    ("a\nX\ni\ne\nf\n", LineRange::new(3, 3))
                };
                assert_eq!(candidate, expected, "insert at {line}");
                assert_eq!(now[1], Some(insert_now), "insert at {line}");
            }
        }
        // Two inserts at one anchor keep their array order in the file, and each reports where
        // its own block landed: the earlier array entry applies last, above the other.
        let (candidate, now) = apply_changes(
            source,
            "a.gamma",
            &[insert_at(1, 3, "first"), insert_at(2, 3, "second")],
        )
        .unwrap();
        assert_eq!(candidate, "a\nb\nfirst\nsecond\nc\nd\ne\nf\n");
        assert_eq!(now[0], Some(LineRange::new(3, 3)), "first lands at 3");
        assert_eq!(
            now[1],
            Some(LineRange::new(4, 4)),
            "second lands below the later insert"
        );
        // A range past the end is refused with the unified grammar.
        assert_eq!(
            apply_changes(
                "a\nb\n",
                "a.gamma",
                &[change(
                    1,
                    LineRange::new(9, 9),
                    ChangeAction::Replace("X".to_owned())
                )]
            )
            .unwrap_err()
            .detail(),
            "1 of 1 changes refused, nothing written — change 1: lines 9 is past the end of a.gamma (2 lines); re-read the file"
        );
    }

    /// `old` matches exactly once in the file or inside `within`, suggests the closest line when
    /// absent, and names every match's line when ambiguous.
    #[test]
    fn old_text_matches_once_inside_a_scope_or_refuses() {
        let source = "sym card\n1\nsym btn\nmark\nend\nmark\nend\n";
        let outline = gamma_outline(source);
        let resolve = |old: &str, within: Option<&str>| {
            let requests = [ChangeRequest::Old {
                old: old.to_owned(),
                new: "stain".to_owned(),
                within: within.map(str::to_owned),
            }];
            resolve_changes(source, Some(&outline), "a.gamma", &requests)
        };
        // Ambiguous in the file, unique inside the card's btn symbol.
        match resolve("mark", None) {
            Err(refusal) => assert!(
                refusal.detail().contains(
                    "change 1: old text matches 2 places (lines 4, 6); add \
                     \"within\":\"<symbol>\" or more surrounding lines"
                ),
                "{}",
                refusal.detail()
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
        let changes = resolve("mark", Some("a.gamma#card/btn")).expect("scoped match");
        let (candidate, landings) = apply_changes(source, "a.gamma", &changes).expect("applies");
        assert_eq!(candidate, "sym card\n1\nsym btn\nstain\nend\nmark\nend\n");
        assert_eq!(landings[0], Some(LineRange::new(4, 4)));
        // Not found points to the closest aligned line window.
        match resolve("martians", None) {
            Err(refusal) => assert!(
                refusal.detail().contains(
                    "change 1: old text not found; closest lines 4-4; first difference at line 4"
                ),
                "{}",
                refusal.detail()
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
        // A mid-line match keeps the text around it on its own line.
        let requests = [ChangeRequest::Old {
            old: "ym btn".to_owned(),
            new: "YM BTN".to_owned(),
            within: None,
        }];
        let changes = resolve_changes(source, Some(&outline), "a.gamma", &requests).unwrap();
        let (candidate, _) = apply_changes(source, "a.gamma", &changes).unwrap();
        assert!(
            candidate.starts_with("sym card\n1\nsYM BTN\n"),
            "{candidate}"
        );
    }

    /// Suggests the closest line window when old text differs only in indentation and value.
    #[test]
    fn closest_old_line_ignores_leading_indentation() {
        let source = "unrelated\n    let value = 1;\n";
        let request = [ChangeRequest::Old {
            old: "        let value = 2;".to_owned(),
            new: "".to_owned(),
            within: None,
        }];
        let refusal = resolve_changes(source, None, "a.rs", &request).unwrap_err();
        assert!(
            refusal
                .detail()
                .contains("closest lines 2-2; first difference at line 2")
        );
    }

    /// Scores unmatched old-text windows by equal trimmed lines, then shared prefixes.
    #[test]
    fn unmatched_old_text_reports_best_window_and_no_similarity() {
        let source = "start\n    }\nmid\n    }\n    tail(1);\n";
        assert!(matches!(
            find_old(source, "    }\n    tail(2);", None),
            OldMatch::NotFound { closest: Some((range, 5)) } if range == LineRange::new(4, 5)
        ));
        assert!(matches!(
            find_old(source, "\n    tail(2);", None),
            OldMatch::NotFound { closest: Some((range, 5)) } if range == LineRange::new(5, 5)
        ));
        assert!(matches!(
            find_old(source, "unrelated", None),
            OldMatch::NotFound { closest: None }
        ));
    }

    /// A change addressing a symbol an earlier change inserts is refused: it is simply not in
    /// the base outline.
    #[test]
    fn a_symbol_created_by_a_change_is_not_addressable() {
        let source = "sym card\n1\nend\n";
        let outline = gamma_outline(source);
        let requests = [
            ChangeRequest::Symbol {
                requested: "a.gamma#card".to_owned(),
                path: SymbolPath::parse("a.gamma#card").unwrap(),
                op: "replace",
                where_: None,
                content: Some("sym card\n2\nend".to_owned()),
            },
            ChangeRequest::Symbol {
                requested: "a.gamma#card/made".to_owned(),
                path: SymbolPath::parse("a.gamma#card/made").unwrap(),
                op: "delete",
                where_: None,
                content: None,
            },
        ];
        match resolve_changes(source, Some(&outline), "a.gamma", &requests) {
            Err(refusal) => assert!(
                refusal.detail().contains(
                    "change 2: no symbol a.gamma#card/made; check ide.outline \
                     {\"path\":\"a.gamma\"}"
                ),
                "{}",
                refusal.detail()
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// The landing note lists the first eight changes and counts the rest.
    #[test]
    fn landing_note_clips_after_eight_changes() {
        let changes: Vec<ResolvedChange> = (1..=10)
            .map(|number| {
                change(
                    number,
                    LineRange::new(number as u32, number as u32),
                    ChangeAction::Replace("x".to_owned()),
                )
            })
            .collect();
        let landings = changes
            .iter()
            .map(|change| Some(change.base))
            .collect::<Vec<_>>();
        let note = landing_note(&changes, &landings, None);
        assert!(
            note.starts_with("10 changes applied: change 1: lines 1 replaced (now 1)"),
            "{note}"
        );
        assert!(note.ends_with("; +2 more"), "{note}");
        assert!(!note.contains("change 9:"), "{note}");
    }

    /// The single form's one change produces byte-identical candidates for every legacy action.
    #[test]
    fn single_change_matches_the_legacy_splices() {
        let source = "a\nb\nc\nd\n";
        let site = lang::InsertSite {
            line: 3,
            indent: "  ".into(),
            blank_before: 0,
            blank_after: 0,
        };
        let replace = single_change(
            "replace",
            &Splice::Replace(LineRange::new(2, 3)),
            Some("X\nY"),
        )
        .unwrap();
        let (candidate, landing) =
            apply_changes(source, "a.rs", std::slice::from_ref(&replace)).unwrap();
        assert_eq!(
            candidate,
            splice_lines(source, LineRange::new(2, 3), "X\nY")
        );
        assert_eq!(landing[0], Some(LineRange::new(2, 3)));
        let delete = single_change("delete", &Splice::Replace(LineRange::new(2, 3)), None).unwrap();
        let (candidate, landing) =
            apply_changes(source, "a.rs", std::slice::from_ref(&delete)).unwrap();
        assert_eq!(candidate, delete_symbol_lines(source, LineRange::new(2, 3)));
        assert_eq!(landing[0], None);
        let insert = single_change("insert", &Splice::Insert(site.clone()), Some("i")).unwrap();
        let (candidate, _) = apply_changes(source, "a.rs", std::slice::from_ref(&insert)).unwrap();
        assert_eq!(candidate, insert_lines(source, &site, "i"));
    }

    /// `old` replaces exactly its matched bytes, terminators included: `b\n` deleted leaves the
    /// following line, `b\n` → `B\n` stays its own line, and a match that is only the terminator
    /// joins the lines around it.
    #[test]
    fn old_text_replaces_its_exact_bytes_including_terminators() {
        let source = "a\nb\nc\n";
        let outline = gamma_outline(source);
        let splice = |old: &str, new: &str| {
            let requests = [ChangeRequest::Old {
                old: old.to_owned(),
                new: new.to_owned(),
                within: None,
            }];
            let changes = resolve_changes(source, Some(&outline), "a.gamma", &requests).unwrap();
            apply_changes(source, "a.gamma", &changes)
        };
        let (candidate, landing) = splice("b\n", "").unwrap();
        assert_eq!(candidate, "a\nc\n");
        assert_eq!(landing[0], None, "a deleted match reports no range");
        let (candidate, landing) = splice("b\n", "B\n").unwrap();
        assert_eq!(candidate, "a\nB\nc\n");
        assert_eq!(landing[0], Some(LineRange::new(2, 2)));
        let (candidate, _) = splice("b\n", "B").unwrap();
        assert_eq!(
            candidate, "a\nBc\n",
            "the new text joins the following line"
        );
        let source = "ab\ncd";
        let outline = gamma_outline(source);
        let requests = [ChangeRequest::Old {
            old: "\n".to_owned(),
            new: "".to_owned(),
            within: None,
        }];
        let changes = resolve_changes(source, Some(&outline), "a.gamma", &requests).unwrap();
        let (candidate, _) = apply_changes(source, "a.gamma", &changes).unwrap();
        assert_eq!(candidate, "abcd");
    }

    /// Exact-text changes on one line clash only when their matched bytes intersect: separate
    /// substrings of one line apply together (right to left, each landing where its text ends up),
    /// while intersecting matches are refused together with nothing applied.
    #[test]
    fn old_texts_on_one_line_clash_only_when_their_bytes_intersect() {
        let source = "a\nlet s = \"before-one / before-two\";\nc\n";
        let outline = gamma_outline(source);
        let old = |old: &str, new: &str| ChangeRequest::Old {
            old: old.to_owned(),
            new: new.to_owned(),
            within: None,
        };
        let requests = [
            old("before-one", "after-one\nwrapped"),
            old("before-two", "after-two"),
        ];
        let changes = resolve_changes(source, Some(&outline), "a.gamma", &requests).unwrap();
        let (candidate, landing) = apply_changes(source, "a.gamma", &changes).unwrap();
        assert_eq!(
            candidate,
            "a\nlet s = \"after-one\nwrapped / after-two\";\nc\n"
        );
        assert_eq!(
            landing,
            vec![Some(LineRange::new(2, 3)), Some(LineRange::new(3, 3))]
        );
        let overlapping = [old("before-one / ", "x"), old("one / before", "y")];
        let changes = resolve_changes(source, Some(&outline), "a.gamma", &overlapping).unwrap();
        let Err(refusal) = apply_changes(source, "a.gamma", &changes) else {
            panic!("intersecting matches are refused");
        };
        assert_eq!(refusal.refused.len(), 2);
    }

    /// Every substring of a multi-line text — with and without a trailing newline, LF and CRLF —
    /// either refuses (no unique match) or replaces exactly its own bytes: no panic anywhere,
    /// byte-exact results for any `old` and any of a terminator-free, terminated and empty `new`.
    #[test]
    fn old_text_never_panics_and_is_byte_exact_for_every_substring() {
        let resolve_and_apply = |source: &str, old: &str, new: &str| {
            let outline = gamma_outline(source);
            let requests = [ChangeRequest::Old {
                old: old.to_owned(),
                new: new.to_owned(),
                within: None,
            }];
            match resolve_changes(source, Some(&outline), "a.gamma", &requests) {
                Ok(changes) => apply_changes(source, "a.gamma", &changes).map(|(text, _)| text),
                Err(_) => Ok("__refused__".to_owned()),
            }
        };
        for source in [
            "fn one() {\n    1\n}\n\nfn two() {\n    2\n}\n",
            "fn one() {\n    1\n}\n\nfn two() {\n    2\n}",
            "fn one() {\r\n    1\r\n}\r\n\r\nfn two() {\r\n    2\r\n}\r\n",
        ] {
            for start in 0..source.len() {
                for end in start..source.len() {
                    let old = &source[start..end];
                    // Only byte boundaries are valid `old` texts.
                    if !source.is_char_boundary(start) || !source.is_char_boundary(end) {
                        continue;
                    }
                    for new in ["X", "X\n", ""] {
                        let applied = resolve_and_apply(source, old, new)
                            .unwrap_or_else(|_| panic!("apply panicked for {old:?} → {new:?}"));
                        if applied == "__refused__" {
                            continue;
                        }
                        assert_eq!(
                            applied,
                            format!("{}{new}{}", &source[..start], &source[end..]),
                            "old {old:?} at {start}..{end} with new {new:?}"
                        );
                    }
                }
            }
        }
    }

    /// An insert anchored at a span change's first line applies after it in either array order:
    /// the inserted text ends up immediately before the replaced (or deleted) region's result,
    /// and each landing note reports where its own text sits.
    #[test]
    fn same_line_insert_applies_after_the_span_change_in_either_array_order() {
        let source = "a\nb\nc\nd\ne\nf\n";
        let span_then_insert = vec![
            change(
                1,
                LineRange::new(2, 4),
                ChangeAction::Replace("X".to_owned()),
            ),
            insert_at(2, 2, "i"),
        ];
        // The reversed array order: insert first in the array, replace second.
        let insert_first = vec![
            insert_at(1, 2, "i"),
            change(
                2,
                LineRange::new(2, 4),
                ChangeAction::Replace("X".to_owned()),
            ),
        ];
        for (label, changes) in [
            ("span first", span_then_insert),
            ("insert first", insert_first),
        ] {
            let (candidate, landings) = apply_changes(source, "a.gamma", &changes).unwrap();
            assert_eq!(candidate, "a\ni\nX\ne\nf\n", "{label}");
            let (insert_landing, replace_landing) = if label == "span first" {
                (landings[1], landings[0])
            } else {
                (landings[0], landings[1])
            };
            assert_eq!(insert_landing, Some(LineRange::new(2, 2)), "{label}");
            assert_eq!(replace_landing, Some(LineRange::new(3, 3)), "{label}");
        }
        // With a delete at the same start line the inserted line survives and the target lines
        // are the ones removed.
        let deleted = vec![
            change(1, LineRange::new(2, 4), ChangeAction::Delete),
            insert_at(2, 2, "i"),
        ];
        let (candidate, landings) = apply_changes(source, "a.gamma", &deleted).unwrap();
        assert_eq!(candidate, "a\ni\ne\nf\n");
        assert_eq!(landings[1], Some(LineRange::new(2, 2)));
        assert_eq!(landings[0], None);
    }

    /// The duplicates footer names eight addresses and counts the rest, like the cut footer.
    #[test]
    fn duplicates_footer_clips_after_eight_names() {
        assert_eq!(
            duplicates_footer(&["src/x.rs#a".to_owned(), "src/x.rs#b".to_owned()]),
            "duplicates shown once: src/x.rs#a, src/x.rs#b\n"
        );
        let ten: Vec<String> = (0..10)
            .map(|index| format!("src/x.rs#sym_{index}"))
            .collect();
        assert_eq!(
            duplicates_footer(&ten),
            format!("duplicates shown once: {}; +2 more\n", ten[..8].join(", "))
        );
    }

    /// A syntax error outside every change's range is attributed to the nearest change before
    /// it — and to the only change when there is one — while an error inside a range keeps the
    /// plain sentence and an error beyond several changes stays the candidate's own.
    #[test]
    fn syntax_errors_after_a_change_are_attributed_to_it() {
        let candidate = "a\nb\nc\nd\ne\nf\ng\n";
        let single = vec![change(
            1,
            LineRange::new(2, 2),
            ChangeAction::Replace("B".to_owned()),
        )];
        let landings = vec![Some(LineRange::new(2, 2))];
        assert!(
            syntax_sentence(candidate, &single, &landings, 2, "boom")
                .starts_with("change 1 produced a syntax error at line 2: \"boom\""),
            "an error inside the range keeps the plain sentence"
        );
        assert_eq!(
            syntax_sentence(candidate, &single, &landings, 5, "boom"),
            "change 1 produced a syntax error at line 5 (just after its line 2): \"boom\" \
             (candidate lines 3-7: \"c\\nd\\ne\\nf\\ng\"); candidate not written"
        );
        let several = vec![
            change(
                1,
                LineRange::new(2, 2),
                ChangeAction::Replace("B".to_owned()),
            ),
            change(
                2,
                LineRange::new(4, 5),
                ChangeAction::Replace("D".to_owned()),
            ),
        ];
        let landings = vec![Some(LineRange::new(2, 2)), Some(LineRange::new(4, 5))];
        assert!(
            syntax_sentence(candidate, &several, &landings, 7, "boom").starts_with(
                "change 2 produced a syntax error at line 7 (just after its lines 4–5)"
            ),
            "the nearest preceding change is named"
        );
        assert!(
            syntax_sentence(candidate, &several, &landings, 1, "boom").starts_with(
                "the candidate produced a syntax error at line 1 outside the edited ranges"
            ),
            "an error no change precedes stays the candidate's own"
        );
    }
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
                // The blank line already above `b` counts toward the spacing.
                LineRange::new(3, 3),
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

    /// Blank lines already beside the insertion point count toward the site's spacing: inserting
    /// after a function that two blank lines separate from a comment keeps exactly two on each
    /// side, and inserting before one keeps the existing two above it.
    #[test]
    fn existing_blank_lines_count_toward_insert_spacing() {
        let source = "def a():\n    pass\n\n\n# section\ndef b():\n    pass\n";
        let after = lang::InsertSite {
            line: 3,
            indent: String::new(),
            blank_before: 2,
            blank_after: 2,
        };
        assert_eq!(
            insert_lines(source, &after, "def n():\n    pass"),
            "def a():\n    pass\n\n\ndef n():\n    pass\n\n\n# section\ndef b():\n    pass\n"
        );
        assert_eq!(insert_gaps(source, &after), (2, 0));
        let before = lang::InsertSite { line: 5, ..after };
        assert_eq!(
            insert_lines(source, &before, "def n():\n    pass"),
            "def a():\n    pass\n\n\ndef n():\n    pass\n\n\n# section\ndef b():\n    pass\n"
        );
        assert_eq!(insert_gaps(source, &before), (0, 2));
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
