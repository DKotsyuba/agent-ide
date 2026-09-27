//! Line and brace helpers shared by the support modules of brace-delimited languages.
//!
//! They read declaration headers (skipping comment and attribute lines), collapse a declaration
//! to one signature line, compute insertion sites from `{`…`}` bodies, and normalize test and
//! command lists. Everything here is a pure function of its inputs.

use std::path::PathBuf;

use super::{InsertSite, InsertWhere, LangError, LineRange, Outline, Symbol, SymbolPath, TestId};

/// Longest signature kept on a symbol card, in characters (the ellipsis included).
const SIGNATURE_LIMIT: usize = 200;

/// A failure's optional `file:line` location and its first message line, as parsed from output.
pub type Located = (Option<(PathBuf, u32)>, String);

/// First declaration line in `from..limit` (1-based): skips blank lines, `//` comments and (when
/// `attributes`) possibly multi-line `#[...]` attributes that a server range may start with.
/// Returns `limit` (the name line) when every line before it is header.
pub fn declaration_line(lines: &[&str], from: u32, limit: u32, attributes: bool) -> u32 {
    let mut line = from;
    let mut depth = 0i32;
    while line < limit {
        let text = line_at(lines, line).trim();
        let brackets = text.matches('[').count() as i32 - text.matches(']').count() as i32;
        if depth > 0 {
            depth += brackets;
        } else if text.is_empty() || text.starts_with("//") {
        } else if attributes && text.starts_with("#[") {
            depth = brackets;
        } else {
            break;
        }
        line += 1;
    }
    line
}

/// The declaration in `body` collapsed to one line.
///
/// With `item_ends` (items that may end at `;` or `=` and use `<>` generics): stops before the
/// `{` that opens the body or the `;`/`=` that ends the item, at bracket depth 0 (`()`, `[]`,
/// `<>`; `->` is not a bracket). Without it: stops before the first depth-0 `{` that is not an
/// empty `{}` type literal, and keeps only the first line when there is none. Line comments are dropped, whitespace collapsed, padding inside brackets and trailing
/// commas removed, and the result cut to `SIGNATURE_LIMIT` characters with `…`.
pub fn signature(lines: &[&str], body: LineRange, item_ends: bool) -> String {
    let joined: Vec<char> = (body.start..=body.end)
        .map(|line| strip_line_comment(line_at(lines, line)))
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .collect();
    let mut depth = 0i32;
    let mut end = None;
    let mut index = 0;
    while index < joined.len() {
        let ch = joined[index];
        let prev = index.checked_sub(1).map(|at| joined[at]);
        let next = joined.get(index + 1).copied();
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            '<' if item_ends => depth += 1,
            '>' if item_ends && prev != Some('-') && prev != Some('=') => depth -= 1,
            '{' if depth <= 0 => {
                let rest = joined[index + 1..].iter().find(|ch| !ch.is_whitespace());
                if item_ends || rest != Some(&'}') {
                    end = Some(index);
                    break;
                }
            }
            ';' if item_ends && depth <= 0 => {
                end = Some(index);
                break;
            }
            '=' if item_ends
                && depth <= 0
                && !matches!(next, Some('=' | '>'))
                && !matches!(prev, Some('=' | '<' | '>' | '!')) =>
            {
                end = Some(index);
                break;
            }
            _ => {}
        }
        index += 1;
    }
    let text: String = match end {
        Some(end) => joined[..end].iter().collect(),
        None if item_ends => joined.iter().collect(),
        None => strip_line_comment(line_at(lines, body.start)).to_owned(),
    };
    finish_signature(&text)
}

/// Whitespace-normalizes a collected declaration and bounds it to `SIGNATURE_LIMIT` characters.
fn finish_signature(text: &str) -> String {
    let mut text = collapse_whitespace(text);
    for (from, to) in [
        ("( ", "("),
        (" )", ")"),
        (",)", ")"),
        ("< ", "<"),
        (" >", ">"),
        (",>", ">"),
    ] {
        text = text.replace(from, to);
    }
    let text = text.trim_end_matches(',').trim_end();
    if text.chars().count() <= SIGNATURE_LIMIT {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(SIGNATURE_LIMIT - 1).collect();
    cut.push('…');
    cut
}

/// Collapses every whitespace run to one space and trims the ends.
pub fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `line` without a trailing `//` comment; `//` inside a `"…"` or `` `…` `` literal is kept.
pub fn strip_line_comment(line: &str) -> &str {
    let mut quote = None;
    let mut previous = '\0';
    for (index, ch) in line.char_indices() {
        match quote {
            Some(open) if ch == open && previous != '\\' => quote = None,
            Some(_) => {}
            None if ch == '"' || ch == '`' => quote = Some(ch),
            None if ch == '/' && previous == '/' => return &line[..index - 1],
            None => {}
        }
        previous = if previous == '\\' && ch == '\\' {
            '\0'
        } else {
            ch
        };
    }
    line
}

/// First paragraph of doc lines whose comment markers are already removed: one optional leading
/// space is dropped per line, leading empty lines skipped, and the paragraph ends at the next empty
/// line; lines are joined by single spaces. `None` when there is no text.
pub fn first_paragraph<'a>(docs: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut paragraph: Vec<&str> = Vec::new();
    for line in docs {
        let line = line.strip_prefix(' ').unwrap_or(line).trim_end();
        if line.trim().is_empty() {
            if paragraph.is_empty() {
                continue;
            }
            break;
        }
        paragraph.push(line.trim());
    }
    (!paragraph.is_empty()).then(|| paragraph.join(" "))
}

/// Lines of `source` without terminators (`\r\n` handled).
pub fn source_lines(source: &str) -> Vec<&str> {
    source.lines().collect()
}

/// The 1-based line `line`, or `""` outside the source.
pub fn line_at<'a>(lines: &[&'a str], line: u32) -> &'a str {
    line.checked_sub(1)
        .and_then(|index| lines.get(index as usize))
        .copied()
        .unwrap_or("")
}

/// Leading whitespace of the 1-based line `line`.
fn indent_at(lines: &[&str], line: u32) -> String {
    let text = line_at(lines, line);
    text[..text.len() - text.trim_start().len()].to_owned()
}

/// Whether the 1-based line `line` is blank or past the end of the source.
fn blank_or_eof(lines: &[&str], line: u32) -> bool {
    line_at(lines, line).trim().is_empty()
}

/// Shared insertion rules for brace-delimited languages.
///
/// * `Before`: at the anchor's header start, anchor indentation, one blank line before unless the
///   line above is blank (or the anchor starts the file), one after.
/// * `After`: the line after the anchor's end, anchor indentation, one blank before, one after
///   unless the next line is blank or the end of file.
/// * `First`/`Last` on a container (`is_container`): after the line holding its opening `{`
///   (skipping `//!` and `#![...]` inner lines) resp. after its last member (or before the
///   closing line when empty); indentation of the first member, else the container's own plus
///   `unit`. A container without a `{` but with members (Go's synthetic method sets) places
///   `First` at its first member and `Last` like `After` its last member.
///
/// Errors: [`LangError::UnknownSymbol`] when `anchor` is not in `outline`,
/// [`LangError::NotAContainer`] for `First`/`Last` on a non-container or a body-less container
/// (`mod x;`), [`LangError::Unparseable`] when the container's braces share one line.
pub fn place(
    source: &str,
    outline: &Outline,
    anchor: &SymbolPath,
    where_: InsertWhere,
    unit: &str,
    is_container: impl Fn(&Symbol) -> bool,
) -> Result<InsertSite, LangError> {
    let symbol = outline
        .find(anchor)
        .ok_or_else(|| LangError::UnknownSymbol(anchor.clone()))?;
    let lines = source_lines(source);
    let after = |end: u32, indent: String| InsertSite {
        line: end + 1,
        indent,
        blank_before: 1,
        blank_after: u8::from(!blank_or_eof(&lines, end + 1)),
    };
    match where_ {
        InsertWhere::Before => {
            let line = symbol.range.start;
            Ok(InsertSite {
                line,
                indent: indent_at(&lines, line),
                blank_before: u8::from(line > 1 && !blank_or_eof(&lines, line - 1)),
                blank_after: 1,
            })
        }
        InsertWhere::After => Ok(after(
            symbol.range.end,
            indent_at(&lines, symbol.range.start),
        )),
        InsertWhere::First | InsertWhere::Last => {
            if !is_container(symbol) {
                return Err(LangError::NotAContainer(anchor.clone()));
            }
            let first = symbol.children.first();
            let last = symbol.children.last();
            let search_end = first.map_or(symbol.body.end, |child| child.range.start - 1);
            let brace = (symbol.body.start..=search_end)
                .find(|line| strip_line_comment(line_at(&lines, *line)).contains('{'));
            let member_indent = match first {
                Some(child) => indent_at(&lines, child.range.start),
                None => format!("{}{unit}", indent_at(&lines, symbol.body.start)),
            };
            let Some(brace) = brace else {
                return match (where_, first, last) {
                    (InsertWhere::First, Some(child), _) => Ok(InsertSite {
                        line: child.range.start,
                        indent: member_indent,
                        blank_before: 0,
                        blank_after: 1,
                    }),
                    (_, _, Some(child)) => Ok(after(child.range.end, member_indent)),
                    _ => Err(LangError::NotAContainer(anchor.clone())),
                };
            };
            if brace >= symbol.body.end {
                return Err(LangError::Unparseable(format!(
                    "{anchor} opens and closes its body on one line"
                )));
            }
            if let (InsertWhere::Last, Some(child)) = (where_, last) {
                return Ok(InsertSite {
                    line: child.range.end + 1,
                    indent: member_indent,
                    blank_before: 1,
                    blank_after: 0,
                });
            }
            if where_ == InsertWhere::Last {
                return Ok(InsertSite {
                    line: symbol.body.end,
                    indent: member_indent,
                    blank_before: 0,
                    blank_after: 0,
                });
            }
            let mut line = brace + 1;
            while line < symbol.body.end && {
                let text = line_at(&lines, line).trim();
                text.starts_with("//!") || text.starts_with("#![")
            } {
                line += 1;
            }
            Ok(InsertSite {
                line,
                indent: member_indent,
                blank_before: 0,
                blank_after: u8::from(first.is_some()),
            })
        }
    }
}

/// `tests` with duplicate entries removed, first occurrence kept, order preserved.
pub fn dedup_tests(tests: &[TestId]) -> Vec<TestId> {
    let mut unique: Vec<TestId> = Vec::new();
    for test in tests {
        if !unique.contains(test) {
            unique.push(test.clone());
        }
    }
    unique
}

/// Splits a command line on whitespace (no quoting: callers only pass quote-free commands).
pub fn argv_of(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}
