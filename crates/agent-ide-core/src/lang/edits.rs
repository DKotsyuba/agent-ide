//! Applies LSP text edits and workspace-edit groupings to exact source text.
//!
//! Everything here is a pure function of its inputs: no I/O, no worktree, no language-specific
//! knowledge. [`byte_offset`] and [`apply_text_edits`] convert the `Position`/`Range` coordinates
//! a language server reports into byte offsets in the exact source string the offer was computed
//! against, and splice the server's edits into it. [`group_workspace_edit`] turns a whole
//! `WorkspaceEdit` (as project-wide `rename` receives it) into one edit list per file, in the
//! stable order rename applies them.

use async_lsp::lsp_types as lsp;

/// Converts an LSP position to a byte offset in `source` under the negotiated encoding (UTF-8,
/// UTF-16 or UTF-32 character units).
///
/// Lines are split on `'\n'`; a `'\r'` immediately before it is the other half of a CRLF
/// terminator, not a character on the line, so it never counts toward `position.character` and is
/// never returned as part of a line's end. `position.character` past a line's content clamps to
/// that line's end (the byte immediately before its terminator, or `source.len()` on the last
/// line); `position.line` past the last line clamps to `source.len()`. A `source` ending in `'\n'`
/// has one trailing empty line, matching how LSP servers count lines.
pub fn byte_offset(
    source: &str,
    position: lsp::Position,
    encoding: &lsp::PositionEncodingKind,
) -> usize {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut line_start = 0usize;
    let mut line = 0u32;
    loop {
        let mut content_end = line_start;
        while content_end < len && bytes[content_end] != b'\n' {
            content_end += 1;
        }
        let has_newline = content_end < len;
        let visible_end =
            if has_newline && content_end > line_start && bytes[content_end - 1] == b'\r' {
                content_end - 1
            } else {
                content_end
            };
        if line == position.line {
            let offset = character_offset(
                &source[line_start..visible_end],
                position.character,
                encoding,
            );
            return line_start + offset;
        }
        if !has_newline {
            return len;
        }
        line += 1;
        line_start = content_end + 1;
    }
}

/// Byte offset of `target_units` encoded character units into `line` (a slice holding no line
/// terminator); clamps to `line.len()` once `target_units` reaches or passes the line's own count.
fn character_offset(line: &str, target_units: u32, encoding: &lsp::PositionEncodingKind) -> usize {
    let target_units = target_units as usize;
    if *encoding == lsp::PositionEncodingKind::UTF8 {
        return target_units.min(line.len());
    }
    let mut units = 0usize;
    for (byte_index, ch) in line.char_indices() {
        if units >= target_units {
            return byte_index;
        }
        units += if *encoding == lsp::PositionEncodingKind::UTF16 {
            ch.len_utf16()
        } else {
            // UTF-32 (and any other encoding, treated the same way): one code point, one unit.
            1
        };
    }
    line.len()
}

/// Number of LSP-addressable lines in `source`: one more than its `'\n'` count, so a trailing
/// newline yields one trailing empty line.
fn line_count(source: &str) -> u32 {
    source.bytes().filter(|byte| *byte == b'\n').count() as u32 + 1
}

/// Failure applying a batch of `TextEdit`s to a source string.
#[derive(Debug, PartialEq, Eq)]
pub enum EditApplyError {
    /// Two edits' ranges cover a shared byte of the source.
    Overlap,
    /// An edit's range names a line the source does not have, or its `end` is before its `start`.
    OutOfRange,
}

/// Applies `edits` (as the server sent them: ranges refer to the ORIGINAL `source`) and returns
/// the new text.
///
/// Every range is resolved against `source` once, up front, under `encoding`, so the edits never
/// need adjusting for one another's effect; overlapping ranges are rejected instead of guessing an
/// order. Two edits with identical (including empty) ranges keep the order they have in `edits`.
/// A range naming a line `source` does not have, or with `end` before `start`, fails with
/// [`EditApplyError::OutOfRange`]; a byte shared by two edits' ranges fails with
/// [`EditApplyError::Overlap`].
pub fn apply_text_edits(
    source: &str,
    edits: &[lsp::TextEdit],
    encoding: &lsp::PositionEncodingKind,
) -> Result<String, EditApplyError> {
    let lines = line_count(source);
    let mut spans = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        if edit.range.start.line >= lines || edit.range.end.line >= lines {
            return Err(EditApplyError::OutOfRange);
        }
        let start = byte_offset(source, edit.range.start, encoding);
        let end = byte_offset(source, edit.range.end, encoding);
        if end < start {
            return Err(EditApplyError::OutOfRange);
        }
        spans.push((start, end, index));
    }
    // Ascending by start, ties broken by input order: concatenating the source in this order
    // below places tied (typically empty-range) edits' text in the same order the caller gave.
    spans.sort_by_key(|&(start, _, index)| (start, index));

    let mut covered_to = 0usize;
    for &(start, end, _) in &spans {
        if start < covered_to {
            return Err(EditApplyError::Overlap);
        }
        covered_to = covered_to.max(end);
    }

    let mut result = String::with_capacity(source.len());
    let mut cursor = 0usize;
    for (start, end, index) in spans {
        result.push_str(&source[cursor..start]);
        result.push_str(&edits[index].new_text);
        cursor = end;
    }
    result.push_str(&source[cursor..]);
    Ok(result)
}

/// One file's edits, taken out of a grouped `WorkspaceEdit`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileEdits {
    pub uri: lsp::Url,
    /// The edits for this file, in the order the server sent them.
    pub edits: Vec<lsp::TextEdit>,
}

/// A `WorkspaceEdit` split into one edit list per file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupedEdits {
    /// One entry per touched file, sorted by URI.
    pub files: Vec<FileEdits>,
    /// Create/rename/delete resource operations the edit asked for, described for the caller to
    /// refuse explicitly; project-wide rename never performs them.
    pub unsupported: Vec<String>,
}

/// Groups a `WorkspaceEdit` into per-file edit lists, sorted by URI.
///
/// Reads both `changes` and `document_changes`; when the same file's URI appears in more than
/// one, its edits are concatenated, `changes` first, each side keeping its own order. A
/// `document_changes` entry that is a `TextDocumentEdit` (`OneOf::Left` plain edits and
/// `OneOf::Right` annotated edits both count) contributes its edits the same way; a create,
/// rename or delete resource operation is not an edit and is described in [`GroupedEdits::unsupported`]
/// instead, one entry per operation, in the order it appeared.
pub fn group_workspace_edit(edit: lsp::WorkspaceEdit) -> GroupedEdits {
    let mut files: Vec<FileEdits> = Vec::new();
    let mut unsupported = Vec::new();

    let mut merge = |uri: lsp::Url, new_edits: Vec<lsp::TextEdit>| {
        if let Some(existing) = files.iter_mut().find(|file| file.uri == uri) {
            existing.edits.extend(new_edits);
        } else {
            files.push(FileEdits {
                uri,
                edits: new_edits,
            });
        }
    };

    if let Some(changes) = edit.changes {
        for (uri, edits) in changes {
            merge(uri, edits);
        }
    }

    if let Some(document_changes) = edit.document_changes {
        match document_changes {
            lsp::DocumentChanges::Edits(document_edits) => {
                for document_edit in document_edits {
                    merge(
                        document_edit.text_document.uri,
                        text_edits_of(document_edit.edits),
                    );
                }
            }
            lsp::DocumentChanges::Operations(operations) => {
                for operation in operations {
                    match operation {
                        lsp::DocumentChangeOperation::Edit(document_edit) => {
                            merge(
                                document_edit.text_document.uri,
                                text_edits_of(document_edit.edits),
                            );
                        }
                        lsp::DocumentChangeOperation::Op(resource_op) => {
                            unsupported.push(describe_resource_op(&resource_op));
                        }
                    }
                }
            }
        }
    }

    files.sort_by(|a, b| a.uri.as_str().cmp(b.uri.as_str()));
    GroupedEdits { files, unsupported }
}

/// Unwraps a `TextDocumentEdit`'s edits, dropping the change-annotation id from annotated ones.
fn text_edits_of(
    edits: Vec<lsp::OneOf<lsp::TextEdit, lsp::AnnotatedTextEdit>>,
) -> Vec<lsp::TextEdit> {
    edits
        .into_iter()
        .map(|edit| match edit {
            lsp::OneOf::Left(text_edit) => text_edit,
            lsp::OneOf::Right(annotated) => annotated.text_edit,
        })
        .collect()
}

/// Human-readable label for a create/rename/delete resource operation, for
/// [`GroupedEdits::unsupported`].
fn describe_resource_op(op: &lsp::ResourceOp) -> String {
    match op {
        lsp::ResourceOp::Create(create) => format!("create {}", create.uri),
        lsp::ResourceOp::Rename(rename) => {
            format!("rename {} to {}", rename.old_uri, rename.new_uri)
        }
        lsp::ResourceOp::Delete(delete) => format!("delete {}", delete.uri),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn pos(line: u32, character: u32) -> lsp::Position {
        lsp::Position::new(line, character)
    }

    fn range(start: (u32, u32), end: (u32, u32)) -> lsp::Range {
        lsp::Range::new(pos(start.0, start.1), pos(end.0, end.1))
    }

    fn edit(start: (u32, u32), end: (u32, u32), new_text: &str) -> lsp::TextEdit {
        lsp::TextEdit::new(range(start, end), new_text.to_string())
    }

    #[test]
    fn byte_offset_converts_units_for_each_encoding() {
        // "é" is 2 UTF-8 bytes / 1 UTF-16 unit / 1 code point; "🦀" is 4 bytes / 2 units (a
        // surrogate pair) / 1 code point.
        let source = "é🦀x";
        assert_eq!(source.len(), 7);

        assert_eq!(
            byte_offset(source, pos(0, 0), &lsp::PositionEncodingKind::UTF8),
            0
        );
        assert_eq!(
            byte_offset(source, pos(0, 2), &lsp::PositionEncodingKind::UTF8),
            2
        );
        assert_eq!(
            byte_offset(source, pos(0, 6), &lsp::PositionEncodingKind::UTF8),
            6
        );
        assert_eq!(
            byte_offset(source, pos(0, 7), &lsp::PositionEncodingKind::UTF8),
            7
        );

        assert_eq!(
            byte_offset(source, pos(0, 1), &lsp::PositionEncodingKind::UTF16),
            2
        );
        assert_eq!(
            byte_offset(source, pos(0, 3), &lsp::PositionEncodingKind::UTF16),
            6
        );
        assert_eq!(
            byte_offset(source, pos(0, 4), &lsp::PositionEncodingKind::UTF16),
            7
        );

        assert_eq!(
            byte_offset(source, pos(0, 1), &lsp::PositionEncodingKind::UTF32),
            2
        );
        assert_eq!(
            byte_offset(source, pos(0, 2), &lsp::PositionEncodingKind::UTF32),
            6
        );
        assert_eq!(
            byte_offset(source, pos(0, 3), &lsp::PositionEncodingKind::UTF32),
            7
        );
    }

    #[test]
    fn byte_offset_treats_crlf_as_one_terminator_and_clamps() {
        let source = "ab\r\ncd";
        let encoding = lsp::PositionEncodingKind::UTF16;

        // End of the first line lands right after "ab", before the CRLF.
        assert_eq!(byte_offset(source, pos(0, 2), &encoding), 2);
        // A character past the line's content clamps to the same spot.
        assert_eq!(byte_offset(source, pos(0, 99), &encoding), 2);

        assert_eq!(byte_offset(source, pos(1, 0), &encoding), 4);
        assert_eq!(byte_offset(source, pos(1, 2), &encoding), 6);

        // A line past the last one clamps to the end of the source.
        assert_eq!(byte_offset(source, pos(5, 0), &encoding), source.len());
    }

    #[test]
    fn apply_text_edits_replaces_a_single_range() {
        let result = apply_text_edits(
            "hello world",
            &[edit((0, 6), (0, 11), "Earth")],
            &lsp::PositionEncodingKind::UTF16,
        );
        assert_eq!(result, Ok("hello Earth".to_string()));
    }

    #[test]
    fn apply_text_edits_applies_two_non_adjacent_edits() {
        let result = apply_text_edits(
            "aaa bbb ccc",
            &[edit((0, 0), (0, 3), "XXX"), edit((0, 8), (0, 11), "ZZZ")],
            &lsp::PositionEncodingKind::UTF16,
        );
        assert_eq!(result, Ok("XXX bbb ZZZ".to_string()));
    }

    #[test]
    fn apply_text_edits_applies_adjacent_edits_cleanly() {
        let result = apply_text_edits(
            "abcdef",
            &[edit((0, 0), (0, 3), "123"), edit((0, 3), (0, 6), "456")],
            &lsp::PositionEncodingKind::UTF16,
        );
        assert_eq!(result, Ok("123456".to_string()));
    }

    #[test]
    fn apply_text_edits_rejects_overlapping_ranges() {
        let result = apply_text_edits(
            "abcdef",
            &[edit((0, 0), (0, 4), "A"), edit((0, 2), (0, 6), "B")],
            &lsp::PositionEncodingKind::UTF16,
        );
        assert_eq!(result, Err(EditApplyError::Overlap));
    }

    #[test]
    fn apply_text_edits_rejects_a_range_beyond_eof() {
        let result = apply_text_edits(
            "abc",
            &[edit((5, 0), (5, 0), "x")],
            &lsp::PositionEncodingKind::UTF16,
        );
        assert_eq!(result, Err(EditApplyError::OutOfRange));
    }

    #[test]
    fn apply_text_edits_replaces_a_multi_line_range() {
        let result = apply_text_edits(
            "line1\nline2\nline3",
            &[edit((0, 5), (1, 0), " ")],
            &lsp::PositionEncodingKind::UTF16,
        );
        assert_eq!(result, Ok("line1 line2\nline3".to_string()));
    }

    #[test]
    fn apply_text_edits_inserts_at_eof() {
        let result = apply_text_edits(
            "abc",
            &[edit((0, 3), (0, 3), " end")],
            &lsp::PositionEncodingKind::UTF16,
        );
        assert_eq!(result, Ok("abc end".to_string()));
    }

    #[test]
    fn apply_text_edits_renames_three_occurrences_on_one_line() {
        // Deliberately out of order to also exercise the internal sort.
        let source = "foo(foo, foo)";
        let edits = [
            edit((0, 9), (0, 12), "bar"),
            edit((0, 0), (0, 3), "bar"),
            edit((0, 4), (0, 7), "bar"),
        ];
        let result = apply_text_edits(source, &edits, &lsp::PositionEncodingKind::UTF16);
        assert_eq!(result, Ok("bar(bar, bar)".to_string()));
    }

    #[test]
    fn group_workspace_edit_reads_the_changes_map() {
        let uri = lsp::Url::parse("file:///a.rs").unwrap();
        let workspace_edit = lsp::WorkspaceEdit {
            changes: Some(HashMap::from([(
                uri.clone(),
                vec![edit((0, 0), (0, 1), "x")],
            )])),
            ..Default::default()
        };
        let grouped = group_workspace_edit(workspace_edit);
        assert_eq!(
            grouped.files,
            vec![FileEdits {
                uri,
                edits: vec![edit((0, 0), (0, 1), "x")],
            }]
        );
        assert!(grouped.unsupported.is_empty());
    }

    #[test]
    fn group_workspace_edit_reads_document_changes_edits_with_annotations() {
        let uri = lsp::Url::parse("file:///b.rs").unwrap();
        let text_document_edit = lsp::TextDocumentEdit {
            text_document: lsp::OptionalVersionedTextDocumentIdentifier {
                uri: uri.clone(),
                version: None,
            },
            edits: vec![
                lsp::OneOf::Left(edit((0, 0), (0, 1), "left")),
                lsp::OneOf::Right(lsp::AnnotatedTextEdit {
                    text_edit: edit((1, 0), (1, 1), "right"),
                    annotation_id: "note".to_string(),
                }),
            ],
        };
        let workspace_edit = lsp::WorkspaceEdit {
            document_changes: Some(lsp::DocumentChanges::Edits(vec![text_document_edit])),
            ..Default::default()
        };
        let grouped = group_workspace_edit(workspace_edit);
        assert_eq!(
            grouped.files,
            vec![FileEdits {
                uri,
                edits: vec![edit((0, 0), (0, 1), "left"), edit((1, 0), (1, 1), "right")],
            }]
        );
        assert!(grouped.unsupported.is_empty());
    }

    #[test]
    fn group_workspace_edit_lists_a_create_operation_as_unsupported() {
        let create_uri = lsp::Url::parse("file:///new.rs").unwrap();
        let workspace_edit = lsp::WorkspaceEdit {
            document_changes: Some(lsp::DocumentChanges::Operations(vec![
                lsp::DocumentChangeOperation::Op(lsp::ResourceOp::Create(lsp::CreateFile {
                    uri: create_uri,
                    options: None,
                    annotation_id: None,
                })),
            ])),
            ..Default::default()
        };
        let grouped = group_workspace_edit(workspace_edit);
        assert!(grouped.files.is_empty());
        assert_eq!(
            grouped.unsupported,
            vec!["create file:///new.rs".to_string()]
        );
    }
}
