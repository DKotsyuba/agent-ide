//! Several-target forms of `ide.symbol` and `ide.graph` (batch-api-v1 §4–§5) on the worker.
//!
//! Each entry is answered by the single form itself, called once per entry inside the batch's
//! one job with the shared options, so its block equals the single form's reply text
//! (symbol-graph-item r2 §1). The batch adds only the summary line, the request-order framing,
//! the reply budget and the follow-up call for entries that did not fit. While a language server
//! is still loading the single form parks the job; the whole batch then parks and answers once it
//! can, so a batch never mixes a partial answer with a waiting one.

use super::*;

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
            answered_state("approximate: rust-analyzer is still indexing; …\nsymbol: f"),
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
