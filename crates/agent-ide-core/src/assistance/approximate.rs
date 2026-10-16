//! Answers for `ide.symbol` and `ide.graph` while a language server loads, and the registration of
//! their precise follow-up with the session outbox (symbol-graph-item r2 §4-§5).
//!
//! A symbol or graph job that meets a loading server (it parks, or its provider answers
//! `provider_loading`) answers its waiting caller at once:
//! - for a language whose source outline is exact, with an answer labelled approximate: the
//!   definition from the source outline, usages and depth-1 callers from a bounded whole-word
//!   scan of the worktree's files of that language, rows marked `~`;
//! - for every other language (or a name the scan cannot resolve to one definition), with the
//!   ordinary `pending` reply.
//!
//! The same job then keeps running in precise mode, re-checking the server every 300 ms for up to
//! [`PRECISE_BUDGET`], and is registered with the session outbox (the worker's shared state),
//! which retains its final reply or failure under its reference and hands a `job-ready` fact to
//! Bridge when configured. At most [`MAX_DEFERRED_PER_SESSION`] such jobs run per
//! session; beyond that the approximate answer says the precise answer is not scheduled and the
//! job ends at its next retry, while a non-exact language keeps the 0.10.8 wait.

use super::navigation::{classify_constructions, collapse_named, constructs, skip_generics};
use super::symbols::{contains_bare_name, history_lines, innermost};
use super::*;
use crate::assistance::outbox::{DeferSpec, FencedPath, SessionOutbox};
use crate::lang::{
    self, Language as Lang, Outline, SymbolPath,
    render::{self, Call, SymbolCard, Usage},
};
use crate::workspace::observation::{MAX_SOURCE_BYTES, SourceReadLimits, read_authorized_source};

/// How long a job that answered early keeps waiting for the precise answer.
pub(super) const PRECISE_BUDGET: Duration = Duration::from_secs(10 * 60);

/// Jobs that answered early and still run for the precise answer, per session.
pub(super) const MAX_DEFERRED_PER_SESSION: usize = 4;

/// Wall-clock budget of one lexical scan of the worktree.
const SCAN_BUDGET: Duration = Duration::from_secs(2);

/// Files one lexical scan reads at most.
const MAX_SCANNED_FILES: usize = 20_000;

/// Files whose source outline one approximate caller list reads at most.
const MAX_CALLER_OUTLINES: usize = 64;

/// Longest outbox target; the subject is `"<tool> <target>"` (session outbox v1 §3).
const MAX_TARGET_BYTES: usize = 120;

/// One whole-word occurrence of a name found by the lexical scan.
struct Occurrence {
    /// Worktree-relative file.
    file: PathBuf,
    /// One-based line.
    line: u32,
    /// The trimmed line text.
    text: String,
}

/// What the lexical scan found for one name.
struct Scan {
    /// Occurrences in listing order, the definition line excluded.
    found: Vec<Occurrence>,
    /// The bound that stopped the scan, when one did (`2 s budget`, `20000 files`).
    stopped: Option<&'static str>,
}

/// The approximate answer of one symbol or graph job and the file it is built from.
pub(super) struct Approximate {
    /// The complete reply text, starting with the `approximate:` line.
    pub(super) text: String,
    /// Worktree-relative definition file: the outbox fence of the precise answer.
    pub(super) definition: PathBuf,
}

impl Worker<'_> {
    /// Answers a symbol or graph `job` whose provider is loading (it parked, or answered
    /// `provider_loading`), once, and keeps the job running for the precise answer.
    ///
    /// Only the job's first attempt with a live waiter answers early; later attempts of a job
    /// that answered early only keep it parked until [`PRECISE_BUDGET`] ends. The early answer is
    /// the approximate answer for a language whose source outline is exact, else `pending`.
    /// When the waiter received it, the job's deadline moves [`PRECISE_BUDGET`] ahead, it is
    /// counted against [`MAX_DEFERRED_PER_SESSION`] and registered for its completion fact. When the
    /// session already has that many, an approximate answer says the precise one is not scheduled and the job ends at its
    /// next retry; a `pending` answer is not sent and the job waits as before. A waiter that is
    /// already gone gets nothing here: the inline-wait timeout answered it `pending`.
    pub(super) async fn answer_while_loading(&mut self, job: &mut Job) {
        let binding = job.invocation.binding_ref().clone();
        if job.stop_reply.is_none() {
            // A later attempt of a job that answered early keeps waiting for the precise answer.
            if self.deferred_precise.contains_key(&job.reference)
                && job.park_until.is_none()
                && tokio::time::Instant::now() < job.deadline
            {
                job.failure_detail = None;
                job.park_until = Some(tokio::time::Instant::now() + Duration::from_millis(300));
            }
            return;
        }
        let room = self.deferred_count(&binding) < MAX_DEFERRED_PER_SESSION;
        let approximate = self.approximate(job).await;
        let (reply, definition) = match approximate {
            Some(answer) => {
                let mut text = answer.text;
                if !room {
                    text.push_str(&format!(
                        "precise answer not scheduled: {MAX_DEFERRED_PER_SESSION} already pending for this session\n"
                    ));
                }
                let kind = if job.tool == AssistanceTool::Graph {
                    ResultKind::Graph
                } else {
                    ResultKind::Symbol
                };
                (
                    PeerReply::Complete {
                        kind,
                        text,
                        detail_ref: Some(job.reference.clone()),
                        truncated: false,
                        continuation: false,
                    },
                    Some(answer.definition),
                )
            }
            None if room => (
                PeerReply::Pending {
                    detail_ref: job.reference.clone(),
                },
                None,
            ),
            None => return,
        };
        let Some(sender) = job.stop_reply.take() else {
            return;
        };
        if sender.send(reply).is_err() {
            return;
        }
        let now = tokio::time::Instant::now();
        if !room {
            // Nothing follows: the job ends at its next retry and its waiter is already answered.
            job.deadline = now;
            job.park_until = Some(now);
            return;
        }
        job.deadline = now + PRECISE_BUDGET;
        job.failure_detail = None;
        if job.park_until.is_none() {
            job.park_until = Some(now + Duration::from_millis(300));
        }
        self.deferred_precise
            .insert(job.reference.clone(), binding.clone());
        let fence = match definition {
            Some(file) => self.fence_of(&binding, &file),
            None => Vec::new(),
        };
        let spec = DeferSpec {
            tool: job.tool,
            target: bounded_target(job.parameters["symbol"].as_str().unwrap_or_default()),
            fence_paths: fence,
        };
        SessionOutbox::defer(&*self.shared, &job.reference, spec);
    }

    /// How many jobs of `binding` answered early and still wait for their precise answer; jobs
    /// whose retained result settled (or was dropped) are forgotten here.
    pub(super) fn deferred_count(&mut self, binding: &BindingRef) -> usize {
        let pending: BTreeSet<String> = self
            .shared
            .ledger
            .lock()
            .map(|ledger| {
                ledger
                    .details
                    .iter()
                    .filter(|(_, detail)| matches!(detail.reply, PeerReply::Pending { .. }))
                    .map(|(reference, _)| reference.clone())
                    .collect()
            })
            .unwrap_or_default();
        self.deferred_precise
            .retain(|reference, _| pending.contains(reference));
        self.deferred_precise
            .values()
            .filter(|owner| *owner == binding)
            .count()
    }

    /// The outbox fence of a precise answer built from `file`: that path with the digest of its
    /// current bytes, so only an edit of the definition file discards the answer.
    pub(super) fn fence_of(&self, binding: &BindingRef, file: &Path) -> Vec<FencedPath> {
        let Some(receipt) = self.grants.get(binding) else {
            return Vec::new();
        };
        let digest = std::fs::read(receipt.worktree().worktree_path().join(file))
            .ok()
            .map(|bytes| *blake3::hash(&bytes).as_bytes());
        vec![FencedPath {
            path: file.display().to_string(),
            digest,
        }]
    }

    /// The approximate answer of `job`, or `None` when its language's source outline is not
    /// exact, the address does not resolve lexically to exactly one definition, or the file
    /// cannot be read.
    pub(super) async fn approximate(&mut self, job: &mut Job) -> Option<Approximate> {
        let binding = job.invocation.binding_ref().clone();
        let requested = job.parameters["symbol"].as_str()?.to_owned();
        let symbol = SymbolPath::parse(&requested).ok()?;
        let authority = self.authority(&binding).await.ok()?;
        let root = authority.worktree().worktree_path().to_path_buf();
        let (file, found) = match symbol.file() {
            Some(file) => {
                let language = exact_language(file)?;
                let source = read_source(&authority, file)?;
                let outline = source_outline(language, &root, file, &source).await?;
                let found = outline.find(&symbol).cloned()?;
                (file.to_path_buf(), found)
            }
            None => self.locate_lexically(&authority, symbol.name()?).await?,
        };
        let language = exact_language(&file)?;
        let server = language
            .server()
            .map_or("the language server", |server| server.name());
        let scan = scan_names(&authority, language, &found.name, (&file, found.body.start));
        let text = if job.tool == AssistanceTool::Graph {
            self.approximate_graph(job, &authority, language, &file, &found, &scan)
                .await
        } else {
            self.approximate_card(job, &authority, language, &file, &found, &scan)
                .await
        };
        Some(Approximate {
            text: format!(
                "approximate: {server} is still indexing; usages and callers below are name \
                 matches from source, marked ~; the precise answer is retained under its reference; \
                 Bridge delivers it automatically when configured\n{text}"
            ),
            definition: file,
        })
    }

    /// Resolves a bare `name` from the source outlines of the worktree's files of languages whose
    /// source outline is exact, collapsing a declaration and its impl blocks; `None` unless
    /// exactly one definition remains.
    async fn locate_lexically(
        &mut self,
        authority: &AuthorityStamp,
        name: &str,
    ) -> Option<(PathBuf, lang::Symbol)> {
        let root = authority.worktree().worktree_path().to_path_buf();
        let started = std::time::Instant::now();
        let mut found: Vec<(PathBuf, lang::Symbol)> = Vec::new();
        for file in crate::intelligence::names::listed_files(&root)
            .into_iter()
            .take(MAX_SCANNED_FILES)
        {
            if started.elapsed() > SCAN_BUDGET {
                return None;
            }
            let Some(language) = exact_language(&file) else {
                continue;
            };
            let Some(source) = read_source(authority, &file) else {
                continue;
            };
            if !contains_bare_name(source.as_bytes(), name.as_bytes()) {
                continue;
            }
            let Some(outline) = source_outline(language, &root, &file, &source).await else {
                continue;
            };
            for candidate in collapse_named(outline.named(name)) {
                found.push((file.clone(), candidate.clone()));
                if found.len() > 1 {
                    return None;
                }
            }
        }
        found.pop()
    }

    /// The approximate symbol card text of `found` in `file`: definition facts from the source
    /// outline, lexical usages (construction sites tagged for a type), lexical callers, callees
    /// left to the precise answer, and the Git history when asked.
    async fn approximate_card(
        &mut self,
        job: &Job,
        authority: &AuthorityStamp,
        language: Lang,
        file: &Path,
        found: &lang::Symbol,
        scan: &Scan,
    ) -> String {
        let root = authority.worktree().worktree_path().to_path_buf();
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
        if want_usages {
            let mut usages: Vec<Usage> = scan
                .found
                .iter()
                .map(|occurrence| Usage {
                    file: occurrence.file.display().to_string(),
                    line: occurrence.line,
                    text: occurrence.text.clone(),
                    is_test: language.support().is_test_file(&occurrence.file),
                    tag: None,
                })
                .collect();
            if constructs(found.kind) {
                classify_constructions(
                    &mut usages,
                    &found.name,
                    found.kind == lang::SymbolKind::Enum,
                );
            }
            for usage in &mut usages {
                if usage.tag.is_none() {
                    usage.tag = Some("~".to_owned());
                }
            }
            if usages.is_empty() {
                card.usages_note = Some("0 name matches in source".to_owned());
            }
            card.usages = usages;
            if let Some(bound) = scan.stopped {
                card.links
                    .push(format!("  (scan stopped: {bound}; partial)"));
            }
        }
        if callers_depth > 0 {
            let callers = self.lexical_callers(authority, language, scan, found).await;
            if callers.is_empty() {
                card.callers_note = Some("0 call matches in source".to_owned());
            }
            card.callers = callers
                .into_iter()
                .map(|(path, file, line)| Call {
                    name: format!("{path} ~"),
                    file: file.display().to_string(),
                    line,
                })
                .collect();
        }
        if callees_depth > 0 {
            card.callees_note = Some("follows with the precise answer".to_owned());
        }
        if job
            .parameters
            .get("history")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            if authority.worktree().is_plain_directory() {
                card.history_unavailable = true;
            } else {
                card.history = history_lines(&root, file, found.range).await;
            }
        }
        // The card prints its usage ceiling and `… N more`; the full list follows precisely.
        render::symbol_card_text(&card)
    }

    /// The approximate depth-1 callers graph text of `found`: one node per enclosing callable of
    /// a lexical call site, tagged `~`, and the precise answer named for the rest.
    async fn approximate_graph(
        &mut self,
        job: &Job,
        authority: &AuthorityStamp,
        language: Lang,
        file: &Path,
        found: &lang::Symbol,
        scan: &Scan,
    ) -> String {
        let depth = job
            .parameters
            .get("depth")
            .and_then(Value::as_u64)
            .unwrap_or(2);
        let direction = job
            .parameters
            .get("direction")
            .and_then(Value::as_str)
            .unwrap_or("callers");
        let mut graph = render::CallGraph::new(render::GraphNode {
            path: found.path.to_string(),
            file: file.display().to_string(),
            line: found.range.start,
            is_test: found.kind == lang::SymbolKind::Test,
            tag: None,
        });
        if direction != "callees" {
            for (path, caller_file, line) in
                self.lexical_callers(authority, language, scan, found).await
            {
                let Some(index) = graph.add_node(render::GraphNode {
                    path,
                    file: caller_file.display().to_string(),
                    line,
                    is_test: language.support().is_test_file(&caller_file),
                    tag: Some("~".to_owned()),
                }) else {
                    break;
                };
                graph.add_edge(render::GraphEdge {
                    from: 0,
                    to: index,
                    direction: render::GraphDirection::Callers,
                    link: false,
                });
            }
        }
        let body = render::call_graph_text(&graph, "callers", 1);
        let mut text = format!(
            "graph: callers of {} (approximate, depth 1 of {depth}, {} nodes, {} edges)\n",
            found.path,
            graph.nodes.len(),
            graph.edges.len()
        );
        text.push_str(body.split_once('\n').map_or("", |(_, rows)| rows));
        if direction != "callers" {
            text.push_str("callees: follows with the precise answer\n");
        }
        if let Some(bound) = scan.stopped {
            text.push_str(&format!("(scan stopped: {bound}; partial)\n"));
        }
        text
    }

    /// The enclosing callables (function, method, constructor or test) of every scanned
    /// occurrence whose name is directly followed by `(` (after an optional `::<…>`), one entry
    /// per callable in scan order, as `(symbol path, file, first line)`. Outlines of at most
    /// [`MAX_CALLER_OUTLINES`] files are read.
    async fn lexical_callers(
        &mut self,
        authority: &AuthorityStamp,
        language: Lang,
        scan: &Scan,
        found: &lang::Symbol,
    ) -> Vec<(String, PathBuf, u32)> {
        let root = authority.worktree().worktree_path().to_path_buf();
        let mut outlines: BTreeMap<PathBuf, Option<Outline>> = BTreeMap::new();
        let mut callers: Vec<(String, PathBuf, u32)> = Vec::new();
        for occurrence in &scan.found {
            if !is_call_shaped(&occurrence.text, &found.name) {
                continue;
            }
            if !outlines.contains_key(&occurrence.file) {
                if outlines.len() >= MAX_CALLER_OUTLINES {
                    continue;
                }
                let outline = match read_source(authority, &occurrence.file) {
                    Some(source) => {
                        source_outline(language, &root, &occurrence.file, &source).await
                    }
                    None => None,
                };
                outlines.insert(occurrence.file.clone(), outline);
            }
            let Some(Some(outline)) = outlines.get(&occurrence.file) else {
                continue;
            };
            let Some(enclosing) = innermost(outline, occurrence.line).filter(|symbol| {
                matches!(
                    symbol.kind,
                    lang::SymbolKind::Function
                        | lang::SymbolKind::Method
                        | lang::SymbolKind::Constructor
                        | lang::SymbolKind::Test
                )
            }) else {
                continue;
            };
            let path = enclosing.path.to_string();
            if !callers.iter().any(|(known, ..)| *known == path) {
                callers.push((path, occurrence.file.clone(), enclosing.range.start));
            }
        }
        callers
    }
}

/// `requested` cut to the outbox's target bound on a character boundary.
fn bounded_target(requested: &str) -> String {
    let mut end = requested.len().min(MAX_TARGET_BYTES);
    while !requested.is_char_boundary(end) {
        end -= 1;
    }
    requested[..end].to_owned()
}

/// The language of `file` when its source outline is exact (it may answer while the server
/// loads); `None` for any other file.
fn exact_language(file: &Path) -> Option<Lang> {
    Lang::for_path(file).filter(|language| language.support().outline_while_loading())
}

/// The text of one worktree file read under `authority` (bounded like every source read), or
/// `None` when it cannot be read or is not UTF-8.
fn read_source(authority: &AuthorityStamp, file: &Path) -> Option<String> {
    let limits = SourceReadLimits::new(1024, MAX_SOURCE_BYTES).ok()?;
    let read = read_authorized_source(authority.worktree(), file, limits).ok()?;
    String::from_utf8(read.contents().to_vec()).ok()
}

/// The source outline of `file`, or `None` when the language cannot outline it.
async fn source_outline(language: Lang, root: &Path, file: &Path, source: &str) -> Option<Outline> {
    crate::modules::calls::outline_from_source(language, root, file, source)
        .await
        .ok()
        .flatten()
}

/// Every whole-word occurrence of `name` in the worktree's files of `language`, the definition
/// line `definition` excluded, within [`SCAN_BUDGET`] and [`MAX_SCANNED_FILES`].
fn scan_names(
    authority: &AuthorityStamp,
    language: Lang,
    name: &str,
    definition: (&Path, u32),
) -> Scan {
    let root = authority.worktree().worktree_path();
    let started = std::time::Instant::now();
    let mut scan = Scan {
        found: Vec::new(),
        stopped: None,
    };
    let listed = crate::intelligence::names::listed_files(root);
    if listed.len() > MAX_SCANNED_FILES {
        scan.stopped = Some("20000 files");
    }
    for file in listed.into_iter().take(MAX_SCANNED_FILES) {
        if started.elapsed() > SCAN_BUDGET {
            scan.stopped = Some("2 s budget");
            break;
        }
        if Lang::for_path(&file) != Some(language) {
            continue;
        }
        let Some(source) = read_source(authority, &file) else {
            continue;
        };
        if !contains_bare_name(source.as_bytes(), name.as_bytes()) {
            continue;
        }
        for (index, line) in source.lines().enumerate() {
            let number = index as u32 + 1;
            if (file.as_path(), number) == definition
                || !contains_bare_name(line.as_bytes(), name.as_bytes())
            {
                continue;
            }
            scan.found.push(Occurrence {
                file: file.clone(),
                line: number,
                text: render::line_text(&source, number),
            });
        }
    }
    scan
}

/// Whether `line` calls `name`: a whole-word occurrence directly followed (after an optional
/// `::<…>` turbofish) by `(`, on a line that does not declare it.
fn is_call_shaped(line: &str, name: &str) -> bool {
    let trimmed = line.trim_start();
    if ["fn ", "pub fn ", "def ", "function "]
        .iter()
        .any(|keyword| trimmed.starts_with(keyword) && trimmed.contains(&format!("{name}(")))
    {
        return false;
    }
    line.match_indices(name).any(|(at, _)| {
        let before = line[..at].chars().next_back();
        let boundary = before.is_none_or(|ch| !(ch.is_alphanumeric() || ch == '_'));
        let rest = &line[at + name.len()..];
        let rest = if rest.starts_with("::<") {
            skip_generics(rest)
        } else {
            rest
        };
        boundary && rest.starts_with('(')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call is the name followed by `(`; a declaration, a longer identifier or a bare mention
    /// is not.
    #[test]
    fn call_shapes_are_recognized_lexically() {
        assert!(is_call_shaped("    let x = parse(input);", "parse"));
        assert!(is_call_shaped("items.iter().map(parse)", "map"));
        assert!(is_call_shaped("let v = collect::<Vec<_>>();", "collect"));
        assert!(!is_call_shaped("fn parse(input: &str) {", "parse"));
        assert!(!is_call_shaped("pub fn parse(input: &str) {", "parse"));
        assert!(!is_call_shaped("let reparse = 1; reparse(2)", "parse"));
        assert!(!is_call_shaped("use crate::parse;", "parse"));
    }

    /// Long targets are cut on a character boundary to the outbox bound.
    #[test]
    fn targets_fit_the_outbox_bound() {
        let long = format!("{}é", "a".repeat(MAX_TARGET_BYTES - 1));
        let cut = bounded_target(&long);
        assert!(cut.len() <= MAX_TARGET_BYTES);
        assert_eq!(bounded_target("src/a.rs#f"), "src/a.rs#f");
    }
}
