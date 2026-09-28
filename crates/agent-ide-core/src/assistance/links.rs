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
use crate::workspace::authority::WorktreeRef;

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

/// Whether `file`'s own language states a name fact on `range` of `bytes`.
fn has_facts_in(file: &Path, bytes: &[u8], range: LineRange) -> bool {
    let (Some(names), Ok(source)) = (
        Lang::for_path(file).and_then(Lang::names),
        std::str::from_utf8(bytes),
    ) else {
        return false;
    };
    let mut sink = crate::lang::names::FactSink::new();
    names.extract(file, source, &mut sink);
    sink.facts()
        .iter()
        .any(|fact| range.start <= fact.line && fact.line <= range.end)
}

/// `sigil + name` of a key, with its domain when scoped.
fn display(key: &NameKey) -> String {
    let mut text = format!("{}{}", key.namespace.sigil().unwrap_or(""), key.name);
    if !key.domain.is_empty() {
        text.push_str(&format!(" in {}", key.domain));
    }
    text
}

/// A definition line shortened to its selector: text before a rule body, clipped.
fn selector(text: &str) -> String {
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

/// `file:line` of a site.
fn location(shown: &ShownSite) -> String {
    format!("{}:{}", shown.site.file.display(), shown.site.fact.line)
}

/// The tagged usage row of a shown site; `key` is added to the tag when a card mixes keys.
fn usage(shown: &ShownSite, key: Option<&NameKey>) -> Usage {
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
        is_test: shown.site.language.support().is_test_file(&shown.site.file),
        tag: Some(tag),
    }
}

/// The index-state and coverage lines a bridge reply ends with.
fn notes(uncovered: &BTreeSet<Lang>, state: IndexState) -> Vec<String> {
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
    lines
}

/// The innermost outline symbol holding `line` of a file the index read, as `path (lines a–b)`,
/// when its language outlines from source.
fn address(index: &NameIndex, shown: &ShownSite) -> Option<(String, LineRange)> {
    let source = index.source(&shown.site.file)?;
    let outline = shown
        .site
        .language
        .support()
        .outline_from_source(&shown.site.file, &source)?;
    let line = shown.site.fact.line;
    let mut found: Option<&Symbol> = None;
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
    found.map(|symbol| (symbol.path.to_string(), symbol.range))
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
}

/// What a name card shows for one key, gathered on the blocking pool.
struct NameLinks {
    /// The key.
    key: NameKey,
    /// Its definitions with outline addresses where the language has them.
    defines: Vec<(ShownSite, Option<(String, LineRange)>)>,
    /// Definitions dropped as unreadable.
    defines_dropped: usize,
    /// Its uses.
    uses: Proven,
    /// Present languages that cannot state the namespace.
    uncovered: BTreeSet<Lang>,
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
        let state = with_names(index.clone(), move |index| index.refresh(deadline)).await?;
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
        // The file's own facts decide first, from the observed bytes: a symbol without name facts
        // (most code) never touches the index, so its card costs nothing extra.
        if !has_facts_in(file, bytes, found.range) {
            return Ok(());
        }
        let (index, state) = self.name_index(job, worktree).await?;
        let own_file = file.to_path_buf();
        let (file, bytes) = (file.to_path_buf(), bytes.to_vec());
        let range = found.range;
        let children: Vec<LineRange> = found.children.iter().map(|child| child.range).collect();
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
                        let defines =
                            index.proven_sites(key, Some(Role::Define), MAX_LINK_DEFINITIONS);
                        (key.clone(), defines)
                    })
                    .collect(),
                linked_total: used.len(),
                uncovered: namespaces
                    .into_iter()
                    .flat_map(|namespace| index.uncovered(namespace))
                    .collect(),
            }
        })
        .await?;
        if gathered.defined.is_empty() && gathered.linked.is_empty() {
            return Ok(());
        }
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
            rows.extend(
                uses.sites
                    .iter()
                    .map(|shown| (shown, usage(shown, mixed.then_some(key)))),
            );
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
                        .map(|shown| format!("{} {}", location(shown), selector(&shown.text)))
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
        card.links.extend(notes(&gathered.uncovered, state));
        Ok(())
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
                    NameLinks {
                        defines: defines
                            .sites
                            .into_iter()
                            .map(|shown| {
                                let address = address(index, &shown);
                                (shown, address)
                            })
                            .collect(),
                        defines_dropped: defines.dropped,
                        uses,
                        uncovered: index.uncovered(key.namespace).into_iter().collect(),
                        key,
                    }
                })
                .collect::<Vec<_>>()
        })
        .await?;
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
                    .map(|shown| usage(shown, None))
                    .collect(),
                usages_indexed: true,
                usages_dropped: links.uses.dropped + links.defines_dropped,
                report_empty_usages: true,
                links: notes(&links.uncovered, state),
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
        with_names(index, move |index| {
            let keys = index.keys_named(&name, Some(namespace));
            let key = keys
                .iter()
                .find(|summary| summary.key.domain.is_empty())
                .or(keys.first())?
                .key
                .clone();
            let proven = index.proven_sites(&key, Some(Role::Define), 1);
            let shown = proven.sites.first()?;
            let line = shown.site.fact.line;
            Some(match address(index, shown) {
                Some((path, range)) => (shown.site.file.clone(), range, path),
                None => (
                    shown.site.file.clone(),
                    LineRange::new(line, line),
                    location(shown),
                ),
            })
        })
        .await?
        .ok_or(FailureCode::UnknownSymbol)
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
    }
}
