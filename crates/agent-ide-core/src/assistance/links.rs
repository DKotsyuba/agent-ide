//! Worker glue for the cross-language name index (the language bridge).
//!
//! The index work is blocking (listing, `lstat` sweeps, authorized reads), so it runs on the
//! blocking pool like the project card. A query that finds the index still building parks its
//! job and retries through the ordinary provider-loading path, so a build that finishes within the
//! inline reply wait is invisible to the agent.
//!
//! What the symbol tools show from the index: the `defines:` lines, tagged index-backed usages
//! and the `links:` section of a symbol card; the name card of a sigil address (`.btn`, `##main`,
//! `--brand`) or of a bare name only the index knows; the bridge candidates of an ambiguity list;
//! and the definition an `ide.read` of a sigil address reads. Every displayed row is read and
//! verified first ([`NameIndex::proven_sites`]); counts are indexed, not live.

use super::*;
use crate::intelligence::names::{IndexState, KeySummary, NameIndex, Proven, ShownSite};
use crate::lang::{
    Language as Lang, LineRange, Symbol,
    names::{Certainty, NameKey, Namespace, Role, ns},
    render::{self, SymbolCard, Usage},
};
use crate::modules::contract::ModuleUnavailable;
use crate::workspace::authority::WorktreeRef;
use std::collections::HashMap;

/// Longest one query sweeps before it parks its job.
const NAMES_QUERY_WAIT: Duration = Duration::from_secs(1);
/// Delay before a parked query retries a building index.
const NAMES_PARK: Duration = Duration::from_millis(300);
/// Index rows one key shows (and proves); the rest are counted.
const MAX_LINK_ROWS: usize = 2_000;
/// Other definitions a `defines:` line names.
const MAX_ALSO: usize = 5;
/// Keys a `links:` section lists.
const MAX_LINK_KEYS: usize = 20;
/// Definitions one `links:` row names.
const MAX_LINK_DEFINITIONS: usize = 2;
/// Characters of a definition's text shown after its location.
const MAX_SELECTOR_CHARS: usize = 60;

/// A bare address with a namespace sigil (`.btn`, `##main`, `--brand`): its namespace and name.
pub(super) fn sigil_address(requested: &str) -> Option<(Namespace, &str)> {
    let requested = requested.trim();
    let mut namespaces = ns::ALL;
    // The longest sigil first, so `##main` is never read as a one-character sigil.
    namespaces.sort_by_key(|namespace| std::cmp::Reverse(namespace.sigil().map_or(0, str::len)));
    namespaces.into_iter().find_map(|namespace| {
        let name = requested.strip_prefix(namespace.sigil()?)?;
        (!name.is_empty() && !name.contains(['#', '/', ' '])).then_some((namespace, name))
    })
}

/// Whether any registered language states name facts.
fn bridged() -> bool {
    crate::lang::registered()
        .iter()
        .any(|language| language.names().is_some())
}

/// Whether the bounded presence walk of `root` finds a file of a language that defines names
/// (a bare name can only name a key something defines; languages that only use names, such as
/// scripts, do not make a project bridged on their own).
pub(super) fn bridged_files_present(root: &Path) -> bool {
    let extensions: Vec<&str> = crate::lang::registered()
        .iter()
        .filter(|language| {
            language
                .names()
                .is_some_and(|names| names.coverage().iter().any(|coverage| coverage.defines))
        })
        .flat_map(|language| language.descriptor().extensions.iter().copied())
        .collect();
    crate::lang::text::has_files_with(root, &extensions)
}

/// Facts `file`'s own language states on `range` of `bytes` (none without a provider). A
/// language that computes in its module is asked on a blocking-pool thread, every other one
/// inline.
///
/// # Errors
///
/// The module's typed fault when it cannot answer: a card never reads a faulted module as
/// "no links".
async fn local_facts(
    worktree: &Path,
    file: &Path,
    bytes: &[u8],
    range: LineRange,
) -> Result<Vec<crate::lang::names::NameFact>, ModuleUnavailable> {
    let Some(language) = Lang::for_path(file) else {
        return Ok(Vec::new());
    };
    let (worktree, file, bytes) = (worktree.to_path_buf(), file.to_path_buf(), bytes.to_vec());
    let facts = move || crate::intelligence::names::facts_of(&worktree, language, &file, &bytes);
    let facts = if crate::intelligence::anchors::routed(language).is_some() {
        tokio::task::spawn_blocking(facts)
            .await
            .unwrap_or(Ok(None))?
    } else {
        facts()?
    };
    Ok(facts
        .unwrap_or_default()
        .into_iter()
        .filter(|fact| range.start <= fact.line && fact.line <= range.end)
        .collect())
}

/// Whether `file`'s own language states a name fact on `range` of `bytes`.
async fn has_facts_in(
    worktree: &Path,
    file: &Path,
    bytes: &[u8],
    range: LineRange,
) -> Result<bool, ModuleUnavailable> {
    Ok(!local_facts(worktree, file, bytes, range).await?.is_empty())
}

/// Keys `found` defines itself (its children's definitions excluded), from `bytes`.
async fn owned_keys(
    worktree: &Path,
    file: &Path,
    bytes: &[u8],
    found: &Symbol,
) -> Result<BTreeSet<NameKey>, ModuleUnavailable> {
    Ok(local_facts(worktree, file, bytes, found.range)
        .await?
        .into_iter()
        .filter(|fact| fact.role == Role::Define)
        .filter(|fact| {
            !found
                .children
                .iter()
                .any(|child| child.range.start <= fact.line && fact.line <= child.range.end)
        })
        .map(|fact| fact.key)
        .collect())
}

/// A name a graph node uses: its display (`.btn`), label and first indexed definition.
pub(super) struct LinkTarget {
    /// `sigil + name`.
    pub display: String,
    /// `class name`, `element id`, ….
    pub label: &'static str,
    /// Definition word of the namespace (`rule`), for undefined names.
    pub define_word: &'static str,
    /// File and line of the first indexed definition.
    pub definition: Option<(PathBuf, u32)>,
    /// For a file reference, the assumption that reached `definition` (`extension probe`); a
    /// reference that names its file exactly, and every other kind, has none.
    pub uncertainty: Option<&'static str>,
}

/// `sigil + name` of a key, with its domain when scoped.
fn display(key: &NameKey) -> String {
    let mut text = format!("{}{}", key.namespace.sigil().unwrap_or(""), key.name);
    if !key.domain.is_empty() {
        text.push_str(&format!(" in {}", key.domain));
    }
    text
}

/// A definition line shortened to its selector: text before a trailing `/* … */` comment and
/// before a rule body, clipped.
fn selector(text: &str) -> String {
    let text = match text
        .trim_end()
        .strip_suffix("*/")
        .and_then(|rest| rest.rsplit_once("/*"))
    {
        Some((before, _)) => before.trim_end(),
        None => text,
    };
    let text = if text.ends_with('{') || text.ends_with('}') {
        text.split('{').next().unwrap_or(text).trim_end()
    } else {
        text
    };
    render::clip(text, MAX_SELECTOR_CHARS)
}

/// `count word` with a plural `s` unless the count is one.
fn counted(count: usize, word: &str) -> String {
    if count == 1 {
        format!("1 {word}")
    } else {
        format!("{count} {word}s")
    }
}

/// The assumption behind a file-reference target (`extension probe`, `index probe`, …), shown so
/// an inferred link never reads as exact; none for an exact reference and for every legacy kind,
/// whose wording stays as it was.
fn inferred(key: &NameKey, shown: &ShownSite) -> Option<&'static str> {
    match shown.site.fact.certainty {
        Certainty::Heuristic(reason) if key.namespace.path_keys() => Some(reason),
        _ => None,
    }
}

/// `file:line` of a site.
fn location(shown: &ShownSite) -> String {
    format!("{}:{}", shown.site.file.display(), shown.site.fact.line)
}

/// The tagged usage row of a shown site; `key` is added to the tag when a card mixes keys.
/// `is_test` is the site's language's own test-file verdict for its file ([`test_flags`]).
fn usage(shown: &ShownSite, key: Option<&NameKey>, is_test: bool) -> Usage {
    let mut tag = format!("[{}", shown.site.language);
    if let Certainty::Heuristic(reason) = shown.site.fact.certainty {
        tag.push_str(&format!(" ~{reason}"));
    }
    if let Some(key) = key {
        tag.push_str(&format!(" {}", display(key)));
    }
    tag.push(']');
    Usage {
        file: shown.site.file.display().to_string(),
        line: shown.site.fact.line,
        text: shown.text.clone(),
        is_test,
        tag: Some(tag),
    }
}

/// The index-state and coverage lines a bridge reply ends with.
fn notes(uncovered: &BTreeSet<Lang>, state: IndexState, capped: usize) -> Vec<String> {
    let mut lines = Vec::new();
    if !uncovered.is_empty() {
        let ids: Vec<&str> = uncovered.iter().map(|language| language.name()).collect();
        lines.push(format!("unavailable for: {}", ids.join(", ")));
    }
    if let IndexState::Partial { indexed, listed } = state {
        lines.push(format!(
            "links: partial (indexed {indexed} of {listed} files); counts are indexed, not live"
        ));
    }
    if capped > 0 {
        lines.push(format!(
            "links: {} hit the per-file limit of {} facts; counts are lower bounds",
            counted(capped, "file"),
            crate::lang::names::MAX_FACTS_PER_FILE
        ));
    }
    lines
}

/// Whether each of `files` (with its language) is a test file by that language's own rule, asked
/// through the module-aware facade (memoized per module build) and never on the blocking pool.
///
/// # Errors
///
/// The module's typed fault.
async fn test_flags(
    worktree: &Path,
    files: Vec<(Lang, PathBuf)>,
) -> Result<HashMap<PathBuf, bool>, ModuleUnavailable> {
    let mut flags = HashMap::new();
    for (language, file) in files {
        if let std::collections::hash_map::Entry::Vacant(slot) = flags.entry(file) {
            let facts = crate::modules::calls::test_facts(language, worktree, slot.key()).await?;
            slot.insert(facts.is_test_file);
        }
    }
    Ok(flags)
}

/// The language and file of each site among `shown`.
fn site_files<'a>(shown: impl IntoIterator<Item = &'a ShownSite>) -> Vec<(Lang, PathBuf)> {
    shown
        .into_iter()
        .map(|shown| (shown.site.language, shown.site.file.clone()))
        .collect()
}

/// The innermost outline symbol holding `shown`'s line in `source` (the file's current text, read
/// by the index), as `path (lines a–b)`, when its language outlines from source; the outline is
/// computed through the module-aware facade.
///
/// # Errors
///
/// The module's typed fault.
async fn address(
    worktree: &Path,
    shown: &ShownSite,
    source: Option<&str>,
) -> Result<Option<(String, LineRange)>, ModuleUnavailable> {
    let Some(source) = source else {
        return Ok(None);
    };
    let outline = crate::modules::calls::outline_from_source(
        shown.site.language,
        worktree,
        &shown.site.file,
        source,
    )
    .await?;
    Ok(outline.and_then(|outline| {
        super::symbols::innermost(&outline, shown.site.fact.line)
            .map(|symbol| (symbol.path.to_string(), symbol.range))
    }))
}

/// What a symbol card shows from the index, gathered on the blocking pool.
struct CardLinks {
    /// Keys the symbol defines, with their definitions and uses.
    defined: Vec<(NameKey, Proven, Proven)>,
    /// Keys the symbol uses (at most [`MAX_LINK_KEYS`]), with their definitions.
    linked: Vec<(NameKey, Proven)>,
    /// Distinct keys the symbol uses.
    linked_total: usize,
    /// Present languages that cannot state the involved namespaces.
    uncovered: BTreeSet<Lang>,
    /// Indexed files whose facts hit the per-file limit.
    capped: usize,
}

/// What a name card shows for one key, gathered on the blocking pool.
struct NameLinks {
    /// The key.
    key: NameKey,
    /// Its definitions with outline addresses where the language has them (filled after the
    /// gather, from [`NameLinks::sources`]).
    defines: Vec<(ShownSite, Option<(String, LineRange)>)>,
    /// The current text of each definition's file, read by the index, in `defines` order.
    sources: Vec<Option<String>>,
    /// Definitions dropped as unreadable.
    defines_dropped: usize,
    /// Its uses.
    uses: Proven,
    /// Present languages that cannot state the namespace.
    uncovered: BTreeSet<Lang>,
    /// Indexed files whose facts hit the per-file limit.
    capped: usize,
}

impl Worker<'_> {
    /// Refreshes `worktree`'s name index on the blocking pool for at most [`NAMES_QUERY_WAIT`]
    /// and returns it with its state.
    ///
    /// # Errors
    ///
    /// [`FailureCode::ProviderLoading`] (detail `names:building`) while the index is still
    /// building; the job is parked for [`NAMES_PARK`] when its deadline leaves room to retry.
    /// [`FailureCode::Internal`] when the blocking task or the index lock fails.
    pub(super) async fn name_index(
        &mut self,
        job: &mut Job,
        worktree: &WorktreeRef,
    ) -> Result<(Arc<Mutex<NameIndex>>, IndexState), FailureCode> {
        let index = self.names.for_worktree(worktree);
        let deadline = std::time::Instant::now() + NAMES_QUERY_WAIT;
        let telemetry = self.telemetry.clone();
        let held = index.clone();
        // A build already running (the activation prewarm) holds the index: the query parks
        // instead of waiting on the lock.
        let state = tokio::task::spawn_blocking(move || match held.try_lock() {
            Ok(mut index) => Some(timed_refresh(&mut index, deadline, telemetry.as_ref())),
            Err(std::sync::TryLockError::WouldBlock) => None,
            Err(std::sync::TryLockError::Poisoned(_)) => Some(IndexState::Building),
        })
        .await
        .map_err(|_| FailureCode::Internal)?
        .unwrap_or(IndexState::Building);
        if state == IndexState::Building {
            let now = tokio::time::Instant::now();
            if job.deadline.saturating_duration_since(now) > NAMES_QUERY_WAIT {
                job.park_until = Some(now + NAMES_PARK);
            }
            job.failure_detail = Some("names:building".to_owned());
            return Err(FailureCode::ProviderLoading);
        }
        Ok((index, state))
    }

    /// Starts building `worktree`'s name index on the blocking pool and returns at once (the
    /// activation prewarm). A query arriving before the build ends parks as `names:building`.
    pub(super) fn prewarm_names(&mut self, worktree: &WorktreeRef) {
        let index = self.names.for_worktree(worktree);
        let telemetry = self.telemetry.clone();
        tokio::task::spawn_blocking(move || {
            if let Ok(mut index) = index.try_lock() {
                let deadline = std::time::Instant::now() + crate::intelligence::names::BUILD_BUDGET;
                timed_refresh(&mut index, deadline, telemetry.as_ref());
            }
        });
    }

    /// Adds the index's view of `found` to its card: `defines:` lines and tagged usages for the
    /// names it defines, a `links:` section for the names it uses, `unavailable for:` and the
    /// index state. `bytes` are the observed bytes of `file`, proven against the index first.
    /// Nothing is added for a symbol without name facts.
    pub(super) async fn card_links(
        &mut self,
        job: &mut Job,
        worktree: &WorktreeRef,
        file: &Path,
        bytes: &[u8],
        found: &Symbol,
        card: &mut SymbolCard,
    ) -> Result<(), FailureCode> {
        let children: Vec<LineRange> = found.children.iter().map(|child| child.range).collect();
        self.span_links(job, worktree, file, bytes, found.range, children, card)
            .await
    }

    /// [`Self::card_links`] for the lines `range` of `file`, `children` being the nested ranges
    /// whose definitions belong to other cards.
    #[allow(
        clippy::too_many_arguments,
        reason = "one coherent span of one observed file"
    )]
    async fn span_links(
        &mut self,
        job: &mut Job,
        worktree: &WorktreeRef,
        file: &Path,
        bytes: &[u8],
        range: LineRange,
        children: Vec<LineRange>,
        card: &mut SymbolCard,
    ) -> Result<(), FailureCode> {
        // The file's own facts decide first, from the observed bytes: a symbol without name facts
        // (most code) never touches the index, so its card costs nothing extra.
        if !has_facts_in(worktree.worktree_path(), file, bytes, range)
            .await
            .map_err(|failure| super::symbols::module_failure(job, &failure))?
        {
            return Ok(());
        }
        let (index, state) = self.name_index(job, worktree).await?;
        let own_file = file.to_path_buf();
        let language = Lang::for_path(file);
        let (file, bytes) = (file.to_path_buf(), bytes.to_vec());
        let gathered = with_names(index, move |index| {
            index.verify(&file, &bytes);
            let facts = index.facts_in(&file, range);
            let owned: BTreeSet<NameKey> = facts
                .iter()
                .filter(|site| site.fact.role == Role::Define)
                .filter(|site| {
                    let line = site.fact.line;
                    !children
                        .iter()
                        .any(|child| child.start <= line && line <= child.end)
                })
                .map(|site| site.fact.key.clone())
                .collect();
            let used: BTreeSet<NameKey> = facts
                .iter()
                .filter(|site| site.fact.role == Role::Use)
                .map(|site| site.fact.key.clone())
                .collect();
            let namespaces: BTreeSet<Namespace> = if owned.is_empty() { &used } else { &owned }
                .iter()
                .map(|key| key.namespace)
                .collect();
            CardLinks {
                defined: owned
                    .iter()
                    .map(|key| {
                        let defines = index.proven_sites(key, Some(Role::Define), MAX_LINK_ROWS);
                        let uses = index.proven_sites(key, Some(Role::Use), MAX_LINK_ROWS);
                        (key.clone(), defines, uses)
                    })
                    .collect(),
                linked: used
                    .iter()
                    .take(MAX_LINK_KEYS)
                    .map(|key| {
                        let defines = match language {
                            Some(language) => {
                                index.proven_definitions(language, key, MAX_LINK_DEFINITIONS)
                            }
                            None => {
                                index.proven_sites(key, Some(Role::Define), MAX_LINK_DEFINITIONS)
                            }
                        };
                        (key.clone(), defines)
                    })
                    .collect(),
                linked_total: used.len(),
                uncovered: namespaces
                    .into_iter()
                    .flat_map(|namespace| index.uncovered(namespace))
                    .collect(),
                capped: index.capped_files(),
            }
        })
        .await?;
        if gathered.defined.is_empty() && gathered.linked.is_empty() {
            return Ok(());
        }
        let flags = test_flags(
            worktree.worktree_path(),
            site_files(gathered.defined.iter().flat_map(|(_, _, uses)| &uses.sites)),
        )
        .await
        .map_err(|failure| super::symbols::module_failure(job, &failure))?;
        let own = |shown: &ShownSite| {
            shown.site.file == own_file
                && range.start <= shown.site.fact.line
                && shown.site.fact.line <= range.end
        };
        let mixed = gathered.defined.len() > 1;
        let mut rows: Vec<(&ShownSite, Usage)> = Vec::new();
        let mut not_listed = 0;
        for (key, defines, uses) in &gathered.defined {
            let others: Vec<&ShownSite> =
                defines.sites.iter().filter(|shown| !own(shown)).collect();
            let mut line = format!("{} {}", key.namespace.label(), key.name);
            if !key.domain.is_empty() {
                line.push_str(&format!(" (in {})", key.domain));
            }
            if !others.is_empty() {
                let also: Vec<String> = others
                    .iter()
                    .take(MAX_ALSO)
                    .map(|shown| format!("{} {}", location(shown), selector(&shown.text)))
                    .collect();
                line.push_str(&format!(" — also {}", also.join(", ")));
                if others.len() > MAX_ALSO {
                    line.push_str(&format!(" (+{} more)", others.len() - MAX_ALSO));
                }
            }
            card.defines.push(line);
            rows.extend(uses.sites.iter().map(|shown| {
                (
                    shown,
                    usage(shown, mixed.then_some(key), flags[&shown.site.file]),
                )
            }));
            card.usages_dropped += uses.dropped;
            not_listed += uses.more;
        }
        if !gathered.defined.is_empty() {
            rows.sort_by(|(a, _), (b, _)| {
                (
                    a.site.file.as_os_str(),
                    a.site.fact.line,
                    a.site.fact.column,
                )
                    .cmp(&(
                        b.site.file.as_os_str(),
                        b.site.fact.line,
                        b.site.fact.column,
                    ))
            });
            card.usages.extend(rows.into_iter().map(|(_, usage)| usage));
            card.usages_indexed = true;
            card.usages_note = None;
            card.report_empty_usages = true;
            if not_listed > 0 {
                card.links
                    .push(format!("  … {not_listed} more indexed usages not listed"));
            }
        }
        if !gathered.linked.is_empty() {
            let namespaces: BTreeSet<Namespace> = gathered
                .linked
                .iter()
                .map(|(key, _)| key.namespace)
                .collect();
            let what = match namespaces.iter().next() {
                Some(namespace) if namespaces.len() == 1 => {
                    counted(gathered.linked_total, namespace.label())
                }
                _ => counted(gathered.linked_total, "name"),
            };
            card.links.push(format!("links: {what} used here"));
            let width = gathered
                .linked
                .iter()
                .map(|(key, _)| display(key).chars().count())
                .max()
                .unwrap_or(0);
            for (key, defines) in &gathered.linked {
                let target = if defines.sites.is_empty() {
                    format!("no indexed {}", key.namespace.define_word())
                } else {
                    let mut target = defines
                        .sites
                        .iter()
                        .map(|shown| {
                            let mut row = format!("{} {}", location(shown), selector(&shown.text));
                            if let Some(reason) = inferred(key, shown) {
                                row.push_str(&format!(" (~{reason})"));
                            }
                            row
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    if defines.more > 0 {
                        target.push_str(&format!(" (+{} more)", defines.more));
                    }
                    target
                };
                card.links
                    .push(format!("  {:<width$}  → {target}", display(key)));
            }
            if gathered.linked_total > MAX_LINK_KEYS {
                card.links.push(format!(
                    "  … {} more keys",
                    gathered.linked_total - MAX_LINK_KEYS
                ));
            }
        }
        card.links
            .extend(notes(&gathered.uncovered, state, gathered.capped));
        Ok(())
    }

    /// The `related_links:` block of an `ide.context` reply for `lines` of `file` (the whole file
    /// or the line under the requested position): the cross-language names defined there with
    /// their indexed usages, the names used there with their definitions, and the same
    /// `unavailable for:`/index notes as a symbol card, bounded by the card's ceilings. Empty when
    /// the range has no name facts. The block is advisory: an index that cannot answer (still
    /// building, a faulted worktree) says so in one line instead of failing the context.
    pub(super) async fn related_links(
        &mut self,
        job: &mut Job,
        worktree: &WorktreeRef,
        file: &Path,
        bytes: &[u8],
        lines: LineRange,
    ) -> String {
        let mut card = SymbolCard::default();
        if let Err(code) = self
            .span_links(job, worktree, file, bytes, lines, Vec::new(), &mut card)
            .await
        {
            job.failure_detail = None;
            return format!(
                "related_links: unavailable ({code:?}); ide.symbol on a symbol lists its links\n"
            );
        }
        // Rows the card cut after its ceiling stay in the block: the reply's ordinary paging
        // (`detail_ref`) carries them, the source bytes after the header stay untouched.
        let hidden = render::hidden_usages_text(&card);
        let text = render::symbol_card_text(&card);
        // The card's first line is its heading; the block keeps the rows under its own.
        let rows = text.split_once('\n').map_or("", |(_, rows)| rows);
        // The card's own marker line, matched as a whole line: a usage row's snippet may end in
        // the same words.
        let cut = format!(
            "  … {} more",
            card.usages.len().saturating_sub(render::MAX_USAGE_LINES)
        );
        let mut rows_out = String::new();
        let mut spliced = false;
        for line in rows.lines() {
            match hidden.as_deref().and_then(|text| text.split_once('\n')) {
                Some((_, more)) if !spliced && line == cut => {
                    rows_out.push_str(more);
                    spliced = true;
                }
                _ => {
                    rows_out.push_str(line);
                    rows_out.push('\n');
                }
            }
        }
        let rows = rows_out;
        if rows.is_empty() {
            return String::new();
        }
        let mut block = String::from("related_links:\n");
        for row in rows.lines() {
            block.push_str("  ");
            block.push_str(row);
            block.push('\n');
        }
        block
    }

    /// Use sites (file, line, language) of the names `found` defines, in site order, each proven
    /// against its file; empty without touching the index when it defines none.
    ///
    /// # Errors
    ///
    /// As [`Worker::name_index`].
    pub(super) async fn defined_use_sites(
        &mut self,
        job: &mut Job,
        worktree: &WorktreeRef,
        file: &Path,
        bytes: &[u8],
        found: &Symbol,
    ) -> Result<Vec<(PathBuf, u32, Lang)>, FailureCode> {
        let keys = owned_keys(worktree.worktree_path(), file, bytes, found)
            .await
            .map_err(|failure| super::symbols::module_failure(job, &failure))?;
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let (index, _) = self.name_index(job, worktree).await?;
        with_names(index, move |index| {
            let mut sites = Vec::new();
            for key in &keys {
                let proven = index.proven_sites(key, Some(Role::Use), MAX_LINK_ROWS);
                for shown in proven.sites {
                    let site = (shown.site.file, shown.site.fact.line, shown.site.language);
                    if !sites.contains(&site) {
                        sites.push(site);
                    }
                }
            }
            sites
        })
        .await
    }

    /// The names `range` of `file` uses (at most `limit`), with their first indexed definition;
    /// empty without touching the index when it uses none.
    ///
    /// # Errors
    ///
    /// As [`Worker::name_index`].
    pub(super) async fn used_names(
        &mut self,
        job: &mut Job,
        worktree: &WorktreeRef,
        file: &Path,
        bytes: &[u8],
        range: LineRange,
        limit: usize,
    ) -> Result<Vec<LinkTarget>, FailureCode> {
        let keys: BTreeSet<NameKey> = local_facts(worktree.worktree_path(), file, bytes, range)
            .await
            .map_err(|failure| super::symbols::module_failure(job, &failure))?
            .into_iter()
            .filter(|fact| fact.role == Role::Use)
            .map(|fact| fact.key)
            .collect();
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let language = Lang::for_path(file);
        let (index, _) = self.name_index(job, worktree).await?;
        with_names(index, move |index| {
            keys.iter()
                .take(limit)
                .map(|key| {
                    let proven = match language {
                        Some(language) => index.proven_definitions(language, key, 1),
                        None => index.proven_sites(key, Some(Role::Define), 1),
                    };
                    let first = proven.sites.first();
                    LinkTarget {
                        display: display(key),
                        label: key.namespace.label(),
                        define_word: key.namespace.define_word(),
                        definition: first
                            .map(|shown| (shown.site.file.clone(), shown.site.fact.line)),
                        uncertainty: first.and_then(|shown| inferred(key, shown)),
                    }
                })
                .collect()
        })
        .await
    }

    /// Indexed keys spelled `name` (in `only`, when given); empty when no registered language
    /// states name facts, without touching the index. A plain bare name (`only` is `None`) also
    /// skips the index while no index of the worktree exists and the bounded presence walk finds
    /// no file of a language with name facts, so projects without such files keep their
    /// bare-name latency.
    ///
    /// # Errors
    ///
    /// As [`Worker::name_index`], plus authority failures.
    pub(super) async fn indexed_keys(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        name: &str,
        only: Option<Namespace>,
    ) -> Result<Vec<KeySummary>, FailureCode> {
        if !bridged() {
            return Ok(Vec::new());
        }
        let authority = self.authority(binding).await?;
        if only.is_none()
            && !self.names.contains(authority.worktree())
            && !bridged_files_present(authority.worktree().worktree_path())
        {
            return Ok(Vec::new());
        }
        let (index, _) = self.name_index(job, authority.worktree()).await?;
        let name = name.to_owned();
        with_names(index, move |index| index.keys_named(&name, only)).await
    }

    /// The name card of `keys` (one section per key; normally one): definitions with their
    /// outline addresses, tagged usages, `unavailable for:` and the index state.
    pub(super) async fn name_card(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        keys: Vec<NameKey>,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let authority = self.authority(binding).await?;
        let (index, state) = self.name_index(job, authority.worktree()).await?;
        let gathered = with_names(index, move |index| {
            keys.into_iter()
                .map(|key| {
                    let defines = index.proven_sites(&key, Some(Role::Define), MAX_LINK_ROWS);
                    let uses = index.proven_sites(&key, Some(Role::Use), MAX_LINK_ROWS);
                    let sources = defines
                        .sites
                        .iter()
                        .map(|shown| index.source(&shown.site.file))
                        .collect();
                    NameLinks {
                        defines: defines
                            .sites
                            .into_iter()
                            .map(|shown| (shown, None))
                            .collect(),
                        sources,
                        defines_dropped: defines.dropped,
                        uses,
                        uncovered: index.uncovered(key.namespace).into_iter().collect(),
                        capped: index.capped_files(),
                        key,
                    }
                })
                .collect::<Vec<_>>()
        })
        .await?;
        let mut gathered = gathered;
        let worktree_path = authority.worktree().worktree_path();
        for links in &mut gathered {
            for ((shown, address_slot), source) in links.defines.iter_mut().zip(&links.sources) {
                *address_slot = address(worktree_path, shown, source.as_deref())
                    .await
                    .map_err(|failure| super::symbols::module_failure(job, &failure))?;
            }
        }
        let flags = test_flags(
            worktree_path,
            site_files(gathered.iter().flat_map(|links| &links.uses.sites)),
        )
        .await
        .map_err(|failure| super::symbols::module_failure(job, &failure))?;
        self.shared.active(binding)?;
        let mut head = String::new();
        let mut tail = String::new();
        for links in gathered {
            let key = &links.key;
            let languages: BTreeSet<Lang> = links
                .defines
                .iter()
                .map(|(shown, _)| shown.site.language)
                .chain(links.uses.sites.iter().map(|shown| shown.site.language))
                .collect();
            let files: BTreeSet<&PathBuf> = links
                .uses
                .sites
                .iter()
                .map(|shown| &shown.site.file)
                .collect();
            let mut heading = format!(
                "{} — {}, {}, {} in {} files",
                display(key),
                key.namespace.label(),
                counted(links.defines.len(), key.namespace.define_word()),
                counted(links.uses.sites.len(), "usage"),
                files.len()
            );
            if !languages.is_empty() {
                let ids: Vec<&str> = languages.iter().map(|language| language.name()).collect();
                heading.push_str(&format!(" ({})", ids.join(", ")));
            }
            let width = links
                .defines
                .iter()
                .map(|(shown, _)| location(shown).len())
                .max()
                .unwrap_or(0);
            let definitions = links
                .defines
                .iter()
                .map(|(shown, address)| {
                    let mut row = format!(
                        "{:<width$}  [{}] {}",
                        location(shown),
                        shown.site.language,
                        selector(&shown.text)
                    );
                    if let Some((path, range)) = address {
                        row.push_str(&format!("  ({path}, lines {range})"));
                    }
                    row
                })
                .collect();
            let mut card = SymbolCard {
                heading,
                definitions,
                usages: links
                    .uses
                    .sites
                    .iter()
                    .map(|shown| usage(shown, None, flags[&shown.site.file]))
                    .collect(),
                usages_indexed: true,
                usages_dropped: links.uses.dropped + links.defines_dropped,
                report_empty_usages: true,
                links: notes(&links.uncovered, state, links.capped),
                ..Default::default()
            };
            if links.uses.more > 0 {
                card.links.insert(
                    0,
                    format!("  … {} more indexed usages not listed", links.uses.more),
                );
            }
            if card.usages.len() > render::MAX_USAGE_LINES {
                card.more_detail = Some(job.reference.clone());
            }
            if !head.is_empty() {
                head.push('\n');
            }
            head.push_str(&render::symbol_card_text(&card));
            if let Some(hidden) = render::hidden_usages_text(&card) {
                tail.push_str(&hidden);
            }
        }
        let tail = (!tail.is_empty()).then_some(tail);
        let (reply, page) =
            ContextPageState::with_tail(head, tail, ResultKind::Symbol).next(&job.reference)?;
        self.shared.set_context_page(&job.reference, page);
        Ok((reply, Some(authority), None))
    }

    /// The first definition of a sigil address, for `ide.read`: its file, the lines of the
    /// innermost outline symbol holding it (the definition line alone without an outline) and
    /// the title to print.
    ///
    /// # Errors
    ///
    /// [`FailureCode::UnknownSymbol`] when the index holds no definition of the name; as
    /// [`Worker::name_index`] otherwise.
    pub(super) async fn sigil_definition(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        namespace: Namespace,
        name: &str,
    ) -> Result<(PathBuf, LineRange, String), FailureCode> {
        let authority = self.authority(binding).await?;
        let (index, _) = self.name_index(job, authority.worktree()).await?;
        let name = name.to_owned();
        let found = with_names(index, move |index| {
            let keys = index.keys_named(&name, Some(namespace));
            let key = keys
                .iter()
                .find(|summary| summary.key.domain.is_empty())
                .or(keys.first())?
                .key
                .clone();
            let proven = index.proven_sites(&key, Some(Role::Define), 1);
            let shown = proven.sites.into_iter().next()?;
            let source = index.source(&shown.site.file);
            Some((shown, source))
        })
        .await?;
        let Some((shown, source)) = found else {
            return Err(FailureCode::UnknownSymbol);
        };
        let line = shown.site.fact.line;
        Ok(
            match address(
                authority.worktree().worktree_path(),
                &shown,
                source.as_deref(),
            )
            .await
            .map_err(|failure| super::symbols::module_failure(job, &failure))?
            {
                Some((path, range)) => (shown.site.file.clone(), range, path),
                None => (
                    shown.site.file.clone(),
                    LineRange::new(line, line),
                    location(&shown),
                ),
            },
        )
    }
}

/// The bridge candidate line of an ambiguity list.
pub(super) fn candidate(summary: &KeySummary) -> String {
    format!(
        "{}  ({}: {}, {})",
        display(&summary.key),
        summary.key.namespace.label(),
        counted(summary.defines, summary.key.namespace.define_word()),
        counted(summary.uses, "usage")
    )
}

/// Refreshes `index` until `deadline` and records the refresh in telemetry when it read files or
/// reused cached facts.
fn timed_refresh(
    index: &mut NameIndex,
    deadline: std::time::Instant,
    telemetry: Option<&crate::telemetry::Telemetry>,
) -> IndexState {
    let started = std::time::Instant::now();
    let state = index.refresh(deadline);
    if index.last_reread() + index.last_reused() > 0
        && let Some(telemetry) = telemetry
    {
        crate::telemetry::adapters::name_index_refreshed(
            telemetry,
            state,
            index.summary(),
            index.last_reused(),
            started.elapsed(),
        );
    }
    state
}

/// Runs `query` against the locked index on the blocking pool (sites, `verify`, `uncovered`).
///
/// # Errors
///
/// [`FailureCode::Internal`] when the blocking task panics or the index lock is poisoned.
pub(super) async fn with_names<T: Send + 'static>(
    index: Arc<Mutex<NameIndex>>,
    query: impl FnOnce(&mut NameIndex) -> T + Send + 'static,
) -> Result<T, FailureCode> {
    tokio::task::spawn_blocking(move || {
        index
            .lock()
            .map(|mut index| query(&mut index))
            .map_err(|_| FailureCode::Internal)
    })
    .await
    .map_err(|_| FailureCode::Internal)?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sigil addresses pick their namespace (the longest sigil first) and reject compound text.
    #[test]
    fn sigil_addresses_pick_their_namespace() {
        assert_eq!(sigil_address(".btn"), Some((ns::CLASS, "btn")));
        assert_eq!(sigil_address("##main"), Some((ns::ELEMENT_ID, "main")));
        assert_eq!(
            sigil_address("--brand"),
            Some((ns::STYLE_VARIABLE, "brand"))
        );
        assert_eq!(sigil_address(".card .btn"), None);
        assert_eq!(sigil_address("btn"), None);
        assert_eq!(sigil_address("."), None);
        assert_eq!(selector(".layout .btn { margin: 0; }"), ".layout .btn");
        assert_eq!(selector("<a class=\"{{ x }}\">"), "<a class=\"{{ x }}\">");
        assert_eq!(selector("--accent: #f60; /* brand */"), "--accent: #f60;");
        assert_eq!(selector(".btn { /* base */"), ".btn");
    }

    /// A reply ends with its coverage, partial-index and per-file-limit lines, in that order.
    #[test]
    fn notes_disclose_coverage_partial_index_and_the_fact_limit() {
        assert!(notes(&BTreeSet::new(), IndexState::Ready, 0).is_empty());
        let notes = notes(
            &BTreeSet::new(),
            IndexState::Partial {
                indexed: 3,
                listed: 9,
            },
            2,
        );
        assert_eq!(
            notes,
            [
                "links: partial (indexed 3 of 9 files); counts are indexed, not live",
                "links: 2 files hit the per-file limit of 5000 facts; counts are lower bounds",
            ]
        );
    }

    /// A file-reference target reached by an assumption is labelled with it; an exact one, and
    /// an uncertain legacy kind, keep their wording.
    #[test]
    fn inferred_file_targets_are_labelled_and_legacy_rows_are_not() {
        use crate::intelligence::names::Site;
        use crate::lang::names::NameFact;
        let shown = |key: &NameKey, certainty| ShownSite {
            site: Site {
                file: PathBuf::from("a"),
                language: crate::lang::testing::ALPHA,
                fact: NameFact {
                    key: key.clone(),
                    role: Role::Define,
                    line: 1,
                    column: 1,
                    certainty,
                },
            },
            text: String::new(),
        };
        let file = NameKey::global(ns::FILE_REF, "src/x");
        let class = NameKey::global(ns::CLASS, "btn");
        let probe = Certainty::Heuristic("extension probe");
        assert_eq!(
            inferred(&file, &shown(&file, probe)),
            Some("extension probe")
        );
        assert_eq!(inferred(&file, &shown(&file, Certainty::Exact)), None);
        assert_eq!(
            inferred(&class, &shown(&class, Certainty::Heuristic("nested"))),
            None
        );
    }
}
