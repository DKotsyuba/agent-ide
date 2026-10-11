//! Reading by address (`symbol-read-edit-v1` section 2): one resolver turns any `ide.read`
//! symbol string into a unit of an [`AddressMap`] and renders it with lines numbered from 1
//! inside the unit.
//!
//! Accepted texts (symbol-address-v1): `file#Type/method`, the same with a `:N`, `:N-M` or `:N-`
//! window counted from the unit's first line, a region such as `file#@imports`, and the
//! `path:line[:col]` location that `grep -n` prints (absolute inside the worktree, or relative).
//! A location names the smallest unit that holds the line and reads that whole unit.

use super::symbols::{Lexical, observed_text};
use super::*;
use crate::lang::address::{Address, AddressMap, Parsed, ResolveError, Window};
use std::sync::Arc;

/// What a read target text asked for, before any file is opened.
pub(super) enum Requested {
    /// `file#unit[:window]`.
    Address(Address),
    /// `path:line`, resolved to the enclosing unit once the file's map exists.
    Line {
        /// The file as written (relative or absolute).
        file: PathBuf,
        /// 1-based file line.
        line: u32,
    },
}

/// Parses `text` as a unit address or a `path:line` location; `None` for anything else (a sigil
/// address, a bare name, a path with no unit), which the caller reads the pre-v1 way.
pub(super) fn parse_request(text: &str) -> Option<Requested> {
    match Address::parse(text).ok()? {
        Parsed::Address(address) if address.file().is_some() && !address.segments().is_empty() => {
            Some(Requested::Address(address))
        }
        Parsed::Location { file, line, .. } => Some(Requested::Line { file, line }),
        Parsed::Address(_) => None,
    }
}

/// The reply text for one unit read, or `None` when `request` names no unit of `map`.
///
/// `file` is the worktree-relative path `map` and `source` belong to. The header is
/// `<address>  (<n> lines; file lines <a>-<b>)`, with `; your line <L> is line <K>` for a
/// location; a window reads `<address>:<x>-<y>  (lines <x>-<y> of <n>; file lines <a>-<b>)`.
/// Lines are numbered from 1 inside the unit (the window keeps the unit's numbers) and the
/// number column is as wide as the largest number shown. A window that starts past the unit
/// answers `<address>: unit has <n> lines; nothing at <window>` as a success; a window end past
/// the unit is clamped. Also returns the whole unit's text, which the caller remembers as the
/// session's last read of the unit.
pub(super) fn read_unit(
    file: &Path,
    map: &AddressMap,
    source: &str,
    request: &Requested,
) -> Option<(String, UnitRead)> {
    let (unit, window, line) = match request {
        Requested::Address(address) => match map.resolve(address) {
            Ok(resolved) => (resolved.unit, address.window(), None),
            Err(ResolveError::LineOutOfRange { length }) => {
                let unit = map.resolve(&address.without_window()).ok()?.unit;
                let canonical = canonical(file, map, unit);
                let window = address.window()?;
                return Some((
                    format!("{canonical}: unit has {length} lines; nothing at {window}\n"),
                    UnitRead {
                        address: canonical,
                        text: String::new(),
                    },
                ));
            }
            Err(_) => return None,
        },
        Requested::Line { line, .. } => (map.at_line(*line)?.unit, None, Some(*line)),
    };
    let span = map.units()[unit].span;
    let length = span.len();
    let canonical = canonical(file, map, unit);
    let whole = crate::lang::slice_lines(source, span);
    let (first, last) = match window {
        Some(Window { first, last }) => (first, last.unwrap_or(length).min(length)),
        None => (1, length),
    };
    let width = last.to_string().len();
    let mut text = match window {
        Some(_) => format!(
            "{canonical}:{}  (lines {first}-{last} of {length}; file lines {}-{})\n",
            Window {
                first,
                last: Some(last)
            },
            span.start + first - 1,
            span.start + last - 1
        ),
        None => {
            let mut head = format!(
                "{canonical}  ({length} lines; file lines {}-{}",
                span.start, span.end
            );
            if let Some(line) = line {
                head.push_str(&format!(
                    "; your line {line} is line {}",
                    line - span.start + 1
                ));
            }
            head.push_str(")\n");
            head
        }
    };
    for (index, row) in whole
        .split_inclusive('\n')
        .skip(first as usize - 1)
        .take((last - first + 1) as usize)
        .enumerate()
    {
        let number = first as usize + index + span.start as usize - 1;
        text.push_str(&format!("{number:>width$}\t{row}"));
        if !row.ends_with('\n') {
            text.push('\n');
        }
    }
    Some((
        text,
        UnitRead {
            address: canonical,
            text: whole,
        },
    ))
}

/// Lines `first..=last` (1-based, inside `text`) of a unit's `text`, as `number<TAB>line` rows
/// numbered from `first` and at most `limit` of them, then `+<k> more` when cut.
pub(super) fn numbered_rows(text: &str, first: u32, last: u32, limit: usize) -> String {
    let rows: Vec<&str> = text
        .split_inclusive('\n')
        .skip(first as usize - 1)
        .take((last - first + 1) as usize)
        .collect();
    let width = last.to_string().len();
    let mut out = String::new();
    for (index, row) in rows.iter().take(limit).enumerate() {
        out.push_str(&format!("{:>width$}\t{}\n", first as usize + index, row.trim_end_matches('\n')));
    }
    if rows.len() > limit {
        out.push_str(&format!("+{} more\n", rows.len() - limit));
    }
    out
}

/// What a unit read delivered: the unit's canonical address and its whole current text, the
/// session's memory of the read for the window-edit guard.
pub(super) struct UnitRead {
    /// `file#unit`, never with a window.
    pub(super) address: String,
    /// The unit's whole text at read time; empty when the read delivered no lines.
    pub(super) text: String,
}

/// `file#unit` of unit `unit` of `map`, windowless.
fn canonical(file: &Path, map: &AddressMap, unit: usize) -> String {
    Address::new(
        Some(file.to_path_buf()),
        map.units()[unit].address.segments().to_vec(),
        None,
    )
    .to_string()
}

impl Worker<'_> {
    /// The worktree-relative path of `file`: an absolute path must lie inside the worktree
    /// (`outside_allowed_roots` otherwise), a relative one is returned as is.
    pub(super) async fn relative_file(
        &mut self,
        binding: &BindingRef,
        file: PathBuf,
    ) -> Result<PathBuf, FailureCode> {
        let authority = self.authority(binding).await?;
        match file.strip_prefix(authority.worktree().worktree_path()) {
            Ok(inside) => Ok(inside.to_path_buf()),
            Err(_) if file.is_absolute() => Err(FailureCode::OutsideAllowedRoots),
            Err(_) => Ok(file),
        }
    }

    /// `ide.read {symbol}` for an address, window, region or `path:line` text: `Ok(None)` when
    /// `text` is none of those or names no unit (the caller then reads it the pre-v1 way and
    /// reports its own refusal); otherwise the numbered unit reply (see [`read_unit`]).
    ///
    /// A file no IDE language reads has no map, so its locations and addresses answer the
    /// outline's own refusal. The read is remembered for the window-edit guard.
    pub(super) async fn read_by_address(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        text: &str,
    ) -> Result<
        Option<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>)>,
        FailureCode,
    > {
        let Some(request) = parse_request(text) else {
            return Ok(None);
        };
        let file = match &request {
            Requested::Address(address) => {
                address.file().map(Path::to_path_buf).unwrap_or_default()
            }
            Requested::Line { file, .. } => self.relative_file(binding, file.clone()).await?,
        };
        let (observed, bytes) = self.observe(binding, file.clone()).await.inspect_err(|code| {
            if matches!(code, FailureCode::SourceUnavailable) {
                job.failure_detail = Some(format!("read:source_unavailable:{}", file.display()));
            }
        })?;
        if let Some(code) = super::symbols::no_such_file(observed.state(), &file.to_string_lossy())
        {
            return Err(code);
        }
        let (map, lexical): (Arc<AddressMap>, Option<Lexical>) =
            Box::pin(self.address_map(job, &observed, &bytes)).await?;
        let source = observed_text(&observed, &bytes)?;
        if let Requested::Line { line, .. } = &request
            && *line > map.units().last().map_or(0, |unit| unit.span.end)
        {
            let authority = self.finish_symbol_job(job, binding, &observed).await?;
            let total = crate::lang::line_count(source);
            let text = format!("{}: file has {total} lines; nothing at {line}\n", file.display());
            return self.unit_reply(job, text, authority, observed).map(Some);
        }
        let Some((mut body, read)) = read_unit(&file, &map, source, &request) else {
            return Ok(None);
        };
        if let Some(note) = lexical
            .as_ref()
            .and_then(|why| self.lexical_note(job, observed.path(), why))
        {
            body.push_str(&note);
            body.push('\n');
        }
        let authority = self.finish_symbol_job(job, binding, &observed).await?;
        self.remember_unit_read(binding, read);
        self.unit_reply(job, body, authority, observed).map(Some)
    }

    /// Remembers `read` as this session's last read of its unit, for the window-edit guard;
    /// nothing is kept while the context epoch is unknown.
    pub(super) fn remember_unit_read(&self, binding: &BindingRef, read: UnitRead) {
        let epoch = self.context_epoch(binding);
        if let Ok(mut ledger) = self.shared.ledger.lock() {
            ledger
                .read_memory
                .entry(binding.clone())
                .or_default()
                .remember_unit(epoch, &read.address, read.text);
        }
    }

    /// The text of unit `address` (`file#unit`) as this session last read it in the current
    /// context epoch, or `None` when nothing is remembered (never read, `ide.stop`, a context
    /// compaction, or an unknown epoch).
    pub(super) fn remembered_unit(&self, binding: &BindingRef, address: &str) -> Option<String> {
        let epoch = self.context_epoch(binding);
        let ledger = self.shared.ledger.lock().ok()?;
        ledger
            .read_memory
            .get(binding)?
            .unit_text(epoch, address)
            .map(str::to_owned)
    }

    /// Wraps a rendered unit read as the paged `read` reply of the job.
    fn unit_reply(
        &mut self,
        job: &Job,
        text: String,
        authority: AuthorityStamp,
        observed: SourceObservation,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let (reply, page) =
            ContextPageState::new(text, 0, false, ResultKind::Read).next(&job.reference)?;
        self.shared.set_context_page(&job.reference, page);
        Ok((reply, Some(authority), Some(observed)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::{LineRange, Symbol, SymbolKind, SymbolPath};

    /// One symbol of `f.rs` spanning `lines`, named by the path `names`.
    fn symbol(names: &[&str], kind: SymbolKind, lines: (u32, u32), children: Vec<Symbol>) -> Symbol {
        let range = LineRange::new(lines.0, lines.1);
        Symbol {
            path: SymbolPath::new(
                Some("f.rs".into()),
                names.iter().map(|n| n.to_string()).collect(),
            ),
            kind,
            name: names.last().unwrap().to_string(),
            range,
            body: range,
            signature: String::new(),
            doc: None,
            children,
        }
    }

    /// A file with a header comment, a struct, an impl with two methods and a tail comment.
    fn sample() -> (String, AddressMap) {
        let src = "// header\n\nstruct Foo;\n\nimpl Foo {\n    fn new() {}\n    // note\n    fn new() {}\n}\n\n// tail\n";
        let new = |n| symbol(&["Foo", "new"], SymbolKind::Method, (n, n), Vec::new());
        let symbols = vec![
            symbol(&["Foo"], SymbolKind::Struct, (3, 3), Vec::new()),
            symbol(&["Foo"], SymbolKind::Impl, (5, 9), vec![new(6), new(8)]),
        ];
        let map = AddressMap::build_parts(
            Path::new("f.rs"),
            &symbols,
            src,
            &crate::lang::regions::GENERIC_RULES,
        );
        (src.to_owned(), map)
    }

    /// Reads `text` from the sample file, panicking when it names no unit.
    fn read(text: &str) -> String {
        let (src, map) = sample();
        let request = parse_request(text).unwrap_or_else(|| panic!("not a target: {text}"));
        read_unit(Path::new("f.rs"), &map, &src, &request)
            .unwrap_or_else(|| panic!("no unit: {text}"))
            .0
    }

    /// A unit reads whole with its extent, numbered from 1 inside the unit.
    #[test]
    fn a_unit_reads_numbered_from_one() {
        assert_eq!(
            read("f.rs#Foo~2"),
            "f.rs#Foo~2  (5 lines; file lines 5-9)\n1\timpl Foo {\n2\t    fn new() {}\n3\t    // note\n4\t    fn new() {}\n5\t}\n"
        );
        assert_eq!(
            read("f.rs#@header"),
            "f.rs#@header  (2 lines; file lines 1-2)\n1\t// header\n2\t\n"
        );
    }

    /// A window keeps the unit's numbers, is clamped at the unit's end and past it answers
    /// a success naming the unit's length.
    #[test]
    fn a_window_keeps_unit_numbers_and_clamps() {
        assert_eq!(
            read("f.rs#Foo~2:2-3"),
            "f.rs#Foo~2:2-3  (lines 2-3 of 5; file lines 6-7)\n2\t    fn new() {}\n3\t    // note\n"
        );
        assert_eq!(
            read("f.rs#Foo~2:4-"),
            "f.rs#Foo~2:4-5  (lines 4-5 of 5; file lines 8-9)\n4\t    fn new() {}\n5\t}\n"
        );
        assert_eq!(
            read("f.rs#Foo~2:4-99"),
            "f.rs#Foo~2:4-5  (lines 4-5 of 5; file lines 8-9)\n4\t    fn new() {}\n5\t}\n"
        );
        assert_eq!(
            read("f.rs#Foo:2"),
            "f.rs#Foo: unit has 1 lines; nothing at 2\n"
        );
    }

    /// `path:line` reads the unit holding the line and says which of its lines that is; text that
    /// is not an address or names no unit is left to the pre-v1 read.
    #[test]
    fn a_location_reads_its_unit_and_other_text_falls_through() {
        let text = read("f.rs:7");
        assert!(
            text.starts_with("f.rs#Foo~2  (5 lines; file lines 5-9; your line 7 is line 3)\n"),
            "{text}"
        );
        let (src, map) = sample();
        let missing = parse_request("f.rs#Missing/x").unwrap();
        assert!(read_unit(Path::new("f.rs"), &map, &src, &missing).is_none());
        assert!(parse_request("Foo/bar").is_none());
        assert!(parse_request("src/a.rs").is_none());
    }
}
