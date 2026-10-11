//! Pure parser and hunk applier for the Codex `apply_patch` text format.
//!
//! [`parse_patch`] turns an envelope (`*** Begin Patch` … `*** End Patch`) into file operations,
//! and [`apply_hunks`] locates each update hunk in a source text and splices the replacements in.
//! Nothing here reads or writes files: the caller supplies the source text, stores the result and
//! decides what to do with a refusal or a miss. Delete and move operations are refused at parse
//! time with a fixed message because the native tools own those effects.

use std::collections::HashSet;

/// Exact error text returned for `*** Delete File:` and `*** Move to:` lines.
const UNSUPPORTED_OPERATION: &str = "patch: delete and move are not supported; use a native tool";

/// Number of matching passes tried in order by [`apply_hunks`]: exact, trailing whitespace,
/// surrounding whitespace, then Unicode punctuation normalisation.
const PASSES: u8 = 4;

/// Largest line span reported by [`Miss`] when no source window resembles the hunk at all.
const MISS_WINDOW: usize = 8;

/// One file operation parsed from a patch envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PatchFile {
    /// Creates a new file. Produced for each `*** Add File:` section.
    Add {
        /// Target path exactly as written after `*** Add File:`, trimmed. Never empty, and
        /// unique within one patch.
        path: String,
        /// Full text of the new file: the section's `+` lines with the prefix removed, each
        /// followed by `\n`. Empty when the section has no lines.
        content: String,
    },
    /// Edits an existing file. Produced for each `*** Update File:` section.
    Update {
        /// Target path exactly as written after `*** Update File:`, trimmed. Never empty, and
        /// unique within one patch.
        path: String,
        /// Hunks in patch order, each with at least one line. Always non-empty.
        hunks: Vec<Hunk>,
    },
}

/// One `@@` section of an update: the text to find and the text that replaces it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Hunk {
    /// Text after `@@ `, trimmed. `None` for a bare `@@` or a blank anchor, and for a hunk whose
    /// first diff line appears without any `@@` header. When `Some`, the search for `old_lines`
    /// starts after the first source line at or after the cursor whose trimmed text equals this
    /// trimmed anchor.
    pub anchor: Option<String>,
    /// Context and removed lines in order, with their one-character prefix removed. Blank
    /// patch lines count as empty context. An empty vector means the hunk inserts at the end of
    /// the file.
    pub old_lines: Vec<String>,
    /// Context and added lines in order, with their one-character prefix removed. Replaces the
    /// matched `old_lines` verbatim. May be empty for a pure deletion.
    pub new_lines: Vec<String>,
    /// `true` when the hunk was followed by `*** End of File`. Such a hunk matches only at the
    /// end of the source, never elsewhere.
    pub at_end_of_file: bool,
}

/// Result of applying a complete set of hunks to a source text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Applied {
    /// The source text after every hunk has been replaced. Line endings are `\n`. The trailing
    /// newline is kept from the source, or added when an end-of-file hunk or an insertion at the
    /// end adds lines.
    pub text: String,
    /// One 1-based inclusive `(first, last)` line range per hunk that changed something, in hunk
    /// order. Ranges refer to lines of [`Applied::text`]. A hunk that only removes lines is
    /// reported as the single line that now sits where the removal happened (clamped to the last
    /// line). A hunk that changes nothing has no entry.
    pub changed: Vec<(u32, u32)>,
}

/// Reason [`apply_hunks`] could not place a hunk. Nothing is applied when this is returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Miss {
    /// 1-based number of the first hunk that could not be located.
    pub hunk: usize,
    /// 1-based first line of the source window that best matches the hunk. Best effort: the
    /// window with the most lines equal under the final normalisation pass, earliest on ties.
    /// When nothing matches at all, this is `1`.
    pub nearest_start: u32,
    /// 1-based last line (inclusive) of that window, clamped to the source. When nothing
    /// matches, this is `min(line count, 8)`, and at least `1`.
    pub nearest_end: u32,
}

/// Parses a complete `apply_patch` envelope into file operations, in patch order.
///
/// Only blank lines may precede `*** Begin Patch`, and only blank lines may follow `*** End
/// Patch`. Each `*** Add File:` section takes every following line up to the next `***` header;
/// each line must start with `+`. Each `*** Update File:` section holds one or more hunks; a
/// hunk starts at `@@` (optionally with an anchor), or, for the first hunk only, directly with
/// its diff lines. Hunk lines start with `' '`, `'-'` or `'+'`; an empty line is empty context.
/// `*** End of File` directly after a hunk's lines marks that hunk as end-anchored.
///
/// `text` is split with [`str::lines`], so `\r\n` endings are accepted and the carriage return
/// is not part of any line.
///
/// # Errors
///
/// Returns `Err` with a message starting `patch: ` and naming a 1-based line number for: a
/// missing or wrong `*** Begin Patch`; a missing `*** End Patch`; non-blank text after it; an
/// unknown header; a header without a path; an Add line that does not start with `+`; a path
/// used twice; an Update section with no hunks; a hunk with no lines; a hunk line with another
/// prefix; `*** End of File` not following hunk lines; hunk lines after `*** End of File` with
/// no `@@`; and an envelope with no file operations. A `*** Delete File:` or `*** Move to:`
/// line returns exactly `patch: delete and move are not supported; use a native tool`, with no
/// line number.
pub(crate) fn parse_patch(text: &str) -> Result<Vec<PatchFile>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut index = 0;
    while index < lines.len() && lines[index].trim().is_empty() {
        index += 1;
    }
    if index == lines.len() {
        return Err("patch: line 1: empty patch, expected `*** Begin Patch`".to_string());
    }
    if lines[index].trim_end() != "*** Begin Patch" {
        return Err(format!(
            "patch: line {}: expected `*** Begin Patch`",
            index + 1
        ));
    }
    index += 1;

    let mut files = Vec::new();
    let mut seen_paths = HashSet::new();
    loop {
        if index >= lines.len() {
            return Err(format!(
                "patch: line {}: missing `*** End Patch`",
                lines.len() + 1
            ));
        }
        let number = index + 1;
        let line = lines[index].trim_end();
        if is_refused(line) {
            return Err(UNSUPPORTED_OPERATION.to_string());
        }
        if line == "*** End Patch" {
            index += 1;
            break;
        }
        if let Some(rest) = line.strip_prefix("*** Add File:") {
            let path = take_path(rest, number)?;
            claim_path(&mut seen_paths, &path, number)?;
            let mut content = String::new();
            index += 1;
            while index < lines.len() && !lines[index].starts_with("***") {
                match lines[index].strip_prefix('+') {
                    Some(body) => {
                        content.push_str(body);
                        content.push('\n');
                    }
                    None => {
                        return Err(format!(
                            "patch: line {}: Add File lines must start with '+'",
                            index + 1
                        ));
                    }
                }
                index += 1;
            }
            files.push(PatchFile::Add { path, content });
        } else if let Some(rest) = line.strip_prefix("*** Update File:") {
            let path = take_path(rest, number)?;
            claim_path(&mut seen_paths, &path, number)?;
            let (hunks, next) = parse_update_body(&lines, index + 1, number)?;
            files.push(PatchFile::Update { path, hunks });
            index = next;
        } else if line.starts_with("***") {
            return Err(format!("patch: line {}: unknown header `{}`", number, line));
        } else {
            return Err(format!(
                "patch: line {}: expected a `*** Add File:` or `*** Update File:` header",
                number
            ));
        }
    }

    if let Some(offset) = lines[index..]
        .iter()
        .position(|line| !line.trim().is_empty())
    {
        return Err(format!(
            "patch: line {}: text after `*** End Patch`",
            index + offset + 1
        ));
    }
    if files.is_empty() {
        return Err(format!(
            "patch: line {}: patch has no file operations",
            index
        ));
    }
    Ok(files)
}

/// Reports whether a trimmed-right line is one of the operations this parser refuses.
///
/// `line` must already be trimmed of trailing whitespace. Returns `true` for a
/// `*** Delete File:` or `*** Move to:` header, and `false` for every other line, including
/// diff lines that merely mention those words.
fn is_refused(line: &str) -> bool {
    line.starts_with("*** Delete File:") || line.starts_with("*** Move to:")
}

/// Reads the path that follows a file header's colon.
///
/// `rest` is the text after the header keyword, and `number` is the header's 1-based line for
/// the error message. Returns the trimmed path. Returns `Err` when the path is blank.
fn take_path(rest: &str, number: usize) -> Result<String, String> {
    let path = rest.trim();
    if path.is_empty() {
        Err(format!("patch: line {}: file header has no path", number))
    } else {
        Ok(path.to_string())
    }
}

/// Records a path as used and rejects it if it was already used by an earlier section.
///
/// `seen` accumulates every path of the patch so far. Returns `Err` naming `number` when `path`
/// is already present; otherwise `path` is added and `Ok(())` is returned.
fn claim_path(seen: &mut HashSet<String>, path: &str, number: usize) -> Result<(), String> {
    if seen.insert(path.to_string()) {
        Ok(())
    } else {
        Err(format!("patch: line {}: duplicate path `{}`", number, path))
    }
}

/// Parses the hunks of one `*** Update File:` section.
///
/// `start` is the index into `lines` of the first line after the header, and `header` is that
/// header's 1-based line number, used for the "no hunks" error. Parsing stops at the first
/// `***` line other than `*** End of File` (which is consumed), or at the end of input.
/// Returns the hunks in order and the index of the first unconsumed line. Returns `Err` for any
/// malformed hunk, and for a section with no hunks.
fn parse_update_body(
    lines: &[&str],
    start: usize,
    header: usize,
) -> Result<(Vec<Hunk>, usize), String> {
    let mut hunks = Vec::new();
    // The hunk being built, with the 1-based line where it opened.
    let mut open: Option<(Hunk, usize)> = None;
    // A hunk may open without `@@` only before any hunk has been started or closed.
    let mut may_open_bare = true;
    let mut index = start;
    while index < lines.len() {
        let raw = lines[index];
        let number = index + 1;
        if is_refused(raw.trim_end()) {
            return Err(UNSUPPORTED_OPERATION.to_string());
        }
        if raw.trim_end() == "*** End of File" {
            match open.take() {
                Some((mut hunk, _))
                    if !(hunk.old_lines.is_empty() && hunk.new_lines.is_empty()) =>
                {
                    hunk.at_end_of_file = true;
                    hunks.push(hunk);
                }
                _ => {
                    return Err(format!(
                        "patch: line {}: `*** End of File` must follow hunk lines",
                        number
                    ));
                }
            }
            may_open_bare = false;
            index += 1;
            continue;
        }
        if raw.starts_with("***") {
            break;
        }
        if let Some(rest) = raw.strip_prefix("@@") {
            close_hunk(&mut open, &mut hunks)?;
            let anchor = rest.trim();
            let anchor = if anchor.is_empty() {
                None
            } else {
                Some(anchor.to_string())
            };
            open = Some((new_hunk(anchor), number));
            may_open_bare = false;
        } else {
            let (kind, body) = match raw.chars().next() {
                None => (' ', ""),
                Some(c @ (' ' | '-' | '+')) => (c, &raw[1..]),
                Some(_) => {
                    return Err(format!(
                        "patch: line {}: hunk line must start with ' ', '-' or '+'",
                        number
                    ));
                }
            };
            if open.is_none() && !may_open_bare {
                return Err(format!(
                    "patch: line {}: hunk lines must follow an `@@` line",
                    number
                ));
            }
            let (hunk, _) = open.get_or_insert_with(|| (new_hunk(None), number));
            match kind {
                '-' => hunk.old_lines.push(body.to_string()),
                '+' => hunk.new_lines.push(body.to_string()),
                _ => {
                    hunk.old_lines.push(body.to_string());
                    hunk.new_lines.push(body.to_string());
                }
            }
            may_open_bare = false;
        }
        index += 1;
    }
    close_hunk(&mut open, &mut hunks)?;
    if hunks.is_empty() {
        return Err(format!("patch: line {}: Update File has no hunks", header));
    }
    Ok((hunks, index))
}

/// Builds an empty hunk with the given anchor.
///
/// `anchor` is stored as given; both line vectors start empty and `at_end_of_file` is `false`.
fn new_hunk(anchor: Option<String>) -> Hunk {
    Hunk {
        anchor,
        old_lines: Vec::new(),
        new_lines: Vec::new(),
        at_end_of_file: false,
    }
}

/// Moves the open hunk, if any, into `hunks`.
///
/// `open` holds the hunk being built together with its 1-based opening line, and is left `None`.
/// Returns `Err` naming that line when the hunk has no lines at all. A hunk with only context
/// lines is accepted.
fn close_hunk(open: &mut Option<(Hunk, usize)>, hunks: &mut Vec<Hunk>) -> Result<(), String> {
    if let Some((hunk, number)) = open.take() {
        if hunk.old_lines.is_empty() && hunk.new_lines.is_empty() {
            return Err(format!("patch: line {}: hunk has no lines", number));
        }
        hunks.push(hunk);
    }
    Ok(())
}

/// Locates every hunk in `source` and splices its replacement in, all or nothing.
///
/// `source` is split on `\n`. A final `\n` is treated as a terminator, not as an empty line, and
/// its presence is remembered. Hunks are resolved in order against the original text, each
/// search starting at the end of the previous match (or after its anchor, see [`Hunk::anchor`]).
/// For each hunk the first of the four passes that finds a match wins: exact equality; equality
/// ignoring trailing whitespace; equality ignoring leading and trailing whitespace; equality
/// after trimming and mapping curly single and double quotes (including the low and reversed
/// forms) to `'` and `"`, en, em and minus signs to `-`, and a non-breaking space to a space.
/// A hunk with `at_end_of_file` matches only at the end of the source. An empty `old_lines`
/// inserts after the hunk's `@@` anchor line when it has one and finds it, else at the end of the
/// source. A source containing `\r\n` keeps that ending on every line a hunk writes.
///
/// The output keeps the source's final-newline state, except that a hunk that is end-anchored or
/// inserts at the end and adds lines makes the result end with `\n`.
///
/// # Errors
///
/// Returns `Err(Miss)` for the first hunk that no pass places, and nothing is applied. The miss
/// names that hunk's 1-based number and the best-matching source window.
pub(crate) fn apply_hunks(source: &str, hunks: &[Hunk]) -> Result<Applied, Miss> {
    let (lines, trailing_newline) = split_source(source);

    // Resolve every hunk against the original text before building any output.
    let mut placed: Vec<(usize, usize)> = Vec::with_capacity(hunks.len());
    let mut cursor = 0;
    for (index, hunk) in hunks.iter().enumerate() {
        match locate(&lines, hunk, cursor) {
            Some(start) => {
                placed.push((start, hunk.old_lines.len()));
                cursor = start + hunk.old_lines.len();
            }
            None => return Err(miss(&lines, hunk, index + 1)),
        }
    }

    // A CRLF file keeps its line ending: the replacement lines carry the same `\r` the source
    // lines (split on `\n`) already end with.
    let crlf = source.contains("\r\n");
    let mut out: Vec<String> = Vec::new();
    let mut spans: Vec<Span> = Vec::new();
    let mut ends_with_newline = trailing_newline;
    let mut next = 0;
    for (hunk, &(start, old_len)) in hunks.iter().zip(&placed) {
        out.extend(lines[next..start].iter().map(|line| line.to_string()));
        let at = out.len();
        out.extend(hunk.new_lines.iter().map(|line| {
            if crlf {
                format!("{line}\r")
            } else {
                line.clone()
            }
        }));
        if let Some(span) = changed_span(hunk, at) {
            spans.push(span);
        }
        next = start + old_len;
        let appends = hunk.at_end_of_file || (hunk.old_lines.is_empty() && start == lines.len());
        if appends && !hunk.new_lines.is_empty() {
            ends_with_newline = true;
        }
    }
    out.extend(lines[next..].iter().map(|line| line.to_string()));

    let changed = spans
        .into_iter()
        .map(|span| span_range(span, out.len()))
        .collect();
    let mut text = out.join("\n");
    if ends_with_newline && !out.is_empty() {
        text.push('\n');
    }
    Ok(Applied { text, changed })
}

/// Splits a source text into its lines and reports whether it ended with a newline.
///
/// Returns the lines without their terminators and `true` when `source` ended with `\n`. The
/// empty string gives no lines and `false`. A text of exactly `"\n"` gives one empty line.
fn split_source(source: &str) -> (Vec<&str>, bool) {
    if source.is_empty() {
        return (Vec::new(), false);
    }
    match source.strip_suffix('\n') {
        Some(body) => (body.split('\n').collect(), true),
        None => (source.split('\n').collect(), false),
    }
}

/// Finds the 0-based start line in `lines` where `hunk` matches, searching from `cursor`.
///
/// `cursor` is the first line not yet consumed by an earlier hunk. Returns `None` when no pass
/// finds a match. An empty `old_lines` returns `lines.len()`. For an end-anchored hunk only the
/// final position is tried. Otherwise the search runs forward from [`search_start`], pass by
/// pass, and the first position of the first passing pass wins.
fn locate(lines: &[&str], hunk: &Hunk, cursor: usize) -> Option<usize> {
    let old = &hunk.old_lines;
    if old.is_empty() {
        // An add-only hunk goes after its `@@` anchor line when it has one and it is found,
        // else at the end of the source.
        return Some(
            hunk.anchor
                .as_deref()
                .and_then(|anchor| find_anchor(lines, anchor, cursor))
                .unwrap_or(lines.len()),
        );
    }
    let from = search_start(lines, hunk, cursor);
    let last = lines.len().checked_sub(old.len())?;
    for level in 0..PASSES {
        if hunk.at_end_of_file {
            if last >= from && matches_at(lines, old, last, level) {
                return Some(last);
            }
        } else {
            for start in from..=last {
                if matches_at(lines, old, start, level) {
                    return Some(start);
                }
            }
        }
    }
    None
}

/// Returns the first line index at which a hunk's search may begin.
///
/// Without an anchor this is `cursor`. With an anchor it is one past the first line at or after
/// `cursor` whose trimmed text equals the trimmed anchor, or `cursor` when no such line exists.
fn search_start(lines: &[&str], hunk: &Hunk, cursor: usize) -> usize {
    hunk.anchor
        .as_deref()
        .and_then(|anchor| find_anchor(lines, anchor, cursor))
        .unwrap_or(cursor)
}

/// The index one past the first line at or after `cursor` whose trimmed text equals the trimmed
/// `anchor`, or `None` when no such line exists.
fn find_anchor(lines: &[&str], anchor: &str, cursor: usize) -> Option<usize> {
    let wanted = anchor.trim();
    lines
        .iter()
        .enumerate()
        .skip(cursor)
        .find(|(_, line)| line.trim() == wanted)
        .map(|(index, _)| index + 1)
}

/// Reports whether `old` matches the source lines starting at `start` under one pass.
///
/// `start + old.len()` must not exceed `lines.len()`; the caller guarantees this. `level` is the
/// pass number, from `0` (exact) to `PASSES - 1` (normalised), as described in [`same_line`].
fn matches_at(lines: &[&str], old: &[String], start: usize, level: u8) -> bool {
    old.iter()
        .enumerate()
        .all(|(offset, wanted)| same_line(lines[start + offset], wanted, level))
}

/// Compares one source line with one hunk line under the pass numbered `level`.
///
/// `level` 0 is exact equality, 1 ignores trailing whitespace, 2 ignores leading and trailing
/// whitespace, and any other value (3) compares after [`normalise`] and trimming both sides.
fn same_line(source: &str, wanted: &str, level: u8) -> bool {
    match level {
        0 => source == wanted,
        1 => source.trim_end() == wanted.trim_end(),
        2 => source.trim() == wanted.trim(),
        _ => normalise(source).trim() == normalise(wanted).trim(),
    }
}

/// Maps common Unicode punctuation to its ASCII counterpart.
///
/// Curly single quotes (U+2018 to U+201B) become `'`; curly double quotes (U+201C to U+201F)
/// become `"`; en dash, em dash and minus sign (U+2013, U+2014, U+2212) become `-`; a no-break
/// space (U+00A0) becomes a space. Every other character is returned unchanged. The result has
/// the same number of characters or fewer, never more.
fn normalise(line: &str) -> String {
    line.chars()
        .map(|c| match c {
            '\u{2018}'..='\u{201B}' => '\'',
            '\u{201C}'..='\u{201F}' => '"',
            '\u{2013}' | '\u{2014}' | '\u{2212}' => '-',
            '\u{00A0}' => ' ',
            other => other,
        })
        .collect()
}

/// Builds the [`Miss`] for a hunk that no pass placed.
///
/// `lines` is the source and `number` is the hunk's 1-based number. The reported window is the
/// run of `old_lines.len()` source lines with the most lines equal under the final pass (earliest
/// on ties, and at least one line equal). When no line matches, or the source is shorter than
/// the hunk, the window is `1` to `min(line count, 8)`, with the end never below `1`.
// ponytail: scans every window, O(source * hunk); fine for hunk-sized edits, add an index if a
// huge file ever makes a miss slow.
fn miss(lines: &[&str], hunk: &Hunk, number: usize) -> Miss {
    let old = &hunk.old_lines;
    let len = old.len();
    let mut best: Option<(usize, usize)> = None;
    if let Some(last) = lines.len().checked_sub(len) {
        for start in 0..=last {
            if len == 0 {
                break;
            }
            let count = (0..len)
                .filter(|&offset| same_line(lines[start + offset], &old[offset], PASSES - 1))
                .count();
            if count > 0 && best.is_none_or(|(best_count, _)| count > best_count) {
                best = Some((count, start));
            }
        }
    }
    let (start, end) = match best {
        Some((_, start)) => (start + 1, (start + len).min(lines.len())),
        None => (1, lines.len().clamp(1, MISS_WINDOW)),
    };
    Miss {
        hunk: number,
        nearest_start: start as u32,
        nearest_end: end as u32,
    }
}

/// Where one hunk changed the output, before final line numbers are known.
#[derive(Clone, Copy, Debug)]
struct Span {
    /// 0-based index in the output of the first changed line, or of the removal point for a pure
    /// deletion (the line that now follows it).
    at: usize,
    /// Number of output lines the hunk added. Zero for a pure deletion.
    added: usize,
}

/// Works out which output lines one applied hunk changed.
///
/// `hunk` is the applied hunk and `at` is the 0-based output index of its first new line.
/// The changed region is bounded by the common prefix and common suffix of `old_lines` and
/// `new_lines`. Returns `None` when the hunk changes nothing (the two sides are equal).
// ponytail: a changed line that repeats its neighbouring context can shift the reported range
// by a line; an LCS diff would be exact. Add it if ranges start pointing at the wrong line.
fn changed_span(hunk: &Hunk, at: usize) -> Option<Span> {
    let old = &hunk.old_lines;
    let new = &hunk.new_lines;
    let prefix = old
        .iter()
        .zip(new.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let room = old.len().min(new.len()) - prefix;
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();
    let removed = old.len() - prefix - suffix;
    let added = new.len() - prefix - suffix;
    if removed == 0 && added == 0 {
        None
    } else {
        Some(Span {
            at: at + prefix,
            added,
        })
    }
}

/// Converts a [`Span`] into a 1-based inclusive line range of the final output.
///
/// `total` is the number of lines in the whole output. An added span gives its own lines. A
/// pure deletion gives the single line at its removal point, clamped to the last line; when the
/// output is empty it gives `(1, 1)`.
fn span_range(span: Span, total: usize) -> (u32, u32) {
    if span.added > 0 {
        return ((span.at + 1) as u32, (span.at + span.added) as u32);
    }
    if total == 0 {
        return (1, 1);
    }
    let line = (span.at.min(total - 1) + 1) as u32;
    (line, line)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a hunk from string slices so the apply tests stay short.
    ///
    /// `anchor` becomes the hunk's anchor, `old` and `new` become its line vectors, and `eof`
    /// sets `at_end_of_file`. Pure data, no validation.
    fn hunk(anchor: Option<&str>, old: &[&str], new: &[&str], eof: bool) -> Hunk {
        Hunk {
            anchor: anchor.map(str::to_string),
            old_lines: old.iter().map(|line| line.to_string()).collect(),
            new_lines: new.iter().map(|line| line.to_string()).collect(),
            at_end_of_file: eof,
        }
    }

    /// Checks that the Add plus Update example parses into one file each, with the anchor,
    /// the context and removed lines, the added lines, and the end-of-file marker.
    #[test]
    fn parses_add_and_update_example() {
        let text = "*** Begin Patch\n*** Add File: src/new.rs\n+line one\n+line two\n*** Update File: src/a.rs\n@@ pub fn transcript(&self)\n context line\n-removed line\n+added line\n context line\n*** End of File\n*** End Patch\n";
        let files = parse_patch(text).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0],
            PatchFile::Add {
                path: "src/new.rs".to_string(),
                content: "line one\nline two\n".to_string(),
            }
        );
        assert_eq!(
            files[1],
            PatchFile::Update {
                path: "src/a.rs".to_string(),
                hunks: vec![Hunk {
                    anchor: Some("pub fn transcript(&self)".to_string()),
                    old_lines: vec![
                        "context line".to_string(),
                        "removed line".to_string(),
                        "context line".to_string(),
                    ],
                    new_lines: vec![
                        "context line".to_string(),
                        "added line".to_string(),
                        "context line".to_string(),
                    ],
                    at_end_of_file: true,
                }],
            }
        );
    }

    /// Checks that a `*** Delete File:` section is refused with the exact fixed message.
    #[test]
    fn refuses_delete_with_exact_text() {
        let text = "*** Begin Patch\n*** Delete File: a.rs\n*** End Patch\n";
        assert_eq!(
            parse_patch(text),
            Err("patch: delete and move are not supported; use a native tool".to_string())
        );
    }

    /// Checks that a `*** Move to:` line after an update's hunk is refused with the exact
    /// fixed message, not reported as a malformed line.
    #[test]
    fn refuses_move_with_exact_text() {
        let text = "*** Begin Patch\n*** Update File: a.rs\n@@\n-x\n+y\n*** Move to: b.rs\n*** End Patch\n";
        assert_eq!(
            parse_patch(text),
            Err("patch: delete and move are not supported; use a native tool".to_string())
        );
    }

    /// Checks that a patch without `*** End Patch` is rejected with a `patch:` error that
    /// mentions the missing marker.
    #[test]
    fn rejects_missing_end_patch() {
        let err = parse_patch("*** Begin Patch\n*** Add File: a\n+x\n").unwrap_err();
        assert!(err.starts_with("patch: "), "{err}");
        assert!(err.contains("End Patch"), "{err}");
    }

    /// Checks that a hunk line with an unknown prefix is rejected and the error names its
    /// 1-based line number (line 6 of this envelope).
    #[test]
    fn rejects_bad_hunk_prefix_with_line_number() {
        let text = "*** Begin Patch\n*** Update File: a.rs\n@@\n-x\n+y\nnope\n*** End Patch\n";
        let err = parse_patch(text).unwrap_err();
        assert!(err.starts_with("patch: "), "{err}");
        assert!(err.contains("line 6"), "{err}");
    }

    /// Checks that an exact match replaces the one line and reports that line as changed.
    #[test]
    fn applies_exact_match() {
        let applied = apply_hunks("a\nb\nc\n", &[hunk(None, &["b"], &["B"], false)]).unwrap();
        assert_eq!(applied.text, "a\nB\nc\n");
        assert_eq!(applied.changed, vec![(2, 2)]);
    }

    /// Checks that a CRLF file keeps its line ending on the replaced lines.
    #[test]
    fn crlf_files_keep_their_line_endings() {
        let applied = apply_hunks(
            "a\r\nb\r\nc\r\n",
            &[hunk(None, &["b"], &["B", "B2"], false)],
        )
        .unwrap();
        assert_eq!(applied.text, "a\r\nB\r\nB2\r\nc\r\n");
    }

    /// Checks that an add-only hunk with an `@@` anchor goes after the anchor line, not at the
    /// end of the file, and that without a found anchor it still appends.
    #[test]
    fn an_anchored_add_only_hunk_lands_after_its_anchor() {
        let applied = apply_hunks(
            "fn a() {\n}\nfn b() {\n}\n",
            &[hunk(Some("fn a() {"), &[], &["    x();"], false)],
        )
        .unwrap();
        assert_eq!(applied.text, "fn a() {\n    x();\n}\nfn b() {\n}\n");
        let appended = apply_hunks("a\nb\n", &[hunk(Some("missing"), &[], &["z"], false)]).unwrap();
        assert_eq!(appended.text, "a\nb\nz\n");
    }

    /// Checks that a source line with trailing spaces still matches the hunk line without them.
    #[test]
    fn applies_despite_trailing_whitespace() {
        let applied =
            apply_hunks("a \nb\n", &[hunk(None, &["a", "b"], &["x", "b"], false)]).unwrap();
        assert_eq!(applied.text, "x\nb\n");
        assert_eq!(applied.changed, vec![(1, 1)]);
    }

    /// Checks that a source line with different indentation still matches after trimming.
    #[test]
    fn applies_despite_indentation_difference() {
        let applied = apply_hunks(
            "    fn f() {}\n",
            &[hunk(None, &["fn f() {}"], &["fn g() {}"], false)],
        )
        .unwrap();
        assert_eq!(applied.text, "fn g() {}\n");
        assert_eq!(applied.changed, vec![(1, 1)]);
    }

    /// Checks that curly quotes in the source match straight quotes in the hunk.
    #[test]
    fn applies_despite_curly_quotes() {
        let applied = apply_hunks(
            "let s = \u{201C}hi\u{201D};\n",
            &[hunk(None, &["let s = \"hi\";"], &["let s = 1;"], false)],
        )
        .unwrap();
        assert_eq!(applied.text, "let s = 1;\n");
        assert_eq!(applied.changed, vec![(1, 1)]);
    }

    /// Checks that two hunks apply in order and each reports its own changed line.
    #[test]
    fn applies_two_hunks_in_order() {
        let applied = apply_hunks(
            "a\nb\nc\nd\n",
            &[
                hunk(None, &["a"], &["A"], false),
                hunk(None, &["c"], &["C"], false),
            ],
        )
        .unwrap();
        assert_eq!(applied.text, "A\nb\nC\nd\n");
        assert_eq!(applied.changed, vec![(1, 1), (3, 3)]);
    }

    /// Checks that a second hunk whose text lies before the first hunk's match is a miss, and
    /// that the miss names hunk 2.
    #[test]
    fn second_hunk_before_first_is_a_miss() {
        let miss = apply_hunks(
            "a\nb\nc\n",
            &[
                hunk(None, &["c"], &["C"], false),
                hunk(None, &["a"], &["A"], false),
            ],
        )
        .unwrap_err();
        assert_eq!(miss.hunk, 2);
    }

    /// Checks that an end-anchored hunk matches the last lines of the source, and that it does
    /// not match when the text exists only earlier in the file.
    #[test]
    fn end_of_file_hunk_matches_only_at_end() {
        let applied = apply_hunks(
            "a\nb\na\nb\n",
            &[hunk(None, &["a", "b"], &["a", "B"], true)],
        )
        .unwrap();
        assert_eq!(applied.text, "a\nb\na\nB\n");
        assert_eq!(applied.changed, vec![(4, 4)]);

        assert!(apply_hunks("a\nb\n", &[hunk(None, &["a"], &["A"], true)]).is_err());
    }

    /// Checks that a failing second hunk reports hunk number 2 and the nearest window (no
    /// line matches, so lines 1 to 2 of this two-line source).
    #[test]
    fn miss_carries_failing_hunk_number() {
        let miss = apply_hunks(
            "a\nb\n",
            &[
                hunk(None, &["a"], &["A"], false),
                hunk(None, &["zz"], &["Z"], false),
            ],
        )
        .unwrap_err();
        assert_eq!(miss.hunk, 2);
        assert_eq!((miss.nearest_start, miss.nearest_end), (1, 2));
    }

    /// Checks that a hunk that inserts lines reports the new lines' range in the new text, and
    /// that a pure deletion reports the single line where the removal happened.
    #[test]
    fn changed_ranges_refer_to_new_text() {
        let inserted = apply_hunks(
            "a\nb\nc\nd\n",
            &[hunk(None, &["a", "b", "c"], &["a", "X", "Y", "c"], false)],
        )
        .unwrap();
        assert_eq!(inserted.text, "a\nX\nY\nc\nd\n");
        assert_eq!(inserted.changed, vec![(2, 3)]);

        let deleted = apply_hunks(
            "a\nb\nc\n",
            &[hunk(None, &["a", "b", "c"], &["a", "c"], false)],
        )
        .unwrap();
        assert_eq!(deleted.text, "a\nc\n");
        assert_eq!(deleted.changed, vec![(2, 2)]);
    }
}
