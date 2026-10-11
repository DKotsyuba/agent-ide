//! Worker glue for `symbol-address-v1` (docs/contracts/symbol-address-v1.md section 7.4): address
//! maps for observed files and best-effort position addresses for result rows.
//!
//! The maps are cheap pure functions of the outline and the exact source text, so nothing is
//! cached across calls; an `Arc` lets one reply share a map between its rows.

use super::symbols::{Lexical, observed_text};
use super::*;
use crate::lang::address::AddressMap;
use crate::workspace::observation::{MAX_SOURCE_BYTES, SourceReadLimits, read_authorized_source};
use std::sync::Arc;
use std::time::Instant;

/// Files one reply may outline for position addresses.
pub(super) const MAX_ADDRESS_FILES: usize = 16;

/// Wall-clock budget of one reply for position addresses.
pub(super) const ADDRESS_BUDGET: Duration = Duration::from_millis(1500);

/// Position addresses of a list of `(file, line)` locations, in request order.
pub(super) struct Addresses {
    /// `file#unit:N` per requested location; `None` when it was not resolved.
    pub(super) rows: Vec<Option<String>>,
    /// Why some locations have no address, naming the first cause.
    pub(super) reason: Option<&'static str>,
}

impl Addresses {
    /// The row text `address (L123)` of location `index`, or `None` when it has no address.
    pub(super) fn tag(&self, index: usize, line: u32) -> Option<String> {
        self.rows
            .get(index)?
            .as_ref()
            .map(|address| tagged(address, line))
    }

    /// The reply line `addresses: omitted for N locations (<reason>)`, or `None` when every
    /// location resolved.
    pub(super) fn note(&self) -> Option<String> {
        let missing = self.rows.iter().filter(|row| row.is_none()).count();
        (missing > 0).then(|| {
            format!(
                "addresses: omitted for {missing} locations ({})",
                self.reason.unwrap_or("not resolved")
            )
        })
    }
}

/// The symbol part of a position address `file#unit:N`: everything before the final `:N`.
pub(super) fn symbol_part(address: &str) -> &str {
    match address.rsplit_once(':') {
        Some((unit, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => unit,
        _ => address,
    }
}

/// `address (L123)`: the position address followed by its compact absolute line.
pub(super) fn tagged(address: &str, line: u32) -> String {
    format!("{address} (L{line})")
}

/// The `@ <address>` lines for the hunks `result` selected (see
/// [`Worker::diff_hunk_addresses`]) for callers that have no worker: each file is read from
/// `root` and outlined by its language's source outline, never a server, within
/// [`ADDRESS_BUDGET`]. A hunk whose file has no source outline gets no line.
pub(super) async fn lexical_hunk_addresses(
    root: &Path,
    result: &crate::changes::DiffResult,
) -> BTreeMap<usize, String> {
    let lookup = async {
        let mut lines = BTreeMap::new();
        let mut maps: BTreeMap<PathBuf, Option<AddressMap>> = BTreeMap::new();
        for hunk in result.selected_hunks() {
            if !hunk.starts_hunk() {
                continue;
            }
            if hunk.deletes_file() {
                lines.insert(hunk.index(), format!("{} (deleted)", hunk.path().display()));
                continue;
            }
            let Some(line) = hunk.first_changed_line() else {
                continue;
            };
            let path = hunk.path().clone();
            if !maps.contains_key(&path) {
                let map = lexical_map(root, &path).await;
                maps.insert(path.clone(), map);
            }
            if let Some(Some(map)) = maps.get(&path)
                && let Some(address) = map.address_at(&path, line)
            {
                lines.insert(hunk.index(), tagged(&address, line));
            }
        }
        lines
    };
    tokio::time::timeout(ADDRESS_BUDGET, lookup)
        .await
        .unwrap_or_default()
}

/// The address map of `relative` under `root` from its language's source outline.
pub(super) async fn lexical_map(root: &Path, relative: &Path) -> Option<AddressMap> {
    let language = crate::lang::Language::for_path(relative)?;
    let source = std::fs::read_to_string(root.join(relative)).ok()?;
    let outline = crate::modules::calls::outline_from_source(language, root, relative, &source)
        .await
        .ok()??;
    Some(AddressMap::build(&outline, &source))
}

impl Worker<'_> {
    /// The [`AddressMap`] of one observed file, built from the same outline the call renders
    /// (server, module or lexical) and the exact source `bytes`; the second value says why the
    /// outline came from the text alone. Failure codes are `outline_of`'s.
    pub(super) async fn address_map(
        &mut self,
        job: &mut Job,
        observed: &SourceObservation,
        bytes: &[u8],
    ) -> Result<(Arc<AddressMap>, Option<Lexical>), FailureCode> {
        // Boxed: the outline futures are large and these helpers sit under the deepest call
        // chains (batch symbol -> card -> addresses), where debug-build stack frames add up.
        let (outline, _, lexical) = Box::pin(self.outline_of(job, observed, bytes)).await?;
        let map = AddressMap::build(&outline, observed_text(observed, bytes)?);
        Ok((Arc::new(map), lexical))
    }

    /// The address map of an observed file for rows that can do without one: `None` when the
    /// file has no outline right now. Never parks the job and leaves its failure detail alone.
    pub(super) async fn optional_address_map(
        &mut self,
        job: &mut Job,
        observed: &SourceObservation,
        bytes: &[u8],
    ) -> Option<Arc<AddressMap>> {
        let parked = job.park_until.take();
        let detail = job.failure_detail.take();
        let map = Box::pin(self.address_map(job, observed, bytes)).await.ok();
        job.park_until = parked;
        job.failure_detail = detail;
        map.map(|(map, _)| map)
    }

    /// The `@ <address>` lines for the hunks `result` selected, by hunk index: the position
    /// address (with absolute line) of the hunk's first changed line in the changed file, or
    /// `<path> (deleted)` for a file the hunk removes. A hunk whose file has no outline gets no
    /// line. Staged mode reads the working-tree file, which equals the index image unless the
    /// file also has unstaged edits.
    pub(super) async fn diff_hunk_addresses(
        &mut self,
        job: &mut Job,
        result: &crate::changes::DiffResult,
    ) -> BTreeMap<usize, String> {
        let mut lines = BTreeMap::new();
        let mut asked: Vec<(usize, PathBuf, u32)> = Vec::new();
        for hunk in result.selected_hunks() {
            if !hunk.starts_hunk() {
                continue;
            }
            if hunk.deletes_file() {
                lines.insert(hunk.index(), format!("{} (deleted)", hunk.path().display()));
            } else if let Some(line) = hunk.first_changed_line() {
                asked.push((hunk.index(), hunk.path().clone(), line));
            }
        }
        let locations: Vec<(PathBuf, u32)> = asked
            .iter()
            .map(|(_, path, line)| (path.clone(), *line))
            .collect();
        let addresses = self.position_addresses(job, &locations).await;
        for (slot, (index, _, line)) in asked.iter().enumerate() {
            if let Some(tag) = addresses.tag(slot, *line) {
                lines.insert(*index, tag);
            }
        }
        lines
    }

    /// Fills the card's usage rows with position addresses and its caller and callee rows with
    /// the addresses of the symbols they name, each with its absolute line; locations that did
    /// not resolve keep `file:line`, and one trailing line says how many were left out.
    pub(super) async fn address_card_rows(
        &mut self,
        job: &mut Job,
        card: &mut crate::lang::render::SymbolCard,
    ) {
        let locate = |file: &str, line: u32| (PathBuf::from(file), line);
        let usages: Vec<(PathBuf, u32)> = card
            .usages
            .iter()
            .map(|usage| locate(&usage.file, usage.line))
            .collect();
        let calls: Vec<(PathBuf, u32)> = card
            .callers
            .iter()
            .chain(&card.callees)
            .map(|call| locate(&call.file, call.line))
            .collect();
        if usages.is_empty() && calls.is_empty() {
            return;
        }
        let all: Vec<(PathBuf, u32)> = usages.iter().chain(&calls).cloned().collect();
        let addresses = Box::pin(self.position_addresses(job, &all)).await;
        for (index, (file, line)) in all.iter().enumerate() {
            let key = (file.to_string_lossy().into_owned(), *line);
            if index < usages.len() {
                if let Some(tag) = addresses.tag(index, *line) {
                    card.usage_addresses.insert(key, tag);
                }
            } else if let Some(Some(address)) = addresses.rows.get(index) {
                card.call_addresses
                    .insert(key, tagged(symbol_part(address), *line));
            }
        }
        if let Some(note) = addresses.note() {
            card.links.push(note);
        }
    }

    /// Fills the graph's nodes with the addresses of the symbols they name, each with its
    /// absolute line; a node that did not resolve keeps its raw path and `file:line`. Returns the
    /// `addresses: omitted for N locations (<reason>)` line when some node did not resolve.
    pub(super) async fn address_graph_nodes(
        &mut self,
        job: &mut Job,
        graph: &mut crate::lang::render::CallGraph,
    ) -> Option<String> {
        let all: Vec<(PathBuf, u32)> = graph
            .nodes
            .iter()
            .filter(|node| !node.file.is_empty())
            .map(|node| (PathBuf::from(&node.file), node.line))
            .collect();
        let addresses = Box::pin(self.position_addresses(job, &all)).await;
        for (index, (file, line)) in all.iter().enumerate() {
            if let Some(Some(address)) = addresses.rows.get(index) {
                graph.addresses.insert(
                    (file.to_string_lossy().into_owned(), *line),
                    tagged(symbol_part(address), *line),
                );
            }
        }
        addresses.note()
    }

    /// Turns a `path:line[:col]` location (what grep and compilers print) into the canonical
    /// address of the symbol that encloses the line; any other text comes back unchanged.
    /// An absolute path must lie inside the worktree. A line beyond the file, or one that
    /// belongs to a file-level region rather than a symbol, refuses `unknown_symbol` with a
    /// detail naming the region address.
    pub(super) async fn locate_line(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        requested: &str,
    ) -> Result<String, FailureCode> {
        use crate::lang::address::{Address, Parsed, UnitKind};

        let Ok(Parsed::Location { file, line, .. }) = Address::parse(requested) else {
            return Ok(requested.to_owned());
        };
        let authority = self.authority(binding).await?;
        let relative = match file.strip_prefix(authority.worktree().worktree_path()) {
            Ok(inside) => inside.to_path_buf(),
            Err(_) if file.is_absolute() => return Err(FailureCode::OutsideAllowedRoots),
            Err(_) => file,
        };
        let (observed, bytes) = self.observe(binding, relative.clone()).await?;
        if let Some(code) =
            super::symbols::no_such_file(observed.state(), &relative.to_string_lossy())
        {
            return Err(code);
        }
        let (map, _) = Box::pin(self.address_map(job, &observed, &bytes)).await?;
        let Some(located) = map.at_line(line) else {
            job.failure_detail = Some(format!(
                "symbol:line_past_end: {}:{line} is past the end of the file",
                relative.display()
            ));
            return Err(FailureCode::UnknownSymbol);
        };
        let unit = &map.units()[located.unit];
        match unit.kind {
            UnitKind::Symbol(_) => Ok(unit.address.to_string()),
            UnitKind::Region(_) => {
                job.failure_detail = Some(format!(
                    "symbol:region: {}:{line} is file-level code at {}, not a symbol; read it with ide.read",
                    relative.display(),
                    unit.address
                ));
                Err(FailureCode::UnknownSymbol)
            }
        }
    }

    /// Position addresses for `locations` (relative file, 1-based line) for result rendering.
    ///
    /// Best effort and bounded: each distinct file is outlined once through `scanned_outline`
    /// (never parking the job for a loading server: the lexical or source outline answers or the
    /// file is skipped), at most [`MAX_ADDRESS_FILES`] files and [`ADDRESS_BUDGET`] per call. A
    /// location it could not resolve answers `None`; no address is ever guessed.
    pub(super) async fn position_addresses(
        &mut self,
        job: &mut Job,
        locations: &[(PathBuf, u32)],
    ) -> Addresses {
        let mut out = Addresses {
            rows: vec![None; locations.len()],
            reason: None,
        };
        let binding = job.invocation.binding_ref().clone();
        let Ok(authority) = self.authority(&binding).await else {
            out.reason = Some("workspace unavailable");
            return out;
        };
        let started = Instant::now();
        let mut maps: BTreeMap<PathBuf, Option<Arc<AddressMap>>> = BTreeMap::new();
        for (index, (file, line)) in locations.iter().enumerate() {
            if !maps.contains_key(file) {
                let map = if maps.len() >= MAX_ADDRESS_FILES {
                    out.reason.get_or_insert("file limit");
                    None
                } else if started.elapsed() >= ADDRESS_BUDGET {
                    out.reason.get_or_insert("time limit");
                    None
                } else {
                    let found = Box::pin(self.scanned_map(job, &authority, file)).await;
                    if found.is_none() {
                        out.reason.get_or_insert("no outline for the file");
                    }
                    found
                };
                maps.insert(file.clone(), map);
            }
            out.rows[index] = maps
                .get(file)
                .and_then(Option::as_ref)
                .and_then(|map| map.address_at(file, *line));
        }
        out
    }

    /// The map of `file` read without registering it against the binding, from the best outline
    /// that answers without waiting; `None` when the file cannot be read or outlined. A parked
    /// retry or a stage failure from this side lookup never leaks into the job's own answer.
    pub(super) async fn scanned_map(
        &mut self,
        job: &mut Job,
        authority: &AuthorityStamp,
        file: &Path,
    ) -> Option<Arc<AddressMap>> {
        let limits = SourceReadLimits::new(1024, MAX_SOURCE_BYTES).ok()?;
        let read = read_authorized_source(authority.worktree(), file, limits).ok()?;
        let text = String::from_utf8(read.contents().to_vec()).ok()?;
        let parked = job.park_until.take();
        let detail = job.failure_detail.take();
        let outline = Box::pin(self.scanned_outline(job, authority, file)).await;
        job.park_until = parked;
        job.failure_detail = detail;
        Some(Arc::new(AddressMap::build(&outline?.0, &text)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_carry_the_absolute_line_and_the_omission_note() {
        let addresses = Addresses {
            rows: vec![Some("src/x.rs#T/m:7".to_owned()), None, None],
            reason: Some("time limit"),
        };
        assert_eq!(
            addresses.tag(0, 123).as_deref(),
            Some("src/x.rs#T/m:7 (L123)")
        );
        assert_eq!(addresses.tag(1, 4), None);
        assert_eq!(
            addresses.note().as_deref(),
            Some("addresses: omitted for 2 locations (time limit)")
        );
        let all = Addresses {
            rows: vec![Some("a#b:1".to_owned())],
            reason: None,
        };
        assert_eq!(all.note(), None);
    }
}
