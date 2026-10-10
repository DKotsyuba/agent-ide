//! Batch request forms shared by the Assistance facade and worker (batch-api-v1).
//!
//! The facade validates a batch request against the caps here before anything runs; the worker
//! turns the validated request back into typed items and renders the summary line and the
//! follow-up footer of its reply with the pure helpers here. Nothing in this module observes a
//! file, touches the ledger or decides how much of a reply fits: those stay with the worker.

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use super::facade::{
    AssistanceTool, FieldRule, ParameterError, invalid_field, parse_line_range, path_shape_rule,
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
/// over-limit refusal names. Only `ide.read`'s `items`, `symbols` and `ranges` are batch keys in
/// this revision of the tool surface.
pub(crate) fn batch_key(tool: AssistanceTool, parameters: &Value) -> Option<&'static str> {
    let object = parameters.as_object()?;
    let keys: &[&'static str] = match tool {
        AssistanceTool::Read => &["items", "symbols", "ranges"],
        _ => &[],
    };
    keys.iter().copied().find(|key| object.contains_key(*key))
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
                if lines.is_some_and(|lines| parse_line_range(lines).is_none()) {
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
            validate_call(AssistanceTool::Read, json!({"path":"a.md","force":true})).is_err(),
            "force needs items"
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
}
