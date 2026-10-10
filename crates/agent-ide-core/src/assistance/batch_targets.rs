//! Several-target forms of `ide.symbol`, `ide.graph` and `ide.outline` (batch-api-v1 §3–§5) on
//! the worker.
//!
//! Each entry is answered by the single form itself, called once per entry inside the batch's
//! one job with the shared options, so its block equals the single form's reply text
//! (symbol-graph-item r2 §1). The batch adds only the summary line, the request-order framing,
//! the reply budget and the follow-up call for entries that did not fit. While a language server
//! is still loading the single form parks the job; the whole batch then parks and answers once it
//! can, so a batch never mixes a partial answer with a waiting one.

use super::*;
use crate::assistance::batch;

/// How one entry of a several-symbol call was answered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryState {
    /// A card or graph, precise.
    Answered,
    /// A labelled approximate card or graph (the language server was still indexing).
    Approximate,
    /// A graph root whose every requested direction faulted.
    Unavailable,
    /// The single form's failure for this entry.
    Failed,
}

/// The state of one answered entry, read from its text: approximate answers open with
/// `approximate:` and an unavailable root's graph says `unavailable (` in its header line.
fn answered_state(text: &str) -> EntryState {
    let first = text.lines().next().unwrap_or_default();
    if first.starts_with("approximate:") {
        EntryState::Approximate
    } else if first.contains(" unavailable (") {
        EntryState::Unavailable
    } else {
        EntryState::Answered
    }
}

impl Worker<'_> {
    /// `ide.symbol {symbols, …}` or `ide.graph {symbols, …}`: every entry answered as the single
    /// form answers it, in request order, behind one summary line.
    ///
    /// The summary is `symbols: <n> — <k> cards, <a> approximate, <p> pending, <f> failed` or
    /// `graph: <n> roots (<direction>, depth <d>) — <k> graphs, <a> approximate, <p> pending,
    /// <u> unavailable, <f> failed`. A failed entry's block is the single form's own error text,
    /// so one bad entry never fails the rest. Whole blocks are delivered in request order while
    /// they fit the reply; the rest are named with the exact call that fetches them. An exactly
    /// repeated entry is answered once. When the single form parks the job (a server still
    /// loading), the whole batch parks and is answered from the start when resumed.
    pub(super) async fn batch_symbols(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let tool = job.tool;
        let request = job.parameters.clone();
        let mut entries: Vec<String> = Vec::new();
        for entry in request
            .get("symbols")
            .and_then(Value::as_array)
            .ok_or(FailureCode::Internal)?
            .iter()
            .filter_map(Value::as_str)
        {
            if !entries.iter().any(|known| known == entry) {
                entries.push(entry.to_owned());
            }
        }
        let mut options = request.clone();
        if let Some(object) = options.as_object_mut() {
            object.remove("symbols");
        }
        let mut blocks: Vec<(String, String, EntryState)> = Vec::with_capacity(entries.len());
        for entry in &entries {
            let mut single = options.clone();
            single["symbol"] = serde_json::json!(entry);
            job.parameters = single;
            job.failure_detail = None;
            // Boxed: the single forms route their own `symbols` form back here.
            let answer = match tool {
                AssistanceTool::Graph => Box::pin(self.graph(job)).await,
                _ => Box::pin(self.symbol(job)).await,
            };
            job.parameters = request.clone();
            match answer {
                Ok((
                    PeerReply::Complete {
                        text, continuation, ..
                    },
                    ..,
                )) => {
                    let mut text = text;
                    if continuation {
                        // A card larger than the whole reply pages in its single form.
                        text.push_str(&format!(
                            "\n(continues: {} {{\"symbol\":{}}})",
                            tool.mcp_name(),
                            Value::String(entry.clone())
                        ));
                    }
                    if !text.ends_with('\n') {
                        text.push('\n');
                    }
                    let state = answered_state(&text);
                    blocks.push((entry.clone(), text, state));
                }
                Err(FailureCode::ProviderLoading) if job.park_until.is_some() => {
                    return Err(FailureCode::ProviderLoading);
                }
                Ok((other, ..)) => {
                    let text = rendered_text(other, tool);
                    blocks.push((entry.clone(), text, EntryState::Failed));
                }
                Err(code) => {
                    let detail = job.failure_detail.take();
                    let text = rendered_text(PeerReply::Error { code, detail }, tool);
                    blocks.push((entry.clone(), text, EntryState::Failed));
                }
            }
        }
        job.failure_detail = None;
        let reference = job.reference.clone();
        let fits = |text: &str| {
            content::fits(
                &PeerReply::Complete {
                    kind: kind_of(tool),
                    text: text.to_owned(),
                    detail_ref: Some(reference.clone()),
                    truncated: true,
                    continuation: false,
                },
                content::Envelope::WithStructured,
            )
        };
        let compose = |count: usize| {
            let shown = &blocks[..count];
            let tally = |state: EntryState| shown.iter().filter(|block| block.2 == state).count();
            let answered = tally(EntryState::Answered);
            let mut text = match tool {
                AssistanceTool::Graph => format!(
                    "graph: {} roots ({}, depth {}) — {answered} graphs, {} approximate, 0 pending, {} unavailable, {} failed\n",
                    blocks.len(),
                    options
                        .get("direction")
                        .and_then(Value::as_str)
                        .unwrap_or("callers"),
                    options.get("depth").and_then(Value::as_u64).unwrap_or(2),
                    tally(EntryState::Approximate),
                    tally(EntryState::Unavailable),
                    tally(EntryState::Failed),
                ),
                _ => format!(
                    "symbols: {} — {answered} cards, {} approximate, 0 pending, {} failed\n",
                    blocks.len(),
                    tally(EntryState::Approximate),
                    tally(EntryState::Failed),
                ),
            };
            for (_, block, _) in shown {
                text.push_str(block);
            }
            if count < blocks.len() {
                let cut = &blocks[count..];
                let names = cut
                    .iter()
                    .take(8)
                    .map(|block| block.0.as_str())
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
        let included = (1..=blocks.len())
            .rev()
            .find(|&count| fits(&compose(count)))
            .unwrap_or(1);
        let text = compose(included);
        let authority = self.authority(job.invocation.binding_ref()).await.ok();
        let (reply, page) =
            ContextPageState::new(text, 0, false, kind_of(tool)).next(&job.reference)?;
        self.shared.set_context_page(&job.reference, page);
        Ok((reply, authority, None))
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
        // (path, text, failed, paged): a paged outline is too large to embed and is listed.
        let mut blocks: Vec<(String, String, bool, bool)> = Vec::with_capacity(paths.len());
        for path in &paths {
            let mut call = single.clone();
            call["path"] = serde_json::json!(path);
            job.parameters = call;
            job.failure_detail = None;
            // Boxed: the single form routes its own batch keys back here.
            let answer = Box::pin(self.outline(job)).await;
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
                    blocks.push((path.clone(), text, false, continuation));
                }
                Err(FailureCode::ProviderLoading) if job.park_until.is_some() => {
                    return Err(FailureCode::ProviderLoading);
                }
                Ok((other, ..)) => {
                    let text = rendered_text(other, AssistanceTool::Outline);
                    blocks.push((path.clone(), text, true, false));
                }
                Err(code) => {
                    let detail = job.failure_detail.take();
                    let text =
                        rendered_text(PeerReply::Error { code, detail }, AssistanceTool::Outline);
                    blocks.push((path.clone(), text, true, false));
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
                content::Envelope::WithStructured,
            )
        };
        let full = |index: usize, count: usize| {
            let (_, _, failed, paged) = &blocks[index];
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
            for (index, (path, block, ..)) in blocks.iter().enumerate() {
                if full(index, count) {
                    text.push_str(block);
                } else {
                    text.push_str(&listed_line(path, block));
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

/// The one line a file past the reply budget gets: `<path> (<lines> lines, <symbols> symbols)
/// → ide.outline {"path":…}`, read from its outline block (header `<path>  (<n> lines, …)`,
/// one line per symbol after it). A block the single form paged (it opens with its `page` line)
/// holds only its first page, so its line names the file's lines alone.
fn listed_line(path: &str, block: &str) -> String {
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
fn rendered_text(reply: PeerReply, tool: AssistanceTool) -> String {
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

    #[test]
    fn entry_states_read_their_first_line() {
        assert_eq!(
            answered_state("approximate: the language server is still indexing; …\nsymbol: f"),
            EntryState::Approximate
        );
        assert_eq!(
            answered_state("graph: callers of f unavailable (incoming calls request failed)\n"),
            EntryState::Unavailable
        );
        assert_eq!(
            answered_state("symbol: f (fn, a.rs:1–3)\n"),
            EntryState::Answered
        );
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
        assert!(text.starts_with("error: unknown_symbol"), "{text}");
        assert!(text.ends_with('\n'));
    }
}
