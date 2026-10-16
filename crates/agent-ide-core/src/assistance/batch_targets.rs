//! Several-target forms of `ide.symbol`, `ide.graph` and `ide.outline` (batch-api-v1 §3–§5) on
//! the worker.
//!
//! Each entry is answered by the single form itself, called once per entry inside the batch's
//! one job with the shared options, so its block equals the single form's reply text
//! (symbol-graph-item r2 §1). The batch adds only the summary line, the request-order framing,
//! the reply budget and the follow-up call for entries that did not fit.
//!
//! While a language server loads, a several-symbol call answers its caller at once: an entry of
//! a language whose source outline is exact shows M-025's labelled approximate answer, any other
//! loading entry names the retained batch reference and conditional Bridge delivery. The same job then re-runs only
//! those entries in precise mode and its final reply becomes ONE `job-ready` fact naming the
//! batch `detail_ref` and the entry indexes (session-outbox-v1 §13.3).

use super::*;
use crate::assistance::batch;
use crate::assistance::outbox::{DeferSpec, FencedPath, SessionOutbox};

/// Margin between a several-symbol job's own precise budget and its job deadline, so the batch,
/// not the worker's deadline check, settles the entries still loading when the budget ends.
const BATCH_DEADLINE_MARGIN: Duration = Duration::from_secs(5);

/// How often a several-symbol job whose entries wait for a loading server re-checks it, as the
/// single form does.
const BATCH_RETRY: Duration = Duration::from_millis(300);

/// How one entry of a several-symbol call stands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryState {
    /// A card or graph, precise.
    Answered,
    /// A labelled approximate card or graph given while its server loads; the precise answer
    /// follows.
    Approximate,
    /// No answer yet: its server loads (and, once answered early, its language has no exact
    /// source outline, so the block names its retained reference and conditional Bridge delivery).
    Deferred,
    /// A graph root whose every requested direction faulted.
    Unavailable,
    /// The single form's failure for this entry.
    Failed,
}

impl EntryState {
    /// Whether the entry still waits for its precise answer.
    fn loading(self) -> bool {
        matches!(self, Self::Approximate | Self::Deferred)
    }
}

/// The state of an entry the single form answered, read from its text: an unavailable root's
/// graph says `unavailable (` in its header line.
fn answered_state(text: &str) -> EntryState {
    if text
        .lines()
        .next()
        .unwrap_or_default()
        .contains(" unavailable (")
    {
        EntryState::Unavailable
    } else {
        EntryState::Answered
    }
}

/// One entry of a several-symbol call, kept across the attempts of its job.
pub(super) struct SymbolEntry {
    /// The requested address, exactly as given.
    name: String,
    /// The entry's block: the single form's reply text, the approximate answer or the pending
    /// line; empty while the entry was never answered.
    text: String,
    /// How the entry stands.
    state: EntryState,
    /// Worktree-relative definition file of an approximate answer: a fence of the follow-up.
    definition: Option<PathBuf>,
}

/// A several-symbol job that answered its caller early and keeps re-running its approximate and
/// deferred entries in precise mode until `until` (batch-api-v1 §4).
pub(super) struct SymbolBatch {
    /// Every entry in request order.
    entries: Vec<SymbolEntry>,
    /// When the precise budget ends; entries still loading then fail as `provider_loading`.
    until: tokio::time::Instant,
}

/// The reply text of a several-symbol call: the summary line, the whole blocks of the first
/// entries in request order while they fit one reply with `reference`, then the exact call that
/// fetches the entries that did not fit.
///
/// The summary is `symbols: <n> — <k> cards, <a> approximate, <p> pending, <f> failed` or
/// `graph: <n> roots (<direction>, depth <d>) — <k> graphs, <a> approximate, <p> pending, <u>
/// unavailable, <f> failed`, counting the delivered entries; `options` are the request without
/// its `symbols`.
fn compose_symbols(
    tool: AssistanceTool,
    entries: &[SymbolEntry],
    options: &Value,
    reference: &str,
) -> String {
    let compose = |count: usize| {
        let shown = &entries[..count];
        let tally = |state: EntryState| shown.iter().filter(|entry| entry.state == state).count();
        let answered = tally(EntryState::Answered);
        let approximate = tally(EntryState::Approximate);
        let pending = tally(EntryState::Deferred);
        let failed = tally(EntryState::Failed);
        let mut text = match tool {
            AssistanceTool::Graph => format!(
                "graph: {} roots ({}, depth {}) — {answered} graphs, {approximate} approximate, {pending} pending, {} unavailable, {failed} failed\n",
                entries.len(),
                options
                    .get("direction")
                    .and_then(Value::as_str)
                    .unwrap_or("callers"),
                options.get("depth").and_then(Value::as_u64).unwrap_or(2),
                tally(EntryState::Unavailable),
            ),
            _ => format!(
                "symbols: {} — {answered} cards, {approximate} approximate, {pending} pending, {failed} failed\n",
                entries.len(),
            ),
        };
        for entry in shown {
            text.push_str(&entry.text);
        }
        if count < entries.len() {
            let cut = &entries[count..];
            let names = cut
                .iter()
                .take(8)
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>();
            let mut call = options.clone();
            call["symbols"] = serde_json::json!(names);
            text.push_str(&format!(
                "not included (over the reply budget): {} — call {} {call}",
                names.join(", "),
                tool.mcp_name()
            ));
            if cut.len() > names.len() {
                text.push_str(&format!("; +{} more", cut.len() - names.len()));
            }
            text.push('\n');
        }
        text
    };
    let fits = |text: &str| {
        content::fits(
            &PeerReply::Complete {
                kind: kind_of(tool),
                text: text.to_owned(),
                detail_ref: Some(reference.to_owned()),
                truncated: true,
                continuation: false,
            },
            content::reply_envelope(),
        )
    };
    let included = (1..=entries.len())
        .rev()
        .find(|&count| fits(&compose(count)))
        .unwrap_or(1);
    compose(included)
}

impl Worker<'_> {
    /// `ide.symbol {symbols, …}` or `ide.graph {symbols, …}`: every entry answered as the single
    /// form answers it, in request order, behind one summary line ([`compose_symbols`]).
    ///
    /// A failed entry's block is the single form's own error text, so one bad entry never fails
    /// the rest; an exactly repeated entry is answered once. An entry whose server is still
    /// loading makes the first attempt answer the waiting caller at once
    /// ([`Self::answer_symbols_early`]); the job then re-runs only its loading entries every
    /// 300 ms for up to [`approximate::PRECISE_BUDGET`], after which those still loading fail
    /// as `provider_loading`, and its final reply becomes the job's one `job-ready` fact. When
    /// the caller could not be answered early (it already left, or the session has
    /// [`approximate::MAX_DEFERRED_PER_SESSION`] jobs waiting), the whole batch waits as the
    /// single form did before.
    pub(super) async fn batch_symbols(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let tool = job.tool;
        let request = job.parameters.clone();
        let mut options = request.clone();
        if let Some(object) = options.as_object_mut() {
            object.remove("symbols");
        }
        let resumed = self.symbol_batches.remove(&job.reference);
        let until = resumed.as_ref().map(|batch| batch.until);
        let mut entries = match resumed {
            Some(batch) => batch.entries,
            None => {
                let mut names: Vec<String> = Vec::new();
                for entry in request
                    .get("symbols")
                    .and_then(Value::as_array)
                    .ok_or(FailureCode::Internal)?
                    .iter()
                    .filter_map(Value::as_str)
                {
                    if !names.iter().any(|known| known == entry) {
                        names.push(entry.to_owned());
                    }
                }
                names
                    .into_iter()
                    .map(|name| SymbolEntry {
                        name,
                        text: String::new(),
                        state: EntryState::Deferred,
                        definition: None,
                    })
                    .collect::<Vec<_>>()
            }
        };
        for entry in entries.iter_mut().filter(|entry| entry.state.loading()) {
            let mut single = options.clone();
            single["symbol"] = serde_json::json!(entry.name);
            job.parameters = single;
            job.failure_detail = None;
            // Boxed: the single forms route their own `symbols` form back here.
            let answer = match tool {
                AssistanceTool::Graph => Box::pin(self.graph(job)).await,
                _ => Box::pin(self.symbol(job)).await,
            };
            job.parameters = request.clone();
            job.park_until = None;
            match answer {
                Ok((
                    PeerReply::Complete {
                        mut text,
                        continuation,
                        ..
                    },
                    ..,
                )) => {
                    if continuation {
                        // A card larger than the whole reply pages in its single form.
                        text.push_str(&format!(
                            "\n(continues: {} {{\"symbol\":{}}})",
                            tool.mcp_name(),
                            Value::String(entry.name.clone())
                        ));
                    }
                    if !text.ends_with('\n') {
                        text.push('\n');
                    }
                    entry.state = answered_state(&text);
                    entry.text = text;
                    entry.definition = None;
                }
                // Its server is still loading: the entry keeps its early block and waits.
                Err(FailureCode::ProviderLoading) => {}
                Ok((other, ..)) => {
                    entry.text = rendered_text(other, tool);
                    entry.state = EntryState::Failed;
                }
                Err(code) => {
                    let detail = job.failure_detail.take();
                    entry.text = rendered_text(PeerReply::Error { code, detail }, tool);
                    entry.state = EntryState::Failed;
                }
            }
        }
        job.failure_detail = None;
        if entries.iter().any(|entry| entry.state.loading()) {
            let now = tokio::time::Instant::now();
            match until {
                Some(until) if now < until => {
                    job.park_until = Some(now + BATCH_RETRY);
                    self.symbol_batches
                        .insert(job.reference.clone(), SymbolBatch { entries, until });
                    return Err(FailureCode::ProviderLoading);
                }
                Some(_) => {
                    // The precise budget ended: what still loads fails as the single form would.
                    for entry in entries.iter_mut().filter(|entry| entry.state.loading()) {
                        entry.text = rendered_text(
                            PeerReply::Error {
                                code: FailureCode::ProviderLoading,
                                detail: None,
                            },
                            tool,
                        );
                        entry.state = EntryState::Failed;
                    }
                }
                None => {
                    if self.answer_symbols_early(job, &mut entries, &options).await {
                        let until = now + approximate::PRECISE_BUDGET;
                        job.deadline = until + BATCH_DEADLINE_MARGIN;
                        let live = &self.deferred_precise;
                        self.symbol_batches
                            .retain(|reference, _| live.contains_key(reference));
                        self.symbol_batches
                            .insert(job.reference.clone(), SymbolBatch { entries, until });
                    }
                    job.park_until = Some(now + BATCH_RETRY);
                    return Err(FailureCode::ProviderLoading);
                }
            }
        }
        let text = compose_symbols(tool, &entries, &options, &job.reference);
        let authority = self.authority(job.invocation.binding_ref()).await.ok();
        let (reply, page) =
            ContextPageState::new(text, 0, false, kind_of(tool)).next(&job.reference)?;
        self.shared.set_context_page(&job.reference, page);
        Ok((reply, authority, None))
    }

    /// Answers the waiting caller of several-symbol `job` at once while some `entries` wait for
    /// a loading server, and registers the follow-up; returns whether the caller received it.
    ///
    /// Each loading entry gets M-025's approximate answer when its language's source outline is
    /// exact (symbol-graph-item r2 §4), else a line naming the retained batch reference and
    /// conditional Bridge delivery.
    /// The reply is the batch's composed text with the job's `detail_ref`. Once sent, the job is
    /// counted against [`approximate::MAX_DEFERRED_PER_SESSION`] and deferred with the session
    /// outbox under the target `batch <detail_ref>: items <i,j>` (1-based indexes of the loading
    /// entries), fenced by the approximate entries' definition files; it holds the turn only
    /// when an entry got no answer at all. Nothing is sent when the caller already left or the
    /// session has that many jobs waiting already.
    async fn answer_symbols_early(
        &mut self,
        job: &mut Job,
        entries: &mut [SymbolEntry],
        options: &Value,
    ) -> bool {
        let binding = job.invocation.binding_ref().clone();
        if job.stop_reply.is_none()
            || self.deferred_count(&binding) >= approximate::MAX_DEFERRED_PER_SESSION
        {
            return false;
        }
        let request = job.parameters.clone();
        for entry in entries.iter_mut().filter(|entry| entry.state.loading()) {
            let mut single = options.clone();
            single["symbol"] = serde_json::json!(entry.name);
            job.parameters = single;
            match self.approximate(job).await {
                Some(answer) => {
                    entry.text = answer.text;
                    if !entry.text.ends_with('\n') {
                        entry.text.push('\n');
                    }
                    entry.state = EntryState::Approximate;
                    entry.definition = Some(answer.definition);
                }
                None => {
                    entry.text = format!(
                        "pending: {} - result retained under {}; Bridge delivers it automatically when configured\n",
                        entry.name, job.reference
                    );
                    entry.state = EntryState::Deferred;
                }
            }
        }
        job.parameters = request;
        let text = compose_symbols(job.tool, entries, options, &job.reference);
        let Some(sender) = job.stop_reply.take() else {
            return false;
        };
        let reply = PeerReply::Complete {
            kind: kind_of(job.tool),
            text,
            detail_ref: Some(job.reference.clone()),
            truncated: false,
            continuation: false,
        };
        if sender.send(reply).is_err() {
            return false;
        }
        self.deferred_precise
            .insert(job.reference.clone(), binding.clone());
        let indexes = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.state.loading())
            .map(|(index, _)| (index + 1).to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut fence: Vec<FencedPath> = Vec::new();
        for file in entries.iter().filter_map(|entry| entry.definition.as_ref()) {
            for path in self.fence_of(&binding, file) {
                if !fence.iter().any(|known| known.path == path.path) {
                    fence.push(path);
                }
            }
        }
        let spec = DeferSpec {
            tool: job.tool,
            target: format!("batch {}: items {indexes}", job.reference),
            fence_paths: fence,
        };
        SessionOutbox::defer(&*self.shared, &job.reference, spec);
        true
    }

    /// `ide.outline {paths | glob, kinds?}`: several files outlined in ONE reply.
    ///
    /// The files are the `paths` in request order (an exact repeat once), or the regular files
    /// below the worktree matching `glob` in path order, at most
    /// [`batch::MAX_OUTLINE_FILES`]. Each file is outlined by the single form with the same
    /// `kinds`, so its block is the single form's text and a failed file fails only itself. The
    /// reply opens with `outline: <n> files — <k> outlined, <m> listed only, <f> failed`, plus
    /// `; +<x> more files matched` when a glob matched more. Whole outlines are delivered in order
    /// while they fit; every later file is one line `<path> (<lines> lines, <symbols> symbols) →
    /// ide.outline {"path":…}`.
    pub(super) async fn batch_outline(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let request = job.parameters.clone();
        let authority = self.authority(job.invocation.binding_ref()).await?;
        let (paths, more) = match request.get("glob").and_then(Value::as_str) {
            Some(pattern) => glob_files(authority.worktree().worktree_path(), pattern),
            None => {
                let mut paths: Vec<String> = Vec::new();
                for path in request
                    .get("paths")
                    .and_then(Value::as_array)
                    .ok_or(FailureCode::Internal)?
                    .iter()
                    .filter_map(Value::as_str)
                {
                    if !paths.iter().any(|known| known == path) {
                        paths.push(path.to_owned());
                    }
                }
                (paths, 0)
            }
        };
        let mut single = serde_json::json!({});
        if let Some(kinds) = request.get("kinds") {
            single["kinds"] = kinds.clone();
        }
        // A paged outline is listed; exact source counts stay separate from its rendered text.
        let mut blocks = Vec::with_capacity(paths.len());
        for path in &paths {
            let mut call = single.clone();
            call["path"] = serde_json::json!(path);
            job.parameters = call;
            job.failure_detail = None;
            // Boxed: the single form routes its own batch keys back here.
            let mut summary = None;
            let answer = Box::pin(self.outline_with_summary(job, &mut summary)).await;
            job.parameters = request.clone();
            match answer {
                Ok((
                    PeerReply::Complete {
                        mut text,
                        continuation,
                        ..
                    },
                    ..,
                )) => {
                    if !text.ends_with('\n') {
                        text.push('\n');
                    }
                    blocks.push((path.clone(), text, false, continuation, summary));
                }
                Err(FailureCode::ProviderLoading) if job.park_until.is_some() => {
                    return Err(FailureCode::ProviderLoading);
                }
                Ok((other, ..)) => {
                    let text = rendered_text(other, AssistanceTool::Outline);
                    blocks.push((path.clone(), text, true, false, None));
                }
                Err(code) => {
                    let detail = job.failure_detail.take();
                    let text =
                        rendered_text(PeerReply::Error { code, detail }, AssistanceTool::Outline);
                    blocks.push((path.clone(), text, true, false, None));
                }
            }
        }
        job.failure_detail = None;
        let reference = job.reference.clone();
        let fits = |text: &str| {
            content::fits(
                &PeerReply::Complete {
                    kind: ResultKind::Outline,
                    text: text.to_owned(),
                    detail_ref: Some(reference.clone()),
                    truncated: true,
                    continuation: false,
                },
                content::reply_envelope(),
            )
        };
        let full = |index: usize, count: usize| {
            let (_, _, failed, paged, _) = &blocks[index];
            *failed || (index < count && !*paged)
        };
        let compose = |count: usize| {
            let failed = blocks.iter().filter(|block| block.2).count();
            let outlined = (0..blocks.len())
                .filter(|&index| full(index, count) && !blocks[index].2)
                .count();
            let listed = blocks.len() - failed - outlined;
            let mut text = format!(
                "outline: {} files — {outlined} outlined, {listed} listed only, {failed} failed",
                blocks.len()
            );
            if more > 0 {
                text.push_str(&format!("; +{more} more files matched"));
            }
            text.push('\n');
            for (index, (path, block, _, _, summary)) in blocks.iter().enumerate() {
                if full(index, count) {
                    text.push_str(block);
                } else {
                    text.push_str(&listed_line(path, block, *summary));
                }
            }
            text
        };
        let included = (0..=blocks.len())
            .rev()
            .find(|&count| fits(&compose(count)))
            .unwrap_or(0);
        let text = compose(included);
        let (reply, page) =
            ContextPageState::new(text, 0, false, ResultKind::Outline).next(&job.reference)?;
        self.shared.set_context_page(&job.reference, page);
        Ok((reply, Some(authority), None))
    }
}

/// Most files a glob walk compares before it stops counting.
const MAX_GLOB_CANDIDATES: usize = 10_000;

/// Most directories a glob walk opens.
const MAX_GLOB_DIRECTORIES: usize = 2_000;

/// The worktree-relative regular files below `root` matching `pattern`, path-sorted, at most
/// [`batch::MAX_OUTLINE_FILES`], and how many more matched.
///
/// The walk starts at the pattern's literal directory ([`batch::glob_root`]) only when that
/// directory resolves inside the worktree, opens at most [`MAX_GLOB_DIRECTORIES`] directories,
/// never follows a symbolic link (a linked file or directory is not a match), and skips hidden
/// directories and the directories the name index skips (`target`, `node_modules`, …).
fn glob_files(root: &Path, pattern: &str) -> (Vec<String>, usize) {
    let start = batch::glob_root(pattern);
    let inside = std::fs::canonicalize(root)
        .ok()
        .zip(std::fs::canonicalize(root.join(&start)).ok())
        .is_some_and(|(root, start)| start.starts_with(root));
    if !inside {
        return (Vec::new(), 0);
    }
    let mut matched: Vec<String> = Vec::new();
    let mut queue = std::collections::VecDeque::from([PathBuf::from(&start)]);
    let mut opened = 0;
    while let Some(directory) = queue.pop_front() {
        opened += 1;
        if opened > MAX_GLOB_DIRECTORIES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(root.join(&directory)) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = directory.join(&name);
            if kind.is_dir() {
                if !name.starts_with('.')
                    && !crate::intelligence::names::SKIPPED_DIRECTORIES.contains(&name.as_str())
                {
                    queue.push_back(path);
                }
            } else if kind.is_file()
                && let Some(path) = path.to_str()
                && batch::glob_matches(pattern, path)
            {
                matched.push(path.to_owned());
                if matched.len() >= MAX_GLOB_CANDIDATES {
                    queue.clear();
                    break;
                }
            }
        }
    }
    matched.sort();
    let more = matched.len().saturating_sub(batch::MAX_OUTLINE_FILES);
    matched.truncate(batch::MAX_OUTLINE_FILES);
    (matched, more)
}

/// Names a listed-only path, exact source line/symbol counts and one call that delivers it.
/// `summary` is the full-file metadata collected before pagination; a paged block names only
/// its line count as before. Directory blocks have no source summary and retain their legacy
/// header fallback. JSON encoding keeps the suggested call valid for every path.
fn listed_line(path: &str, block: &str, summary: Option<(u32, usize)>) -> String {
    if let Some((lines, symbols)) = summary {
        let call = Value::String(path.to_owned());
        if block.starts_with("page ") {
            return format!("{path} ({lines} lines) → ide.outline {{\"path\":{call}}}\n");
        }
        return format!(
            "{path} ({lines} lines, {symbols} symbols) → ide.outline {{\"path\":{call}}}\n"
        );
    }
    let header = block
        .lines()
        .find(|line| line.contains("  ("))
        .unwrap_or_default();
    let lines = header
        .split_once("  (")
        .and_then(|(_, rest)| rest.split(' ').next())
        .unwrap_or("?");
    let call = Value::String(path.to_owned());
    if block.starts_with("page ") {
        return format!("{path} ({lines} lines) → ide.outline {{\"path\":{call}}}\n");
    }
    let symbols = block.lines().count().saturating_sub(1);
    format!("{path} ({lines} lines, {symbols} symbols) → ide.outline {{\"path\":{call}}}\n")
}

/// The complete-result kind of a several-symbol call: the tool's own kind.
fn kind_of(tool: AssistanceTool) -> ResultKind {
    match tool {
        AssistanceTool::Graph => ResultKind::Graph,
        _ => ResultKind::Symbol,
    }
}

/// The text the single form shows for `reply` (an entry's failure), rendered exactly as the
/// facade renders that reply on its own, without a status plate.
pub(super) fn rendered_text(reply: PeerReply, tool: AssistanceTool) -> String {
    let mut text = content::render_with_call(
        reply,
        None,
        content::Envelope::TextOnly,
        Some(tool.mcp_name()),
        None,
    )
    .and_then(|rendered| {
        rendered
            .content
            .first()
            .and_then(|block| block.as_text())
            .map(|block| block.text.clone())
    })
    .unwrap_or_else(|| "error: internal".to_owned());
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A listed cut retains exact counts after ordinary outlines drop their redundant headers;
    /// a paged cut retains the whole file's line count rather than counting delivered rows.
    #[test]
    fn listed_outline_uses_source_counts_without_a_rendered_header() {
        assert_eq!(
            listed_line("a.rs", "1 a.rs#value  fn value()\n", Some((8, 1))),
            "a.rs (8 lines, 1 symbols) → ide.outline {\"path\":\"a.rs\"}\n"
        );
        assert_eq!(
            listed_line("big.rs", "page 1; partial rows\n", Some((7500, 2500))),
            "big.rs (7500 lines) → ide.outline {\"path\":\"big.rs\"}\n"
        );
    }

    #[test]
    fn entry_states_read_their_first_line() {
        assert_eq!(
            answered_state("graph: callers of f unavailable (incoming calls request failed)\n"),
            EntryState::Unavailable
        );
        assert_eq!(
            answered_state("symbol: f (fn, a.rs:1–3)\n"),
            EntryState::Answered
        );
    }

    /// One entry of `state` named `name` whose block is `text`.
    fn entry(name: &str, text: &str, state: EntryState) -> SymbolEntry {
        SymbolEntry {
            name: name.to_owned(),
            text: text.to_owned(),
            state,
            definition: None,
        }
    }

    #[test]
    fn early_replies_count_approximate_and_pending_entries() {
        let entries = [
            entry("a.rs#f", "symbol: f (fn, a.rs:1–3)\n", EntryState::Answered),
            entry(
                "b.rs#g",
                "approximate: the language server is still indexing; …\nsymbol: g\n",
                EntryState::Approximate,
            ),
            entry(
                "c.py#h",
                "pending: c.py#h - result retained under ref-1; Bridge delivers it automatically when configured\n",
                EntryState::Deferred,
            ),
            entry("d.rs#x", "error: unknown_symbol\n", EntryState::Failed),
        ];
        let text = compose_symbols(
            AssistanceTool::Symbol,
            &entries,
            &serde_json::json!({"usages":true}),
            "ref-1",
        );
        assert!(
            text.starts_with("symbols: 4 — 1 cards, 1 approximate, 1 pending, 1 failed\nsymbol: f"),
            "{text}"
        );
        assert!(
            text.contains("\npending: c.py#h - result retained under ref-1; Bridge delivers it automatically when configured\n"),
            "{text}"
        );
        batch::assert_no_polling(&text);
        batch::assert_no_unfinished_seconds(&text);
        let graph = compose_symbols(
            AssistanceTool::Graph,
            &entries[1..3],
            &serde_json::json!({"direction":"both","depth":1}),
            "ref-1",
        );
        assert!(
            graph.starts_with(
                "graph: 2 roots (both, depth 1) — 0 graphs, 1 approximate, 1 pending, 0 unavailable, 0 failed\n"
            ),
            "{graph}"
        );
        assert!(EntryState::Approximate.loading() && EntryState::Deferred.loading());
        assert!(!EntryState::Answered.loading() && !EntryState::Failed.loading());
    }

    #[test]
    fn failed_entries_render_as_their_single_form_errors() {
        let text = rendered_text(
            PeerReply::Error {
                code: FailureCode::UnknownSymbol,
                detail: None,
            },
            AssistanceTool::Symbol,
        );
        assert_eq!(
            text,
            "No symbol matches the requested address.\nRepair: List the file's addresses with ide.outline.\n"
        );
        assert!(text.ends_with('\n'));
    }
}
