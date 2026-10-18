//! Batch request forms shared by the Assistance facade and worker (batch-api-v1).
//!
//! The facade validates a batch request against the caps here before anything runs; the worker
//! turns the validated request back into typed items and renders the summary line and the
//! follow-up footer of its reply with the pure helpers here. Nothing in this module observes a
//! file, touches the ledger or decides how much of a reply fits: those stay with the worker.

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use super::facade::{
    AssistanceTool, FieldRule, ParameterError, edit_path_shape_rule, invalid_field,
    parse_read_range, path_shape_rule,
};
use super::reply::{
    EditBatchFile, EditBatchFileState, EditBatchOutcome, EditBatchReason, PeerReply,
};
use crate::lang::SymbolPath;

/// Argument ceiling, in serialized JSON bytes, of every batch form of `ide.read`,
/// `ide.outline`, `ide.symbol` and `ide.graph`; their single forms keep the facade's 4 KiB.
pub(crate) const MAX_BATCH_PARAMETER_BYTES: usize = 16 * 1024;

/// Most entries one `ide.read {items}` request may carry.
pub(crate) const MAX_READ_ITEMS: usize = 32;

/// Most distinct files the items of one `ide.read {items}` request may name, which equals the
/// multi-file edit's file cap so one read's `source_ref` can cover every file of one edit.
pub(crate) const MAX_READ_FILES: usize = 16;

/// Footer entries named inline before `+N more`, matching the worker's other bounded lists.
const MAX_FOOTER_NAMES: usize = 8;

/// The closed item shape an `items` entry must have, quoted in its refusal.
const ITEM_SHAPE: &str = "an array of 1 to 32 items, each exactly one of {\"symbol\":\"src/x.rs#Owner/name\"}, \
{\"path\":\"docs/x.md\"} or {\"path\":\"src/x.rs\",\"lines\":\"10-40\"}";

/// Names the batch key a request uses, or `None` for a single-target request.
///
/// `parameters` is the raw argument value before validation; a non-object yields `None`. The
/// returned key decides the argument ceiling ([`MAX_BATCH_PARAMETER_BYTES`]) and is the field an
/// over-limit refusal names: `ide.read`'s `items`, `symbols` and `ranges`, the `symbols` of
/// `ide.symbol` and `ide.graph`, and `ide.outline`'s `paths` and `glob`.
pub(crate) fn batch_key(tool: AssistanceTool, parameters: &Value) -> Option<&'static str> {
    let object = parameters.as_object()?;
    let keys: &[&'static str] = match tool {
        AssistanceTool::Read => &["items", "symbols", "ranges"],
        AssistanceTool::Symbol | AssistanceTool::Graph => &["symbols"],
        AssistanceTool::Outline => &["paths", "glob"],
        _ => &[],
    };
    keys.iter().copied().find(|key| object.contains_key(*key))
}

/// Most files one `ide.outline {paths}` or `ide.outline {glob}` call outlines.
pub(crate) const MAX_OUTLINE_FILES: usize = 24;

/// Validates one `ide.outline {paths | glob, kinds?}` request object.
///
/// Exactly one of `paths` (1 to [`MAX_OUTLINE_FILES`] worktree-relative paths, each validated as
/// the single form's `path`) and `glob` (one worktree-relative pattern of at most 1024 bytes with
/// no absolute start, no `..` segment and no NUL) appears, never beside `path`; `kinds` keeps its
/// single-form rule.
pub(crate) fn validate_outline(object: &Map<String, Value>) -> Result<(), ParameterError> {
    use super::facade::validate_call;
    let keys = ["path", "paths", "glob"]
        .iter()
        .filter(|key| object.contains_key(**key))
        .count();
    if keys != 1 {
        return Err(ParameterError::OutlineTarget);
    }
    let single = |path: &Value| {
        let mut single = object.clone();
        single.remove("paths");
        single.remove("glob");
        single.insert("path".to_owned(), path.clone());
        validate_call(AssistanceTool::Outline, Value::Object(single)).map(|_| ())
    };
    if let Some(paths) = object.get("paths") {
        let paths = paths
            .as_array()
            .filter(|paths| (1..=MAX_OUTLINE_FILES).contains(&paths.len()))
            .ok_or_else(|| {
                invalid_field(
                    "paths",
                    FieldRule::OneOf("an array of 1 to 24 worktree-relative paths"),
                )
            })?;
        return paths.iter().try_for_each(single);
    }
    let glob = object
        .get("glob")
        .and_then(Value::as_str)
        .filter(|glob| !glob.is_empty() && glob.len() <= 1024)
        .ok_or_else(|| invalid_field("glob", FieldRule::NonEmptyString))?;
    if let Some(rule) = path_shape_rule(glob) {
        return Err(invalid_field("glob", rule));
    }
    if glob.starts_with('/') {
        return Err(invalid_field("glob", FieldRule::RelativePath));
    }
    // `kinds` is checked against the single form with a placeholder path.
    single(&json!("placeholder.rs"))
}

/// The literal directory a glob's walk starts in: its leading segments before the first one with
/// a wildcard (`*` or `?`); empty for a pattern that starts with one.
pub(crate) fn glob_root(pattern: &str) -> String {
    let segments: Vec<&str> = pattern.split('/').collect();
    segments[..segments.len() - 1]
        .iter()
        .take_while(|segment| !segment.contains(['*', '?']))
        .copied()
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether a worktree-relative `path` matches `pattern`: `*` matches within one segment, `?`
/// one character, and a `**` segment any number of whole segments (none included).
pub(crate) fn glob_matches(pattern: &str, path: &str) -> bool {
    /// Matches one segment's characters against one pattern segment's.
    fn segment(pattern: &[char], name: &[char]) -> bool {
        match (pattern.first(), name.first()) {
            (None, None) => true,
            (Some('*'), _) => {
                segment(&pattern[1..], name) || (!name.is_empty() && segment(pattern, &name[1..]))
            }
            (Some('?'), Some(_)) => segment(&pattern[1..], &name[1..]),
            (Some(expected), Some(actual)) if expected == actual => {
                segment(&pattern[1..], &name[1..])
            }
            _ => false,
        }
    }
    /// Matches whole segment lists, with `**` spanning any number of segments.
    fn segments(pattern: &[&str], path: &[&str]) -> bool {
        match (pattern.first(), path.first()) {
            (None, None) => true,
            (Some(&"**"), _) => {
                segments(&pattern[1..], path) || (!path.is_empty() && segments(pattern, &path[1..]))
            }
            (Some(expected), Some(actual)) => {
                segment(
                    &expected.chars().collect::<Vec<_>>(),
                    &actual.chars().collect::<Vec<_>>(),
                ) && segments(&pattern[1..], &path[1..])
            }
            _ => false,
        }
    }
    segments(
        &pattern.split('/').collect::<Vec<_>>(),
        &path.split('/').collect::<Vec<_>>(),
    )
}

/// Most entries `ide.symbol {symbols}` and `ide.graph {symbols}` accept.
pub(crate) const MAX_SYMBOL_ENTRIES: usize = 8;

/// Validates one `ide.symbol {symbols, …}` or `ide.graph {symbols, …}` request object.
///
/// `symbols` replaces `symbol` (never beside it) and holds 1 to [`MAX_SYMBOL_ENTRIES`] symbol
/// paths or bare names; every other field keeps its single-form meaning. Each entry is validated
/// as the single form with the same options would be, so both forms refuse the same values.
pub(crate) fn validate_symbols(
    tool: AssistanceTool,
    object: &Map<String, Value>,
) -> Result<(), ParameterError> {
    if object.contains_key("symbol") {
        return Err(invalid_field(
            "symbols",
            FieldRule::OneOf("used instead of \"symbol\", never beside it"),
        ));
    }
    let entries = object
        .get("symbols")
        .and_then(Value::as_array)
        .filter(|entries| (1..=MAX_SYMBOL_ENTRIES).contains(&entries.len()))
        .ok_or_else(|| {
            invalid_field(
                "symbols",
                FieldRule::OneOf("an array of 1 to 8 symbol paths or bare names"),
            )
        })?;
    for entry in entries {
        let mut single = object.clone();
        single.remove("symbols");
        single.insert("symbol".to_owned(), entry.clone());
        super::facade::validate_call(tool, Value::Object(single))?;
    }
    Ok(())
}

/// Validates one `ide.read {items, force?}` request object.
///
/// The request must carry no single-target or legacy batch key beside `items` (`symbol`, `path`,
/// `lines`, `symbols`, `ranges`), `force` must be a boolean when present, and `items` must hold
/// 1 to [`MAX_READ_ITEMS`] objects, each exactly one of `{symbol}` (a `file#Owner/name` address
/// with at least one segment), `{path}` or `{path, lines}` (a relative path without `..`, NUL or
/// empty segments, and an inclusive 1-based range), naming at most [`MAX_READ_FILES`] distinct
/// files together. Every violation is the single field-level refusal of the facade; nothing is
/// observed here.
pub(crate) fn validate_read_items(object: &Map<String, Value>) -> Result<(), ParameterError> {
    if ["symbol", "path", "lines", "symbols", "ranges"]
        .iter()
        .any(|field| object.contains_key(*field))
    {
        return Err(ParameterError::ReadTarget);
    }
    if object.get("force").is_some_and(|value| !value.is_boolean()) {
        return Err(invalid_field("force", FieldRule::Boolean));
    }
    let shape = || invalid_field("items", FieldRule::OneOf(ITEM_SHAPE));
    let items = object
        .get("items")
        .and_then(Value::as_array)
        .filter(|items| (1..=MAX_READ_ITEMS).contains(&items.len()))
        .ok_or_else(shape)?;
    let mut files = BTreeSet::new();
    for entry in items {
        let entry = entry.as_object().ok_or_else(shape)?;
        if entry
            .keys()
            .any(|key| !matches!(key.as_str(), "symbol" | "path" | "lines"))
        {
            return Err(shape());
        }
        let text = |key: &str| {
            entry
                .get(key)
                .map(|value| {
                    value
                        .as_str()
                        .filter(|text| !text.is_empty() && text.len() <= 1024)
                        .ok_or_else(shape)
                })
                .transpose()
        };
        let file = match (text("symbol")?, text("path")?, text("lines")?) {
            (Some(symbol), None, None) => {
                let parsed = SymbolPath::parse(symbol).map_err(|_| shape())?;
                match parsed.file() {
                    Some(file) if !parsed.segments().is_empty() => {
                        file.to_string_lossy().into_owned()
                    }
                    _ => return Err(shape()),
                }
            }
            (None, Some(path), lines) => {
                if let Some(rule) = path_shape_rule(path) {
                    return Err(invalid_field("items", rule));
                }
                if lines.is_some_and(|lines| parse_read_range(lines).is_none()) {
                    return Err(invalid_field("items", FieldRule::LineRange));
                }
                path.to_owned()
            }
            _ => return Err(shape()),
        };
        files.insert(file);
    }
    if files.len() > MAX_READ_FILES {
        return Err(invalid_field(
            "items",
            FieldRule::OneOf("items naming at most 16 distinct files"),
        ));
    }
    Ok(())
}

/// Which request form produced a batch read; it decides how the follow-up footer spells the call
/// that fetches cut items, so that call is always a valid request of the same form.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReadForm {
    /// `ide.read {items: […]}`.
    Items,
    /// The legacy `ide.read {symbols: […]}` list.
    Symbols,
    /// The legacy `ide.read {path, ranges: […]}` list of one file.
    Ranges,
}

/// One requested block of a batch read, exactly as the caller spelled it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ReadItem {
    /// A symbol address `file#Owner/name`. From the legacy `symbols` list only, a bare file path
    /// (no `#`), which reads the text of a file no IDE language reads.
    Symbol(String),
    /// `{path}`: the whole text of one readable text file.
    Path(String),
    /// `{path, lines}` or one legacy `ranges` entry: the path and the range text as requested
    /// (`start-end`, inclusive, 1-based; already validated by the facade).
    Lines(String, String),
}

impl ReadItem {
    /// The worktree-relative file the item reads: the file part of a symbol address (the whole
    /// text for a bare path), or the item's path.
    pub(crate) fn file(&self) -> &str {
        match self {
            Self::Symbol(address) => address.split_once('#').map_or(address, |(file, _)| file),
            Self::Path(path) | Self::Lines(path, _) => path,
        }
    }

    /// The item's one-line name in summary-adjacent lines (duplicates, footers, the unchanged
    /// answer): the symbol address, the path, or `<path> lines <start-end>`.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Symbol(address) => address.clone(),
            Self::Path(path) => path.clone(),
            Self::Lines(path, lines) => format!("{path} lines {lines}"),
        }
    }

    /// The item as one `items` entry of a valid `ide.read {items}` request.
    fn call_entry(&self) -> Value {
        match self {
            Self::Symbol(address) => json!({"symbol": address}),
            Self::Path(path) => json!({"path": path}),
            Self::Lines(path, lines) => json!({"path": path, "lines": lines}),
        }
    }
}

/// Reads the validated batch request of one `ide.read` job back into typed items.
///
/// Returns `None` for a single-target request (`symbol`, `path`, `path`+`lines`). Otherwise the
/// form and its items in request order, duplicates kept (the worker renders a duplicate once).
/// The facade has already validated every shape; an entry that is nevertheless malformed is
/// skipped rather than invented.
pub(crate) fn read_request(parameters: &Value) -> Option<(ReadForm, Vec<ReadItem>)> {
    let strings = |key: &str| -> Option<Vec<String>> {
        parameters.get(key).and_then(Value::as_array).map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
    };
    if let Some(items) = parameters.get("items").and_then(Value::as_array) {
        let items = items
            .iter()
            .filter_map(|entry| {
                let field = |key: &str| entry.get(key).and_then(Value::as_str).map(str::to_owned);
                match (field("symbol"), field("path"), field("lines")) {
                    (Some(symbol), None, None) => Some(ReadItem::Symbol(symbol)),
                    (None, Some(path), None) => Some(ReadItem::Path(path)),
                    (None, Some(path), Some(lines)) => Some(ReadItem::Lines(path, lines)),
                    _ => None,
                }
            })
            .collect();
        return Some((ReadForm::Items, items));
    }
    if let Some(symbols) = strings("symbols") {
        return Some((
            ReadForm::Symbols,
            symbols.into_iter().map(ReadItem::Symbol).collect(),
        ));
    }
    let path = parameters.get("path").and_then(Value::as_str)?;
    strings("ranges").map(|ranges| {
        (
            ReadForm::Ranges,
            ranges
                .into_iter()
                .map(|lines| ReadItem::Lines(path.to_owned(), lines))
                .collect(),
        )
    })
}

/// The per-state counts a batch read's summary line reports.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReadCounts {
    /// Distinct items requested (an exact duplicate counts once).
    pub items: usize,
    /// Distinct files those items name, whether or not they could be read.
    pub files: usize,
    /// Items whose block is in the reply.
    pub delivered: usize,
    /// Items answered `unchanged since your read` instead of their block.
    pub unchanged: usize,
    /// Items left out of the reply and named in the follow-up footer.
    pub cut: usize,
    /// Items answered with an in-place failure line (unknown symbol, missing file, …).
    pub failed: usize,
}

/// The first line of every batch read reply, ending with a newline:
/// `read: <n> items in <f> files — <d> delivered, <u> unchanged, <c> not included, <x> failed`.
pub(crate) fn read_summary(counts: &ReadCounts) -> String {
    format!(
        "read: {} items in {} files — {} delivered, {} unchanged, {} not included, {} failed\n",
        counts.items, counts.files, counts.delivered, counts.unchanged, counts.cut, counts.failed
    )
}

/// The footer naming the items a batch read left out, with the exact call that fetches them.
///
/// `cut` holds the left-out items in request order and must not be empty. At most eight are named
/// and put into the call; more end the line with `; +N more`. The call keeps the request's own
/// form: `{"items":[…]}`, `{"symbols":[…]}`, or `{"path":…,"ranges":[…]}` with the path of the
/// cut ranges. The line ends with a newline.
pub(crate) fn read_follow_up(form: ReadForm, cut: &[&ReadItem]) -> String {
    let shown = &cut[..cut.len().min(MAX_FOOTER_NAMES)];
    let names = shown
        .iter()
        .map(|item| match (form, item) {
            (ReadForm::Ranges, ReadItem::Lines(_, lines)) => lines.clone(),
            _ => item.label(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let quoted = |values: Vec<String>| {
        values
            .iter()
            .map(|value| Value::String(value.clone()).to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let call = match form {
        ReadForm::Items => format!(
            "{{\"items\":[{}]}}",
            shown
                .iter()
                .map(|item| item.call_entry().to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
        ReadForm::Symbols => format!(
            "{{\"symbols\":[{}]}}",
            quoted(shown.iter().map(|item| item.label()).collect())
        ),
        ReadForm::Ranges => format!(
            "{{\"path\":{},\"ranges\":[{}]}}",
            Value::String(shown[0].file().to_owned()),
            quoted(
                shown
                    .iter()
                    .map(|item| match item {
                        ReadItem::Lines(_, lines) => lines.clone(),
                        other => other.label(),
                    })
                    .collect()
            )
        ),
    };
    let mut footer =
        format!("not included (over the reply budget): {names} — call ide.read {call}");
    let hidden = cut.len() - shown.len();
    if hidden > 0 {
        footer.push_str(&format!("; +{hidden} more"));
    }
    footer.push('\n');
    footer
}

/// Most delivered blocks one session's [`ReadMemory`] keeps; past it all read history resets.
/// Repeated blocks are delivered again, and windows without remembered rows echo what they replace.
const MAX_REMEMBERED_BLOCKS: usize = 512;

/// One session's memory of the blocks `ide.read {items}` delivered (batch-api-v1 §2), so a
/// repeated item whose bytes are identical answers `unchanged since your read <ref>` instead of
/// its text.
///
/// The memory belongs to one context epoch of the session (session-outbox-v1 §9.1): a different
/// epoch — the host compacted or cleared the agent's context — drops every entry, and an unknown
/// epoch (`None`) disables the memory entirely, because nothing proves the model still holds an
/// earlier read. The owner drops the whole memory when the session stops.
#[derive(Debug, Default)]
pub(crate) struct ReadMemory {
    /// The epoch the entries were delivered in.
    epoch: Option<u64>,
    /// Per item label: the digest of the delivered block and the `detail_ref` that delivered it.
    blocks: std::collections::BTreeMap<String, ([u8; 32], String)>,
    /// Per `file#unit` address: the unit's whole text as the session last read it, the base of
    /// the window-edit guard (symbol-read-edit-v1 §3.2). Dropped with the epoch like `blocks`.
    units: std::collections::BTreeMap<String, RememberedUnit>,
    /// Ordering of explicit unit reads, including partial reads forgotten after an own write.
    unit_order: std::collections::BTreeMap<String, u64>,
    /// Bounded file snapshots; unit texts are sliced only when an edit asks for their guard.
    files: std::collections::VecDeque<RememberedFile>,
    /// Monotonic read order within this memory, independent of native call identity.
    sequence: u64,
}

/// Most unit texts the memory keeps; a full memory starts over rather than growing.
const MAX_REMEMBERED_UNITS: usize = 128;

/// At most thirty-two 1 MiB source snapshots, covering every item of one maximum batch.
const MAX_REMEMBERED_FILES: usize = 32;

/// One delivered file snapshot with per-unit coverage and a single shared source allocation.
#[derive(Debug)]
struct RememberedFile {
    /// Worktree-relative file, identical to the file in canonical guard addresses.
    file: std::path::PathBuf,
    /// The exact read bytes, shared with other snapshots of unchanged source.
    source: std::sync::Arc<str>,
    /// Address geometry for those bytes, never reconstructed from a later file version.
    map: crate::lang::address::AddressMap,
    /// Unit-relative delivered intervals; `None` fences a partial read forgotten after a write.
    coverage: std::collections::BTreeMap<String, Option<crate::lang::LineRange>>,
    /// Own-write text for changed, fully known units; untouched external changes stay in source.
    /// Bounded by the explicit-unit ceiling, without retaining one whole file per own edit.
    overrides: std::collections::BTreeMap<String, String>,
    /// Order of this read relative to explicit address reads.
    sequence: u64,
}

impl ReadMemory {
    /// Returns the `detail_ref` of the earlier read that delivered exactly `block` for `item` in
    /// this same context epoch, or `None` (unknown epoch, new epoch, unknown item, changed bytes).
    /// A new epoch clears the memory.
    pub(crate) fn delivered_before(
        &mut self,
        epoch: Option<u64>,
        item: &str,
        block: &str,
    ) -> Option<String> {
        let epoch = epoch?;
        if self.epoch != Some(epoch) {
            self.blocks.clear();
            self.units.clear();
            self.unit_order.clear();
            self.files.clear();
            self.epoch = Some(epoch);
            return None;
        }
        let digest = *blake3::hash(block.as_bytes()).as_bytes();
        self.blocks
            .get(item)
            .filter(|(known, _)| *known == digest)
            .map(|(_, reference)| reference.clone())
    }

    /// Records that `reference` delivered `block` for `item` in `epoch`; nothing is recorded for
    /// an unknown epoch.
    pub(crate) fn remember(
        &mut self,
        epoch: Option<u64>,
        item: &str,
        block: &str,
        reference: &str,
    ) {
        let Some(epoch) = epoch else {
            return;
        };
        if self.epoch != Some(epoch) || self.blocks.len() >= MAX_REMEMBERED_BLOCKS {
            self.blocks.clear();
            self.units.clear();
            self.unit_order.clear();
            self.files.clear();
            self.epoch = Some(epoch);
        }
        self.blocks.insert(
            item.to_owned(),
            (
                *blake3::hash(block.as_bytes()).as_bytes(),
                reference.to_owned(),
            ),
        );
    }

    /// Records the text of unit `address` (`file#unit`) as read in `epoch`, with the 1-based unit
    /// lines `first..=last` the read delivered; nothing is recorded for an unknown epoch. A new
    /// epoch or a full memory starts over.
    pub(crate) fn remember_unit(
        &mut self,
        epoch: Option<u64>,
        address: &str,
        text: String,
        first: u32,
        last: u32,
    ) {
        let Some(epoch) = epoch else {
            return;
        };
        if self.epoch != Some(epoch) {
            self.blocks.clear();
            self.units.clear();
            self.unit_order.clear();
            self.files.clear();
            self.epoch = Some(epoch);
        }
        if self.unit_order.len() >= MAX_REMEMBERED_UNITS && !self.unit_order.contains_key(address) {
            self.units.clear();
            self.unit_order.clear();
            self.files.clear();
        }
        self.sequence = self.sequence.saturating_add(1);
        self.unit_order.insert(address.to_owned(), self.sequence);
        self.units
            .insert(address.to_owned(), RememberedUnit { text, first, last });
    }

    /// Explicit address-read entry for `address` in the same `epoch`, without file overlays.
    /// Unknown/different epochs or absent explicit entries return `None`. Full last-read
    /// semantics are implemented by [`Self::latest_unit_text`].
    pub(crate) fn unit_text(&self, epoch: Option<u64>, address: &str) -> Option<&RememberedUnit> {
        if epoch.is_none() || self.epoch != epoch {
            return None;
        }
        self.units.get(address)
    }

    /// Remembers delivered inclusive file `range` in `source` and its exact `map` for `file`.
    /// Unknown epochs record nothing; a new epoch resets all reads. Only intersecting units
    /// gain coverage, without copying their text or spending the explicit 128-unit quota.
    /// Thirty-two source snapshots are retained in read order, sharing identical file bytes.
    pub(crate) fn remember_file(
        &mut self,
        epoch: Option<u64>,
        file: &std::path::Path,
        source: &str,
        map: crate::lang::address::AddressMap,
        range: crate::lang::LineRange,
    ) {
        let Some(epoch) = epoch else { return };
        if self.epoch != Some(epoch) {
            self.blocks.clear();
            self.units.clear();
            self.unit_order.clear();
            self.files.clear();
            self.epoch = Some(epoch);
        }
        let coverage = map
            .units()
            .iter()
            .filter(|unit| range.start.max(unit.span.start) <= range.end.min(unit.span.end))
            .map(|unit| {
                let first = range.start.max(unit.span.start);
                let last = range.end.min(unit.span.end);
                (
                    crate::lang::address::Address::new(
                        Some(file.to_owned()),
                        unit.address.segments().to_vec(),
                        None,
                    )
                    .to_string(),
                    Some(crate::lang::LineRange::new(
                        first - unit.span.start + 1,
                        last - unit.span.start + 1,
                    )),
                )
            })
            .collect();
        let source = self
            .files
            .iter()
            .rev()
            .find(|read| read.file == file && read.source.as_ref() == source)
            .map(|read| std::sync::Arc::clone(&read.source))
            .unwrap_or_else(|| std::sync::Arc::from(source));
        self.sequence = self.sequence.saturating_add(1);
        if self.files.len() == MAX_REMEMBERED_FILES {
            self.files.pop_front();
        }
        self.files.push_back(RememberedFile {
            file: file.to_owned(),
            source,
            map,
            coverage,
            overrides: std::collections::BTreeMap::new(),
            sequence: self.sequence,
        });
    }

    /// Returns the latest delivered unit text from explicit address reads or file snapshots.
    /// Only the requested unit is copied. Unknown epochs and forgotten partial reads return
    /// `None`; overlapping file ranges use the last read of that unit, not the last file read.
    pub(crate) fn latest_unit_text(
        &self,
        epoch: Option<u64>,
        address: &str,
    ) -> Option<RememberedUnit> {
        if epoch.is_none() || self.epoch != epoch {
            return None;
        }
        let explicit = self.unit_order.get(address).copied().unwrap_or(0);
        for read in self.files.iter().rev() {
            if read.sequence <= explicit {
                break;
            }
            let Some(coverage) = read.coverage.get(address) else {
                continue;
            };
            let coverage = (*coverage)?;
            let crate::lang::address::Parsed::Address(parsed) =
                crate::lang::address::Address::parse(address).ok()?
            else {
                return None;
            };
            let resolved = read.map.resolve(&parsed).ok()?;
            return Some(RememberedUnit {
                text: read
                    .overrides
                    .get(address)
                    .cloned()
                    .or_else(|| read.source.get(resolved.bytes).map(str::to_owned))?,
                first: coverage.start,
                last: coverage.end,
            });
        }
        self.unit_text(epoch, address).map(|read| RememberedUnit {
            text: read.text.clone(),
            first: read.first,
            last: read.last,
        })
    }

    /// Refreshes fully delivered file units only when their remembered text matches the write base.
    /// `map`/`source` are the post-write bytes, `before_map`/`before` the verified pre-write base.
    /// Externally changed units retain their old baseline. Known partial or removed units are
    /// forgotten explicitly; own-write overlays retain only unit text and have a bounded quota.
    pub(crate) fn refresh_file_reads(
        &mut self,
        epoch: Option<u64>,
        file: &str,
        map: &crate::lang::address::AddressMap,
        source: &str,
        before_map: &crate::lang::address::AddressMap,
        before: &str,
    ) {
        if epoch.is_none() || self.epoch != epoch {
            return;
        }
        for read in self
            .files
            .iter_mut()
            .filter(|read| read.file == std::path::Path::new(file))
        {
            for (address, coverage) in &mut read.coverage {
                let Some(old) = *coverage else { continue };
                let Ok(crate::lang::address::Parsed::Address(parsed)) =
                    crate::lang::address::Address::parse(address)
                else {
                    continue;
                };
                let Ok(prior) = read.map.resolve(&parsed) else {
                    continue;
                };
                let Some(remembered) = read
                    .overrides
                    .get(address)
                    .map(String::as_str)
                    .or_else(|| read.source.get(prior.bytes))
                else {
                    continue;
                };
                let matches_base = before_map
                    .resolve(&parsed)
                    .ok()
                    .and_then(|base| before.get(base.bytes))
                    == Some(remembered);
                if !matches_base {
                    continue;
                }
                if old.start != 1 || old.end < crate::lang::line_count(remembered) {
                    *coverage = None;
                    read.overrides.remove(address);
                    continue;
                }
                let Some((new_text, length)) = map
                    .resolve(&parsed)
                    .ok()
                    .and_then(|next| source.get(next.bytes).map(|text| (text, next.span.len())))
                else {
                    *coverage = None;
                    read.overrides.remove(address);
                    continue;
                };
                if new_text == remembered {
                    continue;
                }
                // ponytail: bounded unit overlays; reread if a write changes over 128 known units.
                if read.overrides.len() >= MAX_REMEMBERED_UNITS
                    && !read.overrides.contains_key(address)
                {
                    continue;
                }
                read.overrides.insert(address.clone(), new_text.to_owned());
                *coverage = Some(crate::lang::LineRange::new(1, length));
            }
        }
    }

    /// Refreshes the remembered units of file `file` after this session's own write: a unit
    /// whose remembered text equals the verified `before` base takes the text `current` gives
    /// (or is forgotten when removed). Foreign changes preserve the old guard; known partial
    /// units are forgotten. These callbacks return whole unit text and its line count.
    pub(crate) fn refresh_file_units(
        &mut self,
        epoch: Option<u64>,
        file: &str,
        current: &dyn Fn(&str) -> Option<(String, u32)>,
        before: &dyn Fn(&str) -> Option<(String, u32)>,
    ) {
        if epoch.is_none() || self.epoch != epoch {
            return;
        }
        let prefix = format!("{file}#");
        let keys: Vec<String> = self
            .units
            .keys()
            .filter(|key| key.starts_with(&prefix))
            .cloned()
            .collect();
        for key in keys {
            let Some(old) = self.units.get(&key) else {
                continue;
            };
            if !before(&key).is_some_and(|(base, _)| base == old.text) {
                continue;
            }
            let whole = old.first == 1 && old.last >= old.text.lines().count() as u32;
            match (whole, current(&key)) {
                (true, Some((text, length))) => {
                    self.units.insert(
                        key,
                        RememberedUnit {
                            text,
                            first: 1,
                            last: length,
                        },
                    );
                }
                _ => {
                    self.units.remove(&key);
                }
            }
        }
    }
}

/// One unit as a session last read it: its whole text at that time and the 1-based unit lines
/// the read delivered (`first..=last`). Only those lines were seen by the agent.
#[derive(Debug)]
pub(crate) struct RememberedUnit {
    /// The unit's whole text when read.
    pub(crate) text: String,
    /// First unit line the read delivered.
    pub(crate) first: u32,
    /// Last unit line the read delivered.
    pub(crate) last: u32,
}

/// The one line that replaces an unchanged block:
/// `unchanged since your read <ref>: <item> (<n> lines); "force":true returns the text`, where
/// `<n>` counts the block's numbered lines (its header line excluded).
pub(crate) fn unchanged_line(reference: &str, item: &str, block: &str) -> String {
    let lines = block.lines().count().saturating_sub(1);
    format!(
        "unchanged since your read {reference}: {item} ({lines} lines); \"force\":true returns the text\n"
    )
}

/// Most files one `ide.edit {files}` request may name.
pub(crate) const MAX_EDIT_FILES: usize = 16;

/// Most changes one `ide.edit {files}` request may carry across all its files.
pub(crate) const MAX_EDIT_CHANGES_TOTAL: usize = 128;

/// Most bytes all `content` and `new` strings of one `ide.edit {files}` request carry together.
pub(crate) const MAX_EDIT_TEXT_BYTES: usize = 128 * 1024;

/// The closed entry shape a `files` entry must have, quoted in its refusal.
const FILE_SHAPE: &str = "an array of 1 to 16 entries, each with \"path\" and exactly one of \
\"changes\" or \"content\" (\"create\":true only with \"content\")";

/// Validates one `ide.edit {operation_id, source_ref?, files}` request object.
///
/// Only `operation_id` (the facade fills a missing one in), a batch default `source_ref` and `files` may appear. `files`
/// holds 1 to [`MAX_EDIT_FILES`] objects with unique paths (one leading `/` of an absolute
/// location is accepted; the worker makes it worktree-relative), each exactly one of `changes`
/// (validated exactly as the one-file `changes` form with the entry's or the batch's
/// `source_ref`), `content` with `create: true` (validated as the creation form) or `content`
/// alone (a whole-content replacement of the file's current bytes; `source_ref` is optional). Across all files at most
/// [`MAX_EDIT_CHANGES_TOTAL`] changes and [`MAX_EDIT_TEXT_BYTES`] of `content` and `new` text are
/// accepted. The one-file rules run through the facade's own validation of the equivalent
/// one-file request, so both forms refuse the same entries the same way; nothing is observed.
pub(crate) fn validate_edit_files(object: &Map<String, Value>) -> Result<(), ParameterError> {
    use super::facade::validate_call;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "operation_id" | "source_ref" | "files"))
    {
        return Err(invalid_field(
            "files",
            FieldRule::OneOf("used only with an optional \"operation_id\""),
        ));
    }
    let operation_id = object
        .get("operation_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| invalid_field("operation_id", FieldRule::Required))?;
    if operation_id.len() > 128 {
        return Err(invalid_field("operation_id", FieldRule::TooLong(128)));
    }
    let default_ref = match object.get("source_ref") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .filter(|reference| !reference.is_empty() && reference.len() <= 128)
                .ok_or_else(|| invalid_field("source_ref", FieldRule::NonEmptyString))?,
        ),
    };
    let shape = || invalid_field("files", FieldRule::OneOf(FILE_SHAPE));
    let files = object
        .get("files")
        .and_then(Value::as_array)
        .filter(|files| (1..=MAX_EDIT_FILES).contains(&files.len()))
        .ok_or_else(shape)?;
    let mut paths = BTreeSet::new();
    let mut changes_total = 0usize;
    let mut text_bytes = 0usize;
    for entry in files {
        let entry = entry.as_object().ok_or_else(shape)?;
        if entry.keys().any(|key| {
            !matches!(
                key.as_str(),
                "path" | "source_ref" | "changes" | "content" | "create"
            )
        }) {
            return Err(shape());
        }
        let path = entry
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty() && path.len() <= 1024)
            .ok_or_else(shape)?;
        // Absolute spellings reach the worker's canonical containment guard unchanged. The
        // worker also compares filesystem destinations after whole-batch path admission.
        if let Some(rule) = edit_path_shape_rule(path) {
            return Err(invalid_field("files", rule));
        }
        if !paths.insert(path) {
            return Err(invalid_field(
                "files",
                FieldRule::OneOf("entries naming each path once"),
            ));
        }
        let reference = match entry.get("source_ref") {
            None => default_ref,
            Some(value) => Some(
                value
                    .as_str()
                    .filter(|reference| !reference.is_empty() && reference.len() <= 128)
                    .ok_or_else(|| invalid_field("source_ref", FieldRule::NonEmptyString))?,
            ),
        };
        let create = match entry.get("create") {
            None => false,
            Some(Value::Bool(create)) => *create,
            Some(_) => return Err(invalid_field("create", FieldRule::Boolean)),
        };
        let mut single = json!({"operation_id": operation_id, "path": path});
        match (entry.get("changes"), entry.get("content")) {
            (Some(changes), None) if !create => {
                let list = changes.as_array().ok_or_else(shape)?;
                changes_total += list.len();
                text_bytes += list
                    .iter()
                    .flat_map(|change| ["content", "new"].map(|key| change.get(key)))
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::len)
                    .sum::<usize>();
                single["changes"] = changes.clone();
                if let Some(reference) = reference {
                    single["source_ref"] = json!(reference);
                }
            }
            (None, Some(content)) => {
                let text = content
                    .as_str()
                    .ok_or_else(|| invalid_field("content", FieldRule::String))?;
                text_bytes += text.len();
                single["content"] = content.clone();
                if let (false, Some(reference)) = (create, reference) {
                    single["source_ref"] = json!(reference);
                }
            }
            _ => return Err(shape()),
        }
        validate_call(AssistanceTool::Edit, single)?;
    }
    if changes_total > MAX_EDIT_CHANGES_TOTAL {
        return Err(invalid_field(
            "files",
            FieldRule::OneOf("at most 128 changes across all files"),
        ));
    }
    if text_bytes > MAX_EDIT_TEXT_BYTES {
        return Err(invalid_field(
            "files",
            FieldRule::TooLong(MAX_EDIT_TEXT_BYTES),
        ));
    }
    Ok(())
}

/// What one `files` entry asks for.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum EditBody {
    /// `changes`: the validated change objects, applied as the one-file `changes` form does.
    Changes(Vec<Value>),
    /// `content` with `create: true`: the content of a file that must not exist yet.
    Create(String),
    /// `content` alone: the whole new content of an existing file.
    Replace(String),
    /// A whole-content replacement computed from bytes the worker read earlier (an applied
    /// `apply_patch`): it is written only while the file still has those bytes, `digest` being
    /// their blake3 hex. A file that changed meanwhile is refused as changed while the edit was
    /// applied. Only the worker builds it (the `expect` key is never accepted from a caller).
    ReplaceIfUnchanged {
        /// The new whole content.
        content: String,
        /// Blake3 hex digest of the bytes `content` was computed from.
        digest: String,
    },
}

/// One `files` entry of a validated multi-file edit, in request order.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EditFile {
    /// Worktree-relative path exactly as requested.
    pub path: String,
    /// The entry's own `source_ref`, else the batch default; `None` when neither was given.
    pub source_ref: Option<String>,
    /// The requested change of this file.
    pub body: EditBody,
}

/// Reads a validated `ide.edit {files}` request back into its operation id and typed files.
///
/// Returns `None` when the request is not a multi-file edit (no `files` array or no
/// `operation_id`). The facade has validated every shape; an entry that is nevertheless
/// malformed makes the whole request `None` rather than silently dropping a file.
pub(crate) fn edit_files_request(parameters: &Value) -> Option<(String, Vec<EditFile>)> {
    let operation_id = parameters.get("operation_id")?.as_str()?.to_owned();
    let default_ref = parameters.get("source_ref").and_then(Value::as_str);
    let files = parameters
        .get("files")?
        .as_array()?
        .iter()
        .map(|entry| {
            let path = entry.get("path")?.as_str()?.to_owned();
            let source_ref = entry
                .get("source_ref")
                .and_then(Value::as_str)
                .or(default_ref)
                .map(str::to_owned);
            let create = entry.get("create").and_then(Value::as_bool) == Some(true);
            let body = match (entry.get("changes"), entry.get("content")) {
                (Some(changes), None) => EditBody::Changes(changes.as_array()?.clone()),
                (None, Some(content)) if create => EditBody::Create(content.as_str()?.to_owned()),
                (None, Some(content)) => match entry.get("expect").and_then(Value::as_str) {
                    Some(digest) => EditBody::ReplaceIfUnchanged {
                        content: content.as_str()?.to_owned(),
                        digest: digest.to_owned(),
                    },
                    None => EditBody::Replace(content.as_str()?.to_owned()),
                },
                _ => return None,
            };
            Some(EditFile {
                path,
                source_ref,
                body,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some((operation_id, files))
}

/// Digest of the caller's own multi-file edit arguments (`operation_id`, the batch
/// `source_ref`, `files`, or the original `patch`) — never derived patch candidates — so a retry, even
/// after a daemon restart, matches its durable receipt and a changed request conflicts.
pub(crate) fn edit_request_digest(parameters: &Value) -> [u8; 32] {
    // serde_json keeps object keys sorted, so equal requests serialize to equal bytes.
    let canonical = if parameters.get("patch").is_some() {
        // A replay derives different full-file candidates after earlier writes; the caller's
        // original patch remains its stable input, while the receipt retains candidate digests.
        json!({"operation_id": parameters.get("operation_id"), "patch": parameters.get("patch")})
    } else {
        json!({
            "operation_id": parameters.get("operation_id"),
            "source_ref": parameters.get("source_ref"),
            "files": parameters.get("files"),
        })
    };
    let mut hash = blake3::Hasher::new();
    hash.update(b"changes-batch-edit-request-v1");
    hash.update(canonical.to_string().as_bytes());
    *hash.finalize().as_bytes()
}

/// Digest of a rename request (`operation_id`, `symbol`, `new_name`), the receipt key that
/// lets a retry of a rename stopped midway finish it and a different rename under the same
/// `operation_id` conflict.
pub(crate) fn rename_request_digest(parameters: &Value) -> [u8; 32] {
    let canonical = json!({
        "operation_id": parameters.get("operation_id"),
        "symbol": parameters.get("symbol"),
        "new_name": parameters.get("new_name"),
    });
    let mut hash = blake3::Hasher::new();
    hash.update(b"changes-rename-request-v1");
    hash.update(canonical.to_string().as_bytes());
    *hash.finalize().as_bytes()
}

/// Why a file of a multi-file edit was not written.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NotWritten {
    /// Its bytes no longer match the bytes it was validated against.
    ChangedAfterValidation,
    /// The write failed or could not be established (I/O, deadline, cancellation, authority).
    IoError,
    /// The commit stopped at an earlier file.
    NotReached,
}

impl NotWritten {
    /// The reason's closed word in replies (`changed after validation`, …).
    pub(crate) const fn words(self) -> &'static str {
        match self {
            Self::ChangedAfterValidation => "changed after validation",
            Self::IoError => "write failed",
            Self::NotReached => "not reached",
        }
    }
}

/// The settled state of one file of a multi-file edit after this call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FileState {
    /// The file did not exist and was created.
    Created,
    /// The file's bytes were replaced.
    Replaced,
    /// The candidate equalled the current bytes; nothing was written.
    Unchanged,
    /// The file was written by an earlier call with the same operation id; `created` when the
    /// file did not exist before the operation (its recorded base is an absence).
    WrittenEarlier {
        /// True when the operation created the file.
        created: bool,
    },
    /// The file was not written; the reason says why.
    NotWritten(NotWritten),
    /// A write may have happened but its result could not be established.
    Unknown,
}

impl FileState {
    /// True when the file now holds this operation's candidate.
    pub(crate) const fn written(self) -> bool {
        matches!(
            self,
            Self::Created | Self::Replaced | Self::Unchanged | Self::WrittenEarlier { .. }
        )
    }
}

/// One file's line in a multi-file edit reply.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FileReport {
    /// Worktree-relative path as requested.
    pub path: String,
    /// What happened to the file in this call.
    pub state: FileState,
    /// The first line of the file's landing note (where its changes landed), when written.
    pub note: Option<String>,
    /// The file's diagnostics in a few words (`clean`, `2 new problems`, `unknown`, …).
    pub diagnostics: Option<String>,
}

/// Renders the whole reply text of one multi-file edit, summary line first.
///
/// `files` holds every requested file in request order. When all are written the first line is
/// `edit: applied <w> of <n> files; diagnostics: <d>`;
/// otherwise it is `edit: partial — written <w> of <n>; not written: <k> <path> (<reason>), …;
/// the same operation_id resumes the rest`, followed by a `diagnostics: <d>` line when any file
/// is written. No line names a file-version reference. One line per file follows
/// (`  <k> <path>: <state>[; <note>][; diagnostics <d>]`), then at most eight `new:` problem lines.
/// Every line states a fact; none names a waiting or repeating instruction.
pub(crate) fn render_edit_reply(
    files: &[FileReport],
    diagnostics: &str,
    new_lines: &[String],
) -> String {
    let written = files.iter().filter(|file| file.state.written()).count();
    let total = files.len();
    let mut text = if written == total {
        format!("edit: applied {written} of {total} files; diagnostics: {diagnostics}\n")
    } else {
        let missing = files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| match file.state {
                FileState::NotWritten(reason) => {
                    Some(format!("{} {} ({})", index + 1, file.path, reason.words()))
                }
                FileState::Unknown => Some(format!("{} {} (unknown)", index + 1, file.path)),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut head = if files.iter().any(|file| file.state == FileState::Unknown) {
            format!(
                "edit: partial — written {written} of {total}; not written or possibly written: \
                 {missing}; the same operation_id resumes the rest from the recorded digests\n"
            )
        } else {
            format!(
                "edit: partial — written {written} of {total}; not written: {missing}; the same \
                 operation_id resumes the rest\n"
            )
        };
        if written > 0 {
            head.push_str(&format!("diagnostics: {diagnostics}\n"));
        }
        head
    };
    for (index, file) in files.iter().enumerate() {
        let state = match file.state {
            FileState::Created => "created".to_owned(),
            FileState::Replaced => "replaced".to_owned(),
            FileState::Unchanged => "unchanged".to_owned(),
            FileState::WrittenEarlier { .. } => "written earlier by this operation_id".to_owned(),
            FileState::NotWritten(reason) => format!("not written ({})", reason.words()),
            FileState::Unknown => "unknown (the write may have happened)".to_owned(),
        };
        text.push_str(&format!("  {} {}: {state}", index + 1, file.path));
        if let Some(diagnostics) = &file.diagnostics {
            text.push_str(&format!("; diagnostics {diagnostics}"));
        }
        text.push('\n');
        // The file's changed lines (by address), one per line under the file's own line.
        if let Some(note) = &file.note {
            for line in note.lines() {
                text.push_str("  ");
                text.push_str(line);
                text.push('\n');
            }
        }
    }
    for line in new_lines.iter().take(8) {
        text.push_str(line);
        text.push('\n');
    }
    text
}

/// The closed outcome of a multi-file edit whose files ended in `files`: `outcome_unknown` when a
/// write may have happened unobserved, `applied` when every file holds its candidate, else
/// `partial`.
pub(crate) fn edit_outcome(files: &[FileReport]) -> EditBatchOutcome {
    if files.iter().any(|file| file.state == FileState::Unknown) {
        EditBatchOutcome::OutcomeUnknown
    } else if files.iter().all(|file| file.state.written()) {
        EditBatchOutcome::Applied
    } else {
        EditBatchOutcome::Partial
    }
}

/// The structured per-file states of a multi-file edit, request order: a file written by an
/// earlier call with the same operation id reads `created` or `replaced` as that call wrote it.
pub(crate) fn edit_batch_files(files: &[FileReport]) -> Vec<EditBatchFile> {
    files
        .iter()
        .map(|file| {
            let (state, reason) = match file.state {
                FileState::Created | FileState::WrittenEarlier { created: true } => {
                    (EditBatchFileState::Created, None)
                }
                FileState::Replaced | FileState::WrittenEarlier { created: false } => {
                    (EditBatchFileState::Replaced, None)
                }
                FileState::Unchanged => (EditBatchFileState::Unchanged, None),
                FileState::NotWritten(reason) => (
                    EditBatchFileState::NotWritten,
                    Some(match reason {
                        NotWritten::ChangedAfterValidation => {
                            EditBatchReason::ChangedAfterValidation
                        }
                        NotWritten::IoError => EditBatchReason::IoError,
                        NotWritten::NotReached => EditBatchReason::NotReached,
                    }),
                ),
                FileState::Unknown => (EditBatchFileState::Unknown, None),
            };
            EditBatchFile {
                path: file.path.clone(),
                state,
                reason,
            }
        })
        .collect()
}

/// The complete reply of one multi-file edit (batch-api-v1 §7): the compact text of
/// [`render_edit_reply`] and the structured `operation_id`, outcome, `source_ref` (only when a
/// file holds its candidate) and per-file states.
pub(crate) fn edit_batch_reply(
    operation_id: &str,
    files: &[FileReport],
    source_ref: Option<&str>,
    diagnostics: &str,
    new_lines: &[String],
) -> PeerReply {
    let source_ref = source_ref.filter(|_| files.iter().any(|file| file.state.written()));
    PeerReply::EditBatch {
        operation_id: operation_id.to_owned(),
        outcome: edit_outcome(files),
        source_ref: source_ref.map(str::to_owned),
        files: edit_batch_files(files),
        text: render_edit_reply(files, diagnostics, new_lines),
        truncated: false,
    }
}

/// The reply to a reused `operation_id` whose recorded request differs: outcome
/// `conflicting_duplicate`, no file touched, nothing written. The text is a head and one Repair
/// line (harness-reply-v1 R5) and does not echo the id, which can be 128 bytes long and which the
/// caller already holds; the structured copy still carries it.
pub(crate) fn conflicting_duplicate_reply(operation_id: &str) -> PeerReply {
    PeerReply::EditBatch {
        operation_id: operation_id.to_owned(),
        outcome: EditBatchOutcome::ConflictingDuplicate,
        source_ref: None,
        files: Vec::new(),
        text: "This operation_id already names a different multi-file edit; nothing was \
               written.\nRepair: Read the targets with ide.read, then edit with a new operation_id.\n"
            .to_owned(),
        truncated: false,
    }
}

/// The reply of a multi-file edit whose call ended before it could establish its writes (a panic
/// after a write): outcome `outcome_unknown` with every requested file `unknown`; the operation's
/// receipt settles each file on the retry.
pub(crate) fn unknown_batch_reply(operation_id: &str, paths: &[String]) -> PeerReply {
    let files = paths
        .iter()
        .map(|path| FileReport {
            path: path.clone(),
            state: FileState::Unknown,
            note: None,
            diagnostics: None,
        })
        .collect::<Vec<_>>();
    edit_batch_reply(operation_id, &files, None, "unknown", &[])
}

/// Builds the one `edit-checked` fact of a multi-file edit whose reply said `diagnostics:
/// pending` (session-outbox-v1 §3.3).
///
/// `files` pairs each checked file with its project-check diagnostics in request order;
/// `fences` are the written files with their post-write digests (at most 16). The summary is
/// `<K> files, <N> new errors, <M> new warnings`, `<K> files, clean` or `<K> files, diagnostics
/// unknown`; the detail has one line per file (at most 16, then `+K more files`) and at most
/// eight `new:` lines, cut to the fact budget. New problems make the fact a failure, and their
/// compiler text makes it source-bearing, so it is re-proved before it is handed to Bridge.
pub(crate) fn edit_checked_draft(
    operation_id: &str,
    reference: &str,
    files: &[(String, super::reply::EditDiagnostics)],
    fences: Vec<super::outbox::FencedPath>,
) -> super::outbox::FactDraft {
    use super::outbox::{
        FactDraft, FactKind, MAX_FACT_BYTES, MAX_SUBJECT_BYTES, MAX_SUMMARY_BYTES,
    };
    use super::reply::{EditDiagnostics, bounded_utf8_prefix};
    let (mut errors, mut warnings, mut unknown, mut unavailable) = (0usize, 0usize, false, false);
    let mut lines = Vec::new();
    let mut new_lines = Vec::new();
    for (index, (path, diagnostics)) in files.iter().enumerate() {
        let words = match diagnostics {
            EditDiagnostics::CurrentClean { .. } => "clean".to_owned(),
            EditDiagnostics::CurrentReported { messages, .. } => {
                let fresh = messages
                    .iter()
                    .filter(|message| message.starts_with("new: "))
                    .collect::<Vec<_>>();
                let file_errors = fresh.iter().filter(|line| line.contains(" error")).count();
                let file_warnings = fresh.len() - file_errors;
                errors += file_errors;
                warnings += file_warnings;
                new_lines.extend(fresh.into_iter().cloned());
                format!("{file_errors} new errors, {file_warnings} new warnings")
            }
            EditDiagnostics::NotAnalysed { reason } => {
                unavailable = true;
                format!("not analysed ({reason})")
            }
            EditDiagnostics::Unknown {} | EditDiagnostics::Pending { .. } => {
                unknown = true;
                "unknown".to_owned()
            }
        };
        if index < 16 {
            lines.push(format!("  {path}: {words}"));
        }
    }
    if files.len() > 16 {
        lines.push(format!("  +{} more files", files.len() - 16));
    }
    lines.extend(new_lines.iter().take(8).map(|line| format!("  {line}")));
    let summary = if errors + warnings > 0 {
        format!(
            "{} files, {errors} new errors, {warnings} new warnings",
            files.len()
        )
    } else if unknown {
        format!("{} files, diagnostics unknown", files.len())
    } else if unavailable {
        format!("{} files, checks unavailable or not analysed", files.len())
    } else {
        format!("{} files, clean", files.len())
    };
    let summary = bounded_utf8_prefix(&summary, MAX_SUMMARY_BYTES);
    let detail = (!lines.is_empty())
        .then(|| bounded_utf8_prefix(&lines.join("\n"), MAX_FACT_BYTES - MAX_SUMMARY_BYTES - 64));
    FactDraft {
        kind: FactKind::EditChecked,
        subject: bounded_utf8_prefix(&format!("edit {operation_id}"), MAX_SUBJECT_BYTES),
        summary,
        detail,
        detail_ref: Some(reference.to_owned()),
        failed: errors > 0 || unknown || unavailable,
        source_bearing: !new_lines.is_empty(),
        fence_paths: fences,
    }
}

/// Summarizes one multi-file edit's per-file project-check diagnostics.
///
/// `files` pairs each checked file's path with its diagnostics; files never checked are left
/// out by the caller. Returns the summary words for the first line (`pending; …` while the check
/// still runs, `clean`, `<n> new problems in <f> files`, `unknown`, `not analysed`, or `none`
/// without any checked file), the per-file words in the same order, and at most eight `new:`
/// problem lines across all files.
pub(crate) fn summarize_diagnostics(
    files: &[(String, super::reply::EditDiagnostics)],
) -> (String, Vec<String>, Vec<String>) {
    use super::reply::EditDiagnostics;
    let mut words = Vec::with_capacity(files.len());
    let mut new_lines = Vec::new();
    let (mut problems, mut problem_files, mut unknown, mut clean, mut elsewhere) =
        (0usize, 0usize, false, false, 0u32);
    let mut pending = None;
    for (_, diagnostics) in files {
        words.push(match diagnostics {
            EditDiagnostics::CurrentClean { project_errors } => {
                clean = true;
                elsewhere = elsewhere.max(*project_errors);
                "clean".to_owned()
            }
            EditDiagnostics::CurrentReported {
                messages,
                truncated,
                ..
            } => {
                let fresh = messages
                    .iter()
                    .filter(|message| message.starts_with("new: "))
                    .collect::<Vec<_>>();
                if fresh.is_empty() {
                    clean = true;
                    "pre-existing problems only".to_owned()
                } else {
                    problems += fresh.len();
                    problem_files += 1;
                    for message in &fresh {
                        if new_lines.len() < 8 {
                            new_lines.push((*message).clone());
                        }
                    }
                    let plus = if *truncated { "+" } else { "" };
                    format!("{}{plus} new problems", fresh.len())
                }
            }
            EditDiagnostics::Unknown {} => {
                unknown = true;
                "unknown".to_owned()
            }
            EditDiagnostics::NotAnalysed { reason }
                if reason.starts_with("checks unavailable:") =>
            {
                reason.clone()
            }
            EditDiagnostics::NotAnalysed { .. } => "not analysed".to_owned(),
            EditDiagnostics::Pending { detail_ref } => {
                pending.get_or_insert(detail_ref.as_str());
                "pending".to_owned()
            }
        });
    }
    let summary = if let Some(reference) = pending {
        format!(
            "pending; project check retained under {reference}; Bridge delivers it automatically"
        )
    } else if problems > 0 {
        format!("{problems} new problems in {problem_files} files")
    } else if unknown {
        "unknown".to_owned()
    } else if clean && elsewhere > 0 {
        format!("clean in these files; the project check reports {elsewhere} errors elsewhere")
    } else if clean {
        "clean".to_owned()
    } else if files.is_empty() {
        "none (no checked language)".to_owned()
    } else {
        "not analysed".to_owned()
    };
    (summary, words, new_lines)
}

/// Most targets one `ide.test {targets}` request may carry.
pub(crate) const MAX_TEST_TARGETS: usize = 8;

/// The closed target shape a `targets` entry must have, quoted in its refusal.
const TARGET_SHAPE: &str = "an array of 1 to 8 targets, each exactly one of \
{\"symbol\":\"src/a.rs#parse\"}, {\"path\":\"tests/cli.rs\"}, {\"pattern\":\"roles_\"} or {\"command\":[\"cargo\",\"check\"]}";

/// One target of an `ide.test {targets}` request, in request order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TestTargetSpec {
    /// The tests that reference a symbol (`file#Owner/name` or a bare name).
    Symbol(String),
    /// Every test in one file or directory.
    Path(String),
    /// A raw filter for the language's runner.
    Pattern(String),
    /// An explicit argv executed at the worktree root, without shell interpretation.
    Command(Vec<String>),
}

impl TestTargetSpec {
    /// The target as its line names it: `symbol <address>`, `path <path>` or
    /// `pattern <filter>`.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Symbol(symbol) => format!("symbol {symbol}"),
            Self::Path(path) => format!("path {path}"),
            Self::Pattern(pattern) => format!("pattern {pattern}"),
            Self::Command(argv) => format!("command {}", super::worker::display_argv(argv)),
        }
    }
}

/// Validates one `ide.test {targets, budget_s?}` request object.
///
/// No single-target selector (`symbol`, `path`, `pattern`, `command`), result/cancel query,
/// `cwd` or `env` appears beside `targets`; `targets` holds 1 to [`MAX_TEST_TARGETS`] objects,
/// each exactly one of `{symbol}`, `{path}`, `{pattern}` or `{command}`, validated as the single form validates
/// that key together with the request's `budget_s`.
pub(crate) fn validate_test_targets(object: &Map<String, Value>) -> Result<(), ParameterError> {
    if [
        "symbol",
        "path",
        "pattern",
        "command",
        "status",
        "result",
        "cancel",
        "history",
        "detail_ref",
        "cwd",
        "env",
    ]
    .iter()
    .any(|field| object.contains_key(*field))
    {
        return Err(invalid_field(
            "targets",
            FieldRule::OneOf(
                "used instead of execution or result/history/output selectors, never beside them",
            ),
        ));
    }
    let shape = || invalid_field("targets", FieldRule::OneOf(TARGET_SHAPE));
    let targets = object
        .get("targets")
        .and_then(Value::as_array)
        .filter(|targets| (1..=MAX_TEST_TARGETS).contains(&targets.len()))
        .ok_or_else(shape)?;
    for target in targets {
        let entry = target
            .as_object()
            .filter(|entry| {
                entry.len() == 1
                    && entry.keys().all(|key| {
                        matches!(key.as_str(), "symbol" | "path" | "pattern" | "command")
                    })
            })
            .ok_or_else(shape)?;
        let mut single = entry.clone();
        if let Some(budget) = object.get("budget_s") {
            single.insert("budget_s".to_owned(), budget.clone());
        }
        super::facade::validate_call(AssistanceTool::Test, Value::Object(single))?;
    }
    Ok(())
}

/// The targets of a validated `ide.test {targets}` request in request order, an exactly
/// repeated target once; an entry of any other shape is skipped (the facade refused it).
pub(crate) fn test_targets(parameters: &Value) -> Vec<TestTargetSpec> {
    let mut targets: Vec<TestTargetSpec> = Vec::new();
    for entry in parameters
        .get("targets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let text = |key: &str| entry.get(key).and_then(Value::as_str).map(str::to_owned);
        let target = if let Some(symbol) = text("symbol") {
            TestTargetSpec::Symbol(symbol)
        } else if let Some(path) = text("path") {
            TestTargetSpec::Path(path)
        } else if let Some(pattern) = text("pattern") {
            TestTargetSpec::Pattern(pattern)
        } else if let Some(argv) = entry.get("command").and_then(Value::as_array) {
            TestTargetSpec::Command(
                argv.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            )
        } else {
            continue;
        };
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    targets
}

/// The phrases no batch reply line may contain (M024-M027-REPLY-HYGIENE r2 §2): a batch reply
/// states what happened and never invites the agent to poll or wait.
#[cfg(test)]
pub(crate) const POLLING_PHRASES: [&str; 5] = [
    "poll",
    "every few seconds",
    "in a few seconds",
    "until it completes",
    "again every",
];

/// Asserts that `text`, a line about unfinished work (running, pending, still active), carries no
/// seconds count such as `12 s` or `12s` (M024-M027-REPLY-HYGIENE r2 §2: no seconds on something
/// unfinished).
#[cfg(test)]
pub(crate) fn assert_no_unfinished_seconds(text: &str) {
    let bytes = text.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if !byte.is_ascii_digit() || bytes.get(index + 1).is_some_and(u8::is_ascii_digit) {
            continue;
        }
        let rest = text[index + 1..].trim_start_matches(' ');
        let seconds = rest
            .strip_prefix('s')
            .is_some_and(|after| !after.starts_with(|next: char| next.is_ascii_alphanumeric()));
        assert!(!seconds, "a seconds count on unfinished work: {text:?}");
    }
}

/// Asserts that `text` contains none of [`POLLING_PHRASES`], case-insensitively.
#[cfg(test)]
pub(crate) fn assert_no_polling(text: &str) {
    let lower = text.to_lowercase();
    for phrase in POLLING_PHRASES {
        assert!(!lower.contains(phrase), "{phrase:?} in {text:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Validates one `ide.read` items request through the same function the facade uses.
    fn check(request: Value) -> Result<(), ParameterError> {
        validate_read_items(request.as_object().expect("object request"))
    }

    #[test]
    fn items_accept_the_three_shapes_and_refuse_everything_else() {
        assert!(
            check(json!({"items":[
                {"symbol":"src/a.rs#Store/transcript"},
                {"path":"docs/api.md"},
                {"path":"src/b.rs","lines":"10-40"}],"force":true}))
            .is_ok()
        );
        for bad in [
            json!({"items":[]}),
            json!({"items":"src/a.rs"}),
            json!({"items":[{"symbol":"src/a.rs"}]}),
            json!({"items":[{"symbol":"src/a.rs#"}]}),
            json!({"items":[{"symbol":"Name"}]}),
            json!({"items":[{"symbol":"src/a.rs#A","path":"src/a.rs"}]}),
            json!({"items":[{"lines":"1-2"}]}),
            json!({"items":[{"path":"../x.rs"}]}),
            json!({"items":[{"path":"src/x.rs","lines":"9-2"}]}),
            json!({"items":[{"path":"src/x.rs","extra":1}]}),
            json!({"items":[{"path":""}]}),
            json!({"items":[{"path":"a.rs"}],"force":"yes"}),
        ] {
            assert!(check(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn items_refuse_single_and_legacy_keys_beside_them() {
        for key in ["symbol", "path", "lines", "symbols", "ranges"] {
            let mut request = json!({"items":[{"path":"a.md"}]});
            request[key] = json!("x");
            assert_eq!(check(request), Err(ParameterError::ReadTarget), "{key}");
        }
    }

    #[test]
    fn items_cap_entries_at_32_and_distinct_files_at_16() {
        let same_file = (0..32)
            .map(|line| json!({"path":"a.rs","lines":format!("{}-{}", line + 1, line + 1)}))
            .collect::<Vec<_>>();
        assert!(check(json!({ "items": same_file })).is_ok());
        let mut over = same_file.clone();
        over.push(json!({"path":"a.rs"}));
        assert!(check(json!({ "items": over })).is_err());
        let sixteen = (0..16)
            .map(|file| json!({"path": format!("f{file}.rs")}))
            .collect::<Vec<_>>();
        assert!(check(json!({ "items": sixteen.clone() })).is_ok());
        let mut seventeen = sixteen;
        seventeen.push(json!({"symbol":"f16.rs#Seventeenth"}));
        assert!(check(json!({ "items": seventeen })).is_err());
    }

    #[test]
    fn batch_forms_get_16_kib_and_single_forms_keep_4_kib() {
        use crate::assistance::facade::validate_call;
        let item =
            |index: usize| json!({"symbol": format!("src/{index:02}.rs#{}", "N".repeat(400))});
        let large = json!({ "items": (0..16).map(item).collect::<Vec<_>>() });
        let size = serde_json::to_vec(&large).unwrap().len();
        assert!(
            size > 4 * 1024 && size < MAX_BATCH_PARAMETER_BYTES,
            "{size}"
        );
        assert!(validate_call(AssistanceTool::Read, large).is_ok());
        let over = json!({ "items": (0..32).map(|i| json!({"symbol": format!("src/{}.rs#{}", i % 16, "N".repeat(600))})).collect::<Vec<_>>() });
        assert_eq!(
            validate_call(AssistanceTool::Read, over),
            Err(invalid_field(
                "items",
                FieldRule::TooLong(MAX_BATCH_PARAMETER_BYTES)
            ))
        );
        assert_eq!(
            validate_call(
                AssistanceTool::Read,
                json!({"symbol": format!("a.rs#{}", "N".repeat(5000))})
            ),
            Err(ParameterError::InvalidObject)
        );
        assert!(
            validate_call(AssistanceTool::Read, json!({"path":"a.md","force":true})).is_ok(),
            "legacy file reads normalize to the same force-aware items engine"
        );
        assert!(
            validate_call(
                AssistanceTool::Read,
                json!({"items":[{"path":"a.md"}],"force":false})
            )
            .is_ok()
        );
    }

    #[test]
    fn batch_keys_name_the_read_batch_forms_only() {
        assert_eq!(
            batch_key(AssistanceTool::Read, &json!({"items":[]})),
            Some("items")
        );
        assert_eq!(
            batch_key(AssistanceTool::Read, &json!({"symbols":[]})),
            Some("symbols")
        );
        assert_eq!(
            batch_key(AssistanceTool::Read, &json!({"path":"a","ranges":[]})),
            Some("ranges")
        );
        assert_eq!(batch_key(AssistanceTool::Read, &json!({"path":"a"})), None);
        assert_eq!(
            batch_key(AssistanceTool::Outline, &json!({"path":"a"})),
            None
        );
    }

    #[test]
    fn requests_read_back_in_order_with_their_form() {
        let (form, items) = read_request(&json!({"items":[
            {"symbol":"a.rs#A"},{"path":"b.md"},{"path":"c.rs","lines":"3-4"}]}))
        .unwrap();
        assert_eq!(form, ReadForm::Items);
        assert_eq!(
            items,
            vec![
                ReadItem::Symbol("a.rs#A".into()),
                ReadItem::Path("b.md".into()),
                ReadItem::Lines("c.rs".into(), "3-4".into()),
            ]
        );
        assert_eq!(
            read_request(&json!({"path":"x.rs","ranges":["1-2"]})),
            Some((
                ReadForm::Ranges,
                vec![ReadItem::Lines("x.rs".into(), "1-2".into())]
            ))
        );
        assert_eq!(
            read_request(&json!({"symbols":["x.rs#A"]})).map(|(form, _)| form),
            Some(ReadForm::Symbols)
        );
        assert_eq!(read_request(&json!({"path":"x.rs","lines":"1-2"})), None);
    }

    #[test]
    fn items_name_their_files_and_labels() {
        let symbol = ReadItem::Symbol("src/a.rs#Store/next".into());
        assert_eq!(symbol.file(), "src/a.rs");
        assert_eq!(symbol.label(), "src/a.rs#Store/next");
        let lines = ReadItem::Lines("src/b.rs".into(), "10-40".into());
        assert_eq!(lines.file(), "src/b.rs");
        assert_eq!(lines.label(), "src/b.rs lines 10-40");
        assert_eq!(ReadItem::Symbol("notes.md".into()).file(), "notes.md");
    }

    #[test]
    fn the_summary_line_reports_every_state() {
        let counts = ReadCounts {
            items: 4,
            files: 3,
            delivered: 2,
            unchanged: 1,
            cut: 0,
            failed: 1,
        };
        assert_eq!(
            read_summary(&counts),
            "read: 4 items in 3 files — 2 delivered, 1 unchanged, 0 not included, 1 failed\n"
        );
    }

    #[test]
    fn follow_up_calls_keep_the_request_form_and_parse() {
        let items = [
            ReadItem::Symbol("a.rs#A".into()),
            ReadItem::Path("b \"q\".md".into()),
            ReadItem::Lines("c.rs".into(), "3-4".into()),
        ];
        let cut = items.iter().collect::<Vec<_>>();
        let footer = read_follow_up(ReadForm::Items, &cut);
        let call = footer.trim_end().split_once("call ide.read ").unwrap().1;
        let parsed: Value = serde_json::from_str(call).unwrap();
        assert_eq!(
            parsed,
            json!({"items":[{"symbol":"a.rs#A"},{"path":"b \"q\".md"},{"path":"c.rs","lines":"3-4"}]})
        );
        assert!(footer.starts_with(
            "not included (over the reply budget): a.rs#A, b \"q\".md, c.rs lines 3-4 — call"
        ));

        let ranges = [
            ReadItem::Lines("x.rs".into(), "1-2".into()),
            ReadItem::Lines("x.rs".into(), "9-9".into()),
        ];
        let footer = read_follow_up(ReadForm::Ranges, &ranges.iter().collect::<Vec<_>>());
        assert_eq!(
            footer,
            "not included (over the reply budget): 1-2, 9-9 — call ide.read \
             {\"path\":\"x.rs\",\"ranges\":[\"1-2\",\"9-9\"]}\n"
        );
        let symbols = [ReadItem::Symbol("x.rs#A".into())];
        assert_eq!(
            read_follow_up(ReadForm::Symbols, &symbols.iter().collect::<Vec<_>>()),
            "not included (over the reply budget): x.rs#A — call ide.read {\"symbols\":[\"x.rs#A\"]}\n"
        );
    }

    #[test]
    fn follow_up_names_eight_and_counts_the_rest() {
        let items = (0..11)
            .map(|index| ReadItem::Path(format!("f{index}.md")))
            .collect::<Vec<_>>();
        let footer = read_follow_up(ReadForm::Items, &items.iter().collect::<Vec<_>>());
        assert!(footer.ends_with("; +3 more\n"), "{footer}");
        assert!(
            footer.contains("f7.md") && !footer.contains("f8.md"),
            "{footer}"
        );
    }

    /// Unknown/new context cannot use guards, and an existing unit at the quota must not evict
    /// other entries. Crossing the distinct-unit or block quota explicitly resets read history.
    #[test]
    fn read_guard_epochs_and_quota_boundaries_are_explicit() {
        let mut memory = ReadMemory::default();
        for index in 0..MAX_REMEMBERED_UNITS {
            memory.remember_unit(Some(1), &format!("a.rs#unit{index}"), "old\n".into(), 1, 1);
        }
        assert!(memory.latest_unit_text(None, "a.rs#unit0").is_none());
        assert!(memory.latest_unit_text(Some(2), "a.rs#unit0").is_none());
        memory.remember_unit(Some(1), "a.rs#unit0", "new\n".into(), 1, 1);
        assert_eq!(
            memory.latest_unit_text(Some(1), "a.rs#unit0").unwrap().text,
            "new\n"
        );
        assert!(memory.unit_text(Some(1), "a.rs#unit1").is_some());
        memory.remember_unit(Some(1), "a.rs#overflow", "x\n".into(), 1, 1);
        assert!(memory.latest_unit_text(Some(1), "a.rs#unit0").is_none());
        for index in 0..=MAX_REMEMBERED_BLOCKS {
            memory.remember(Some(1), &format!("block{index}"), "body", "ref");
        }
        assert!(memory.latest_unit_text(Some(1), "a.rs#overflow").is_none());
    }

    /// After the session's own write, a unit it had read whole takes the new text, a unit it had
    /// read in part is forgotten, and another epoch's memory is never touched.
    #[test]
    fn own_writes_refresh_whole_units_and_forget_partial_ones() {
        let mut memory = ReadMemory::default();
        memory.remember_unit(Some(1), "a.rs#whole", "x\ny\n".into(), 1, 2);
        memory.remember_unit(Some(1), "a.rs#part", "p\nq\nr\n".into(), 1, 2);
        memory.remember_unit(Some(1), "b.rs#other", "o\n".into(), 1, 1);
        let current = |address: &str| match address {
            "a.rs#whole" => Some(("x\nY\nz\n".to_owned(), 3)),
            _ => None,
        };
        let before = |address: &str| match address {
            "a.rs#whole" => Some(("x\ny\n".to_owned(), 2)),
            "a.rs#part" => Some(("p\nq\nr\n".to_owned(), 3)),
            _ => None,
        };
        memory.refresh_file_units(Some(2), "a.rs", &current, &before);
        assert!(memory.unit_text(Some(1), "a.rs#whole").unwrap().text == "x\ny\n");
        memory.refresh_file_units(Some(1), "a.rs", &current, &before);
        let whole = memory.unit_text(Some(1), "a.rs#whole").unwrap();
        assert_eq!(
            (whole.text.as_str(), whole.first, whole.last),
            ("x\nY\nz\n", 1, 3)
        );
        assert!(memory.unit_text(Some(1), "a.rs#part").is_none());
        assert!(memory.unit_text(Some(1), "b.rs#other").is_some());
    }

    #[test]
    fn read_memory_answers_unchanged_only_for_identical_bytes_in_one_known_epoch() {
        use crate::assistance::outbox::{RecordingOutbox, SessionOutbox};
        let outbox = RecordingOutbox::default();
        let session = crate::assistance::host_binding::BindingRef::fixture("reader", "channel", 1);
        let mut memory = ReadMemory::default();
        let block = "a.md  (lines 1–2)\n1\tone\n2\ttwo\n";
        // No compaction signal: never answered unchanged, nothing remembered.
        assert_eq!(outbox.context_epoch(&session), None);
        memory.remember(outbox.context_epoch(&session), "a.md", block, "ref-1");
        assert_eq!(
            memory.delivered_before(outbox.context_epoch(&session), "a.md", block),
            None
        );
        outbox.set_context_epoch(&session, 1);
        let epoch = outbox.context_epoch(&session);
        assert_eq!(memory.delivered_before(epoch, "a.md", block), None);
        memory.remember(epoch, "a.md", block, "ref-1");
        assert_eq!(
            memory.delivered_before(epoch, "a.md", block),
            Some("ref-1".to_owned())
        );
        assert_eq!(
            memory.delivered_before(epoch, "a.md", "a.md  (lines 1–2)\n1\tone\n2\tTWO\n"),
            None,
            "changed bytes are sent again"
        );
        assert_eq!(memory.delivered_before(epoch, "b.md", block), None);
        outbox.set_context_epoch(&session, 2);
        assert_eq!(
            memory.delivered_before(outbox.context_epoch(&session), "a.md", block),
            None,
            "a compaction forgets every earlier read"
        );
        assert_eq!(
            unchanged_line("ref-1", "a.md", block),
            "unchanged since your read ref-1: a.md (2 lines); \"force\":true returns the text\n"
        );
    }

    /// Validates one `ide.edit {files}` request through the facade, as a call would be.
    fn edit(request: Value) -> Result<(), ParameterError> {
        crate::assistance::facade::validate_call(AssistanceTool::Edit, request).map(|_| ())
    }

    #[test]
    fn edit_files_accept_the_three_entry_forms() {
        assert!(
            edit(json!({"operation_id":"op","source_ref":"r-1","files":[
                {"path":"a.rs","changes":[{"lines":"1-2","content":"x"}]},
                {"path":"b.rs","source_ref":"r-2","changes":[{"old":"a","new":"b"}]},
                {"path":"c.rs","changes":[{"symbol":"c.rs#f","op":"delete"}]},
                {"path":"new.rs","create":true,"content":"fn main() {}\n"},
                {"path":"d.md","content":"whole"}]}))
            .is_ok()
        );
    }

    #[test]
    fn edit_files_refuse_bad_shapes_and_mixes() {
        for bad in [
            json!({"operation_id":"op","files":[]}),
            json!({"operation_id":"op","path":"a.md","files":[{"path":"a.md","create":true,"content":"x"}]}),
            json!({"operation_id":"op","files":[{"path":"a.md"}]}),
            json!({"operation_id":"op","files":[{"path":"a.md","content":"x","changes":[{"lines":"1-1","content":""}]}]}),
            json!({"operation_id":"op","files":[{"path":"a.md","create":true,"changes":[{"symbol":"a.md#x","op":"delete"}]}]}),
            json!({"operation_id":"op","files":[{"path":"../a.md","create":true,"content":"x"}]}),
            json!({"operation_id":"op","files":[{"path":"a.md","create":true,"content":"x"},{"path":"a.md","create":true,"content":"y"}]}),
            json!({"operation_id":"op","files":[{"path":"a.md","create":"yes","content":"x"}]}),
            json!({"operation_id":"op","files":[{"path":"a.md","create":true,"content":"x","extra":1}]}),
        ] {
            assert!(edit(bad.clone()).is_err(), "{bad}");
        }
    }

    /// Entry paths are compared as written: `/repo/a.md` and `repo/a.md` are different files and
    /// both pass the facade, while the same spelling twice is refused. Whether an absolute and a
    /// relative spelling name one worktree file is the worker's check.
    #[test]
    fn edit_files_compare_paths_as_written() {
        assert!(
            edit(json!({"operation_id":"op","files":[
                {"path":"/repo/a.md","create":true,"content":"x"},
                {"path":"repo/a.md","create":true,"content":"y"}]}))
            .is_ok()
        );
        assert!(
            edit(json!({"operation_id":"op","files":[
                {"path":"/repo/a.md","create":true,"content":"x"},
                {"path":"/repo/a.md","create":true,"content":"y"}]}))
            .is_err()
        );
    }

    #[test]
    fn edit_files_cap_files_changes_and_text() {
        let file =
            |index: usize| json!({"path": format!("f{index}.md"), "create": true, "content": "x"});
        assert!(
            edit(json!({"operation_id":"op","files":(0..16).map(file).collect::<Vec<_>>()}))
                .is_ok()
        );
        assert!(
            edit(json!({"operation_id":"op","files":(0..17).map(file).collect::<Vec<_>>()}))
                .is_err()
        );
        let many = |index: usize| {
            json!({"path": format!("f{index}.rs"), "changes": (0..32)
                .map(|line| json!({"symbol": format!("f{index}.rs#s{line}"), "op": "delete"}))
                .collect::<Vec<_>>()})
        };
        assert!(
            edit(json!({"operation_id":"op","files":(0..4).map(many).collect::<Vec<_>>()})).is_ok()
        );
        assert_eq!(
            edit(json!({"operation_id":"op","files":(0..5).map(many).collect::<Vec<_>>()})),
            Err(invalid_field(
                "files",
                FieldRule::OneOf("at most 128 changes across all files")
            ))
        );
        let large = |index: usize| json!({"path": format!("f{index}.md"), "create": true, "content": "y".repeat(40 * 1024)});
        assert_eq!(
            edit(json!({"operation_id":"op","files":(0..4).map(large).collect::<Vec<_>>()})),
            Err(ParameterError::InvalidObject),
            "160 KiB of content is over the 136 KiB argument bound before the text total"
        );
        let medium = |index: usize| json!({"path": format!("f{index}.md"), "create": true, "content": "y".repeat(33 * 1024)});
        assert!(
            edit(json!({"operation_id":"op","files":(0..3).map(medium).collect::<Vec<_>>()}))
                .is_ok()
        );
        // 4 × 33 KiB = 132 KiB of text: inside the 136 KiB argument bound, so only the 128 KiB
        // text total refuses it.
        let four = json!({"operation_id":"op","files":(0..4).map(medium).collect::<Vec<_>>()});
        assert!(
            serde_json::to_string(&four).unwrap().len()
                <= crate::changes::edit::MAX_EDIT_ARGUMENT_BYTES
        );
        assert_eq!(
            edit(four),
            Err(invalid_field(
                "files",
                FieldRule::TooLong(MAX_EDIT_TEXT_BYTES)
            ))
        );
    }

    #[test]
    fn edit_requests_read_back_and_digest_only_the_callers_arguments() {
        let request = json!({"operation_id":"op","source_ref":"r-1","files":[
            {"path":"a.rs","changes":[{"lines":"1-2","content":"x"}]},
            {"path":"b.rs","source_ref":"r-2","content":"whole"},
            {"path":"c.rs","create":true,"content":"new"}]});
        let (operation, files) = edit_files_request(&request).unwrap();
        assert_eq!(operation, "op");
        assert_eq!(
            files,
            vec![
                EditFile {
                    path: "a.rs".into(),
                    source_ref: Some("r-1".into()),
                    body: EditBody::Changes(vec![json!({"lines":"1-2","content":"x"})]),
                },
                EditFile {
                    path: "b.rs".into(),
                    source_ref: Some("r-2".into()),
                    body: EditBody::Replace("whole".into()),
                },
                EditFile {
                    path: "c.rs".into(),
                    source_ref: Some("r-1".into()),
                    body: EditBody::Create("new".into()),
                },
            ]
        );
        let reordered: Value = serde_json::from_str(
            r#"{"files":[{"changes":[{"content":"x","lines":"1-2"}],"path":"a.rs"},{"content":"whole","source_ref":"r-2","path":"b.rs"},{"create":true,"path":"c.rs","content":"new"}],"source_ref":"r-1","operation_id":"op"}"#,
        )
        .unwrap();
        assert_eq!(
            edit_request_digest(&request),
            edit_request_digest(&reordered)
        );
        let mut changed = request.clone();
        changed["files"][2]["content"] = json!("other");
        assert_ne!(edit_request_digest(&request), edit_request_digest(&changed));
    }

    #[test]
    fn edit_replies_state_every_file_and_open_with_the_outcome() {
        let report = |path: &str, state| FileReport {
            path: path.into(),
            state,
            note: None,
            diagnostics: None,
        };
        let applied = render_edit_reply(
            &[
                FileReport {
                    note: Some("  a.rs:2 (L2)  replaced (1 lines → 1)".into()),
                    diagnostics: Some("clean".into()),
                    ..report("a.rs", FileState::Replaced)
                },
                report("b.rs", FileState::WrittenEarlier { created: false }),
                report("c.rs", FileState::Created),
            ],
            "clean",
            &[],
        );
        assert_eq!(
            applied,
            "edit: applied 3 of 3 files; diagnostics: clean\n  \
             1 a.rs: replaced; diagnostics clean\n    a.rs:2 (L2)  replaced (1 lines → 1)\n  \
             2 b.rs: written earlier by this operation_id\n  3 c.rs: created\n"
        );
        let partial = render_edit_reply(
            &[
                report("a.rs", FileState::Replaced),
                report(
                    "b.rs",
                    FileState::NotWritten(NotWritten::ChangedAfterValidation),
                ),
                report("c.rs", FileState::NotWritten(NotWritten::NotReached)),
            ],
            "unknown",
            &["new: a.rs:1:1 error x".into()],
        );
        assert!(
            partial.starts_with(
                "edit: partial — written 1 of 3; not written: 2 b.rs (changed after validation), \
             3 c.rs (not reached); the same operation_id resumes the rest\ndiagnostics: unknown\n"
            ),
            "{partial}"
        );
        assert!(partial.ends_with("new: a.rs:1:1 error x\n"), "{partial}");
        for banned in [
            "poll",
            "every few seconds",
            "in a few seconds",
            "until it completes",
            "again every",
        ] {
            assert!(
                !applied.contains(banned) && !partial.contains(banned),
                "{banned}"
            );
        }
    }

    #[test]
    fn globs_match_segments_and_any_depth() {
        assert!(glob_matches("src/**/*.rs", "src/a.rs"));
        assert!(glob_matches("src/**/*.rs", "src/x/y/z.rs"));
        assert!(!glob_matches("src/**/*.rs", "lib/a.rs"));
        assert!(!glob_matches("src/*.rs", "src/x/a.rs"));
        assert!(glob_matches("src/?.rs", "src/a.rs"));
        assert!(!glob_matches("src/?.rs", "src/ab.rs"));
        assert!(glob_matches("**/*_test.py", "pkg/sub/a_test.py"));
        assert!(glob_matches("*.md", "README.md"));
        assert!(!glob_matches("*.md", "docs/a.md"));
        assert_eq!(glob_root("crates/x/src/**/*.rs"), "crates/x/src");
        assert_eq!(glob_root("**/*.rs"), "");
        assert_eq!(glob_root("src/*.rs"), "src");
        assert_eq!(glob_root("*.md"), "");
    }

    #[test]
    fn outline_batches_take_one_of_paths_or_glob() {
        use crate::assistance::facade::validate_call;
        let outline = |request: Value| validate_call(AssistanceTool::Outline, request);
        assert!(outline(json!({"paths":["src/a.rs","src/b.rs"],"kinds":"fn,struct"})).is_ok());
        assert!(outline(json!({"glob":"src/**/*.rs","kinds":"fn"})).is_ok());
        for bad in [
            json!({"paths":[]}),
            json!({ "paths": vec!["a.rs"; 25] }),
            json!({"paths":["../a.rs"]}),
            json!({"paths":["a.rs"],"glob":"*.rs"}),
            json!({"paths":["a.rs"],"path":"b.rs"}),
            json!({"glob":"../**/*.rs"}),
            json!({"glob":"/etc/*"}),
            json!({"glob":""}),
            json!({"glob":"*.rs","kinds":"function"}),
        ] {
            assert!(outline(bad.clone()).is_err(), "{bad}");
        }
        assert!(
            outline(json!({"path":"src/a.rs"})).is_ok(),
            "the single form is unchanged"
        );
    }

    #[test]
    fn symbol_and_graph_entries_validate_like_their_single_forms() {
        use crate::assistance::facade::validate_call;
        for tool in [AssistanceTool::Symbol, AssistanceTool::Graph] {
            assert!(
                validate_call(tool, json!({"symbols":["src/a.rs#A","Name"]})).is_ok(),
                "{tool:?}"
            );
            for bad in [
                json!({"symbols":[]}),
                json!({ "symbols": vec!["a"; 9] }),
                json!({"symbols":["a", ""]}),
                json!({"symbols":["a"], "symbol":"b"}),
                json!({"symbols":"a"}),
            ] {
                assert!(validate_call(tool, bad.clone()).is_err(), "{tool:?} {bad}");
            }
        }
        assert!(
            validate_call(
                AssistanceTool::Symbol,
                json!({"symbols":["a","b"],"callers":2,"usages":false})
            )
            .is_ok()
        );
        assert!(
            validate_call(AssistanceTool::Symbol, json!({"symbols":["a"],"callers":4})).is_err()
        );
        assert!(
            validate_call(
                AssistanceTool::Graph,
                json!({"symbols":["a","b"],"direction":"both","depth":3})
            )
            .is_ok()
        );
        assert!(validate_call(AssistanceTool::Graph, json!({"symbols":["a"],"depth":0})).is_err());
        assert!(
            validate_call(AssistanceTool::Symbol, json!({"usages":true})).is_err(),
            "the single form still needs its symbol"
        );
    }

    /// Accepted batch checks preserve bounded per-file diagnostics and never label unavailable
    /// coverage clean; terminal failed/cancelled/superseded causes remain in their Bridge fact.
    #[test]
    fn edit_checked_facts_cover_every_file_within_the_outbox_budget() {
        use crate::assistance::outbox::{
            FactKind, FencedPath, JobEpoch, RecordingOutbox, SessionOutbox,
        };
        use crate::assistance::reply::EditDiagnostics;
        let reported = |count: usize| EditDiagnostics::CurrentReported {
            messages: (0..count)
                .map(|line| format!("new: src/b.rs:{line}:1 error [E0001] {}", "é".repeat(80)))
                .collect(),
            delta: String::new(),
            truncated: false,
        };
        let files = vec![
            (
                "src/a.rs".to_owned(),
                EditDiagnostics::CurrentClean { project_errors: 0 },
            ),
            ("src/b.rs".to_owned(), reported(8)),
            (
                "docs/x.md".to_owned(),
                EditDiagnostics::NotAnalysed { reason: "r".into() },
            ),
        ];
        let fences = vec![FencedPath {
            path: "src/a.rs".into(),
            digest: Some([1; 32]),
        }];
        let draft = edit_checked_draft("op-7", "ref-9", &files, fences.clone());
        assert_eq!(draft.kind, FactKind::EditChecked);
        assert_eq!(draft.subject, "edit op-7");
        assert_eq!(draft.summary, "3 files, 8 new errors, 0 new warnings");
        assert!(draft.failed && draft.source_bearing);
        assert_eq!(draft.detail_ref.as_deref(), Some("ref-9"));
        assert_eq!(draft.fence_paths, fences);
        let detail = draft.detail.clone().unwrap();
        assert!(
            detail.starts_with("  src/a.rs: clean\n  src/b.rs: 8 new errors"),
            "{detail}"
        );
        assert_eq!(draft.validate(), Ok(()), "{detail}");
        let outbox = RecordingOutbox::default();
        let session = crate::assistance::host_binding::BindingRef::fixture("editor", "channel", 1);
        assert!(outbox.push(&[session], draft, JobEpoch::default()));

        let clean = edit_checked_draft(
            "op-8",
            "ref-10",
            &[(
                "src/a.rs".to_owned(),
                EditDiagnostics::CurrentClean { project_errors: 0 },
            )],
            Vec::new(),
        );
        assert_eq!(clean.summary, "1 files, clean");
        assert!(!clean.failed && !clean.source_bearing);
        assert_eq!(clean.validate(), Ok(()));
        for reason in [
            "checks unavailable: actual checker cause",
            "project check cancelled",
            "project check superseded",
            "matching diagnostics deadline",
        ] {
            let unavailable = edit_checked_draft(
                "op-9",
                "ref-11",
                &[(
                    "src/a.rs".into(),
                    EditDiagnostics::NotAnalysed {
                        reason: reason.into(),
                    },
                )],
                Vec::new(),
            );
            assert!(!unavailable.summary.contains("clean"));
            assert!(unavailable.detail.as_deref().unwrap().contains(reason));
            assert!(unavailable.failed);
            assert_eq!(unavailable.validate(), Ok(()));
        }
    }

    #[test]
    fn diagnostics_summaries_count_new_problems_per_file() {
        use crate::assistance::reply::EditDiagnostics;
        let (summary, words, lines) = summarize_diagnostics(&[
            (
                "a.rs".into(),
                EditDiagnostics::CurrentClean { project_errors: 0 },
            ),
            (
                "b.rs".into(),
                EditDiagnostics::CurrentReported {
                    messages: vec![
                        "new: b.rs:1:1 error E1".into(),
                        "pre-existing: b.rs:2:1 warning W".into(),
                    ],
                    delta: String::new(),
                    truncated: false,
                },
            ),
            (
                "c.md".into(),
                EditDiagnostics::NotAnalysed { reason: "r".into() },
            ),
        ]);
        assert_eq!(summary, "1 new problems in 1 files");
        assert_eq!(words, ["clean", "1 new problems", "not analysed"]);
        assert_eq!(lines, ["new: b.rs:1:1 error E1"]);
        assert_eq!(summarize_diagnostics(&[]).0, "none (no checked language)");
        assert_eq!(
            summarize_diagnostics(&[("a.rs".into(), EditDiagnostics::Unknown {})]).0,
            "unknown"
        );
    }
    #[test]
    fn test_targets_validate_each_entry_as_its_single_form() {
        let valid = json!({"targets":[{"symbol":"src/a.rs#parse"},{"path":"tests/cli.rs"},{"pattern":"roles_"}],"budget_s":300});
        assert!(super::super::facade::validate_call(AssistanceTool::Test, valid.clone()).is_ok());
        assert_eq!(
            test_targets(&valid),
            vec![
                TestTargetSpec::Symbol("src/a.rs#parse".to_owned()),
                TestTargetSpec::Path("tests/cli.rs".to_owned()),
                TestTargetSpec::Pattern("roles_".to_owned()),
            ]
        );
        assert_eq!(
            test_targets(&json!({"targets":[{"pattern":"a"},{"pattern":"a"}]})),
            vec![TestTargetSpec::Pattern("a".to_owned())],
            "an exact repeat runs once"
        );
        for invalid in [
            json!({"targets":[]}),
            json!({"targets":[{"pattern":"a"}],"symbol":"src/a.rs#f"}),
            json!({"targets":[{"pattern":"a"}],"command":["true"]}),
            json!({"targets":[{"pattern":"a","path":"tests/a.rs"}]}),
            json!({"targets":[{"command":[]}]}),
            json!({"targets":[{"path":"../outside.rs"}]}),
            json!({"targets":[{"pattern":"a"}],"budget_s":0}),
            json!({"targets":(0..9).map(|index| json!({"pattern":format!("p{index}")})).collect::<Vec<_>>()}),
        ] {
            assert!(
                super::super::facade::validate_call(AssistanceTool::Test, invalid.clone()).is_err(),
                "{invalid}"
            );
        }
        assert_eq!(
            TestTargetSpec::Path("tests/cli.rs".to_owned()).label(),
            "path tests/cli.rs"
        );
    }
    /// Every batch read and edit reply line, the pending diagnostics line and the edit-checked
    /// fact state what happened and never invite polling (M024-M027-REPLY-HYGIENE r2 §2).
    #[test]
    fn batch_reply_lines_never_invite_polling() {
        use crate::assistance::reply::EditDiagnostics;
        assert_no_polling(&read_summary(&ReadCounts {
            items: 3,
            files: 2,
            delivered: 1,
            unchanged: 1,
            cut: 1,
            failed: 0,
        }));
        assert_no_polling(&unchanged_line("ref-1", "src/a.rs", "1  a\n2  b\n"));
        let cut = ReadItem::Symbol("src/c.rs#C".to_owned());
        assert_no_polling(&read_follow_up(ReadForm::Items, &[&cut]));
        let pending = vec![(
            "src/a.rs".to_owned(),
            EditDiagnostics::Pending {
                detail_ref: "ref-2".to_owned(),
            },
        )];
        let (words, per_file, _) = summarize_diagnostics(&pending);
        assert!(words.starts_with("pending"), "{words}");
        assert!(
            words.contains("retained under ref-2")
                && words.contains("Bridge delivers it automatically"),
            "{words}"
        );
        assert_no_polling(&words);
        assert_no_unfinished_seconds(&words);
        for unfinished in ["tests #4: still running (12 s)", "pending for 3s"] {
            assert!(
                std::panic::catch_unwind(|| assert_no_unfinished_seconds(unfinished)).is_err(),
                "the guard refuses {unfinished:?}"
            );
        }
        assert_no_unfinished_seconds("tests #4: still running");
        per_file.iter().for_each(|line| assert_no_polling(line));
        let draft = edit_checked_draft("op-1", "ref-2", &pending, Vec::new());
        assert_no_polling(&draft.summary);
        assert_no_polling(draft.detail.as_deref().unwrap_or_default());
    }
    /// Every canonical example of batch-api-v1 (docs/contracts/batch-api-v1.md, one per form)
    /// passes the facade's validation for its tool, its `…` placeholders read as plain text; a
    /// shape the facade refused would make the contract's own example invalid.
    #[test]
    fn the_contracts_canonical_examples_validate() {
        let contract = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../docs/contracts/batch-api-v1.md"),
        )
        .unwrap();
        let mut examples = Vec::new();
        let mut block: Option<String> = None;
        for line in contract.lines() {
            match (&mut block, line) {
                (None, "```json") => block = Some(String::new()),
                (Some(text), "```") => {
                    examples.push(text.replace('…', "x"));
                    block = None;
                }
                (Some(text), line) => {
                    text.push_str(line);
                    text.push('\n');
                }
                (None, _) => {}
            }
        }
        // A block may hold the alternatives of one form (`{paths}` and `{glob}`).
        let examples = examples
            .iter()
            .flat_map(|block| {
                serde_json::Deserializer::from_str(block)
                    .into_iter::<Value>()
                    .map(Result::unwrap)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(examples.len(), 7, "one example per form");
        for value in examples {
            let tool = if value.get("items").is_some() {
                AssistanceTool::Read
            } else if value.get("paths").is_some() || value.get("glob").is_some() {
                AssistanceTool::Outline
            } else if value.get("direction").is_some() {
                AssistanceTool::Graph
            } else if value.get("symbols").is_some() {
                AssistanceTool::Symbol
            } else if value.get("targets").is_some() {
                AssistanceTool::Test
            } else {
                AssistanceTool::Edit
            };
            assert!(
                super::super::facade::validate_call(tool, value.clone()).is_ok(),
                "{tool:?}: {value}"
            );
        }
    }
    /// Every multi-file edit outcome carries the structured result of batch-api-v1 §7 beside its
    /// text: `operation_id`, `outcome`, `source_ref` only when a file holds its candidate, and
    /// per file `path`, `state` and, for a file not written, `reason`.
    #[test]
    fn edit_batch_replies_carry_the_structured_result_for_every_outcome() {
        let report = |path: &str, state: FileState| FileReport {
            path: path.into(),
            state,
            note: None,
            diagnostics: None,
        };
        let structured = |reply: PeerReply| serde_json::to_value(reply).unwrap();
        let applied = structured(edit_batch_reply(
            "op-1",
            &[
                report("a.rs", FileState::Replaced),
                report("b.rs", FileState::WrittenEarlier { created: true }),
                report("c.rs", FileState::Unchanged),
            ],
            Some("ref-1"),
            "clean",
            &[],
        ));
        assert_eq!(applied["state"], "edit_batch");
        assert_eq!(applied["operation_id"], "op-1");
        assert_eq!(applied["outcome"], "applied");
        assert_eq!(applied["source_ref"], "ref-1");
        assert_eq!(
            applied["files"],
            json!([
                {"path":"a.rs","state":"replaced"},
                {"path":"b.rs","state":"created"},
                {"path":"c.rs","state":"unchanged"}
            ])
        );
        assert!(
            applied["text"]
                .as_str()
                .unwrap()
                .starts_with("edit: applied 3 of 3 files")
        );
        let partial = structured(edit_batch_reply(
            "op-2",
            &[
                report("a.rs", FileState::Created),
                report(
                    "b.rs",
                    FileState::NotWritten(NotWritten::ChangedAfterValidation),
                ),
                report("c.rs", FileState::NotWritten(NotWritten::NotReached)),
                report("d.rs", FileState::NotWritten(NotWritten::IoError)),
            ],
            Some("ref-2"),
            "unknown",
            &[],
        ));
        assert_eq!(partial["outcome"], "partial");
        assert_eq!(partial["source_ref"], "ref-2");
        assert_eq!(
            partial["files"],
            json!([
                {"path":"a.rs","state":"created"},
                {"path":"b.rs","state":"not_written","reason":"changed_after_validation"},
                {"path":"c.rs","state":"not_written","reason":"not_reached"},
                {"path":"d.rs","state":"not_written","reason":"io_error"}
            ])
        );
        let nothing = structured(edit_batch_reply(
            "op-3",
            &[report(
                "a.rs",
                FileState::NotWritten(NotWritten::ChangedAfterValidation),
            )],
            Some("ref-3"),
            "none",
            &[],
        ));
        assert_eq!(nothing["outcome"], "partial");
        assert!(
            nothing["source_ref"].is_null(),
            "no written file, no source_ref"
        );
        // A 128-byte operation_id (the longest valid one) must not stretch the refusal head.
        let long_id = "i".repeat(128);
        let duplicate = structured(conflicting_duplicate_reply(&long_id));
        assert_eq!(duplicate["outcome"], "conflicting_duplicate");
        assert_eq!(duplicate["operation_id"], json!(long_id));
        assert_eq!(duplicate["files"], json!([]));
        assert!(duplicate["source_ref"].is_null());
        let text = duplicate["text"].as_str().unwrap();
        let lines = text.lines().collect::<Vec<_>>();
        assert!(
            text.starts_with("This operation_id already names"),
            "{text}"
        );
        assert!(!text.contains(&long_id), "{text}");
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].len() <= 160 && lines[1].len() <= 120, "{text}");
        assert!(lines[1].starts_with("Repair: "), "{text}");
        let lost = structured(edit_batch_reply(
            "op-5",
            &[
                report("a.rs", FileState::Replaced),
                report("b.rs", FileState::Unknown),
                report("c.rs", FileState::NotWritten(NotWritten::NotReached)),
            ],
            Some("ref-5"),
            "unknown",
            &[],
        ));
        assert_eq!(lost["outcome"], "outcome_unknown");
        assert_eq!(lost["files"][1], json!({"path":"b.rs","state":"unknown"}));
        assert!(
            lost["text"]
                .as_str()
                .unwrap()
                .starts_with("edit: partial — written 1 of 3")
        );
        let panicked = structured(unknown_batch_reply(
            "op-6",
            &["a.rs".to_owned(), "b.rs".to_owned()],
        ));
        assert_eq!(panicked["outcome"], "outcome_unknown");
        assert!(panicked["source_ref"].is_null());
        assert_eq!(
            panicked["files"],
            json!([{"path":"a.rs","state":"unknown"},{"path":"b.rs","state":"unknown"}])
        );
        for reply in [&applied, &partial, &duplicate, &lost, &panicked] {
            let text = reply["text"].as_str().unwrap();
            assert_no_polling(text);
            let decoded = PeerReply::decode(&serde_json::to_string(reply).unwrap())
                .unwrap_or_else(|| panic!("the closed reply decodes: {reply}"));
            let rendered = super::super::content::render_with_call(
                decoded,
                None,
                super::super::content::Envelope::WithStructured,
                Some("ide.edit"),
                None,
            )
            .expect("the edit_batch state renders");
            assert_eq!(
                rendered.content[0]
                    .as_text()
                    .map(|block| block.text.as_str()),
                Some(text),
                "the compact text is the reply text"
            );
            assert_eq!(rendered.structured_content.as_ref(), Some(reply));
        }
    }
}
