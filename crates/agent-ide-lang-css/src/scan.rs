//! Hand-written style-sheet tokenizer shared by the outline and the name facts.
//!
//! [`Sheet::parse`] splits the text into blocks (`prelude { … }`) and statements (`…;`) while
//! skipping comments (`/* */`, plus `//` in SCSS, Sass and LESS), strings, escapes, `url(…)` and
//! interpolation (`#{…}`, `@{…}`), so braces and semicolons inside them never count. Indented
//! Sass is first given braces ([`braces_from_indentation`]) without moving any original byte's
//! line or column. Spans are byte ranges into the parsed text.

use std::{borrow::Cow, ops::Range, path::Path};

/// Deepest block nesting followed; deeper braces are read as plain text.
const MAX_DEPTH: usize = 64;

/// One `prelude { … }` block: a rule or an at-rule.
#[derive(Debug)]
pub(crate) struct Block {
    /// Selector list or at-rule prelude, trimmed of whitespace and comments.
    pub prelude: Range<usize>,
    /// Byte of the closing `}` (the text length when unclosed).
    pub close: usize,
    /// Nested blocks.
    pub blocks: Vec<Block>,
    /// Declarations and at-rule statements directly inside, trimmed.
    pub statements: Vec<Range<usize>>,
}

/// A parsed style sheet.
#[derive(Debug)]
pub(crate) struct Sheet<'a> {
    /// The parsed text (Sass already given braces).
    pub text: &'a str,
    /// Whether `//` starts a comment (every dialect but plain CSS).
    line_comments: bool,
    /// Byte offset of every line start, for positions.
    line_starts: Vec<usize>,
    /// Top-level blocks.
    pub blocks: Vec<Block>,
    /// Top-level statements.
    pub statements: Vec<Range<usize>>,
}

impl<'a> Sheet<'a> {
    /// Parses `text`; `line_comments` enables `//` comments.
    pub fn parse(text: &'a str, line_comments: bool) -> Self {
        let line_starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(index, _)| index + 1))
            .collect();
        let mut sheet = Self {
            text,
            line_comments,
            line_starts,
            blocks: Vec::new(),
            statements: Vec::new(),
        };
        let mut at = 0;
        loop {
            let (blocks, statements) = sheet.items(&mut at, 0);
            sheet.blocks.extend(blocks);
            sheet.statements.extend(statements);
            if at >= text.len() {
                return sheet;
            }
            at += 1; // a stray `}` at the top level
        }
    }

    /// Blocks and statements from `at` up to the `}` closing the current block (left unconsumed)
    /// or the end of the text.
    fn items(&self, at: &mut usize, depth: usize) -> (Vec<Block>, Vec<Range<usize>>) {
        let bytes = self.text.as_bytes();
        let (mut blocks, mut statements) = (Vec::new(), Vec::new());
        let mut start = *at;
        while *at < bytes.len() {
            if let Some(next) = self.skip(*at) {
                *at = next;
                continue;
            }
            match bytes[*at] {
                b'{' if depth < MAX_DEPTH => {
                    let prelude = self.trim(start..*at);
                    *at += 1;
                    let (inner, inner_statements) = self.items(at, depth + 1);
                    blocks.push(Block {
                        prelude,
                        close: *at,
                        blocks: inner,
                        statements: inner_statements,
                    });
                    *at = (*at + 1).min(bytes.len());
                    start = *at;
                }
                b'}' => break,
                b';' => {
                    statements.push(self.trim(start..*at));
                    *at += 1;
                    start = *at;
                }
                _ => *at += 1,
            }
        }
        statements.push(self.trim(start..*at));
        statements.retain(|span| !span.is_empty());
        (blocks, statements)
    }

    /// End of the comment, string, escape, interpolation or `url(…)` starting at `at`, if one
    /// does.
    pub fn skip(&self, at: usize) -> Option<usize> {
        let bytes = self.text.as_bytes();
        let rest = &bytes[at..];
        let end_of = |from: usize, needle: &[u8]| {
            bytes[from..]
                .windows(needle.len())
                .position(|window| window == needle)
                .map_or(bytes.len(), |found| from + found + needle.len())
        };
        if rest.starts_with(b"/*") {
            return Some(end_of(at + 2, b"*/"));
        }
        if self.line_comments && rest.starts_with(b"//") {
            return Some(end_of(at, b"\n").min(bytes.len()));
        }
        match rest[0] {
            quote @ (b'"' | b'\'') => {
                let mut index = at + 1;
                while index < bytes.len() {
                    match bytes[index] {
                        b'\\' => index += 1 + char_len(bytes.get(index + 1).copied()),
                        b'\n' => return Some(index),
                        byte if byte == quote => return Some(index + 1),
                        _ => index += 1,
                    }
                }
                Some(bytes.len())
            }
            b'\\' => Some((at + 1 + char_len(rest.get(1).copied())).min(bytes.len())),
            b'#' | b'@' if rest.get(1) == Some(&b'{') => {
                let mut depth = 0usize;
                for (offset, byte) in rest.iter().enumerate().skip(1) {
                    match byte {
                        b'{' => depth += 1,
                        b'}' if depth == 1 => return Some(at + offset + 1),
                        b'}' => depth -= 1,
                        _ => {}
                    }
                }
                Some(bytes.len())
            }
            b'u' | b'U'
                if rest.len() >= 4
                    && rest[..4].eq_ignore_ascii_case(b"url(")
                    && !(at > 0 && is_name_byte(bytes[at - 1])) =>
            {
                Some(end_of(at + 4, b")"))
            }
            _ => None,
        }
    }

    /// Whether the skippable run at `at` is a comment (as opposed to a string or escape).
    pub fn is_comment(&self, at: usize) -> bool {
        let rest = &self.text.as_bytes()[at..];
        rest.starts_with(b"/*") || (self.line_comments && rest.starts_with(b"//"))
    }

    /// Whether the skippable run at `at` is an interpolation.
    pub fn is_interpolation(&self, at: usize) -> bool {
        let rest = &self.text.as_bytes()[at..];
        matches!(rest, [b'#' | b'@', b'{', ..])
    }

    /// `span` without leading and trailing whitespace and comments.
    fn trim(&self, span: Range<usize>) -> Range<usize> {
        let bytes = self.text.as_bytes();
        let mut start = span.start;
        while start < span.end {
            if bytes[start].is_ascii_whitespace() {
                start += 1;
            } else if self.is_comment(start) {
                start = self.skip(start).unwrap_or(span.end).min(span.end);
            } else {
                break;
            }
        }
        // The end of the last code byte, so trailing whitespace and comments drop too.
        let mut index = start;
        let mut code_end = start;
        while index < span.end {
            if self.is_comment(index) {
                index = self.skip(index).unwrap_or(span.end).min(span.end);
                continue;
            }
            let next = self.skip(index).unwrap_or(index + 1).min(span.end);
            if !bytes[index].is_ascii_whitespace() {
                code_end = next;
            }
            index = next;
        }
        start..code_end
    }

    /// The text of `span` with comments removed and whitespace runs collapsed to one space.
    pub fn normalized(&self, span: &Range<usize>) -> String {
        let bytes = self.text.as_bytes();
        let mut out = String::new();
        let mut index = span.start;
        let mut space = false;
        while index < span.end {
            if self.is_comment(index) {
                index = self.skip(index).unwrap_or(span.end).min(span.end);
                space = true;
                continue;
            }
            let next = self
                .skip(index)
                .unwrap_or_else(|| index + char_len(Some(bytes[index])))
                .min(span.end);
            if bytes[index].is_ascii_whitespace() {
                space = true;
            } else {
                if space && !out.is_empty() {
                    out.push(' ');
                }
                space = false;
                out.push_str(&self.text[index..next]);
            }
            index = next;
        }
        out
    }

    /// Reads a name (ident characters and escapes, decoded) starting at `at`; returns it and the
    /// byte after it.
    pub fn name(&self, mut at: usize, end: usize) -> (String, usize) {
        let bytes = self.text.as_bytes();
        let mut name = String::new();
        while at < end {
            let byte = bytes[at];
            if byte == b'\\' && at + 1 < end {
                let hex = bytes[at + 1..end]
                    .iter()
                    .take(6)
                    .take_while(|byte| byte.is_ascii_hexdigit())
                    .count();
                if hex > 0 {
                    let code = u32::from_str_radix(&self.text[at + 1..at + 1 + hex], 16)
                        .ok()
                        .and_then(char::from_u32)
                        .unwrap_or(char::REPLACEMENT_CHARACTER);
                    name.push(code);
                    at += 1 + hex;
                    if at < end && matches!(bytes[at], b' ' | b'\t' | b'\n') {
                        at += 1;
                    }
                } else {
                    let escaped = self.text[at + 1..].chars().next().unwrap_or('\\');
                    name.push(escaped);
                    at += 1 + escaped.len_utf8();
                }
            } else if is_name_byte(byte) {
                let ch = self.text[at..].chars().next().unwrap_or('\u{fffd}');
                name.push(ch);
                at += ch.len_utf8();
            } else {
                break;
            }
        }
        (name, at)
    }

    /// Whether a name can start at `at` (a letter, `_`, `-`, a non-ASCII character or an escape).
    pub fn starts_name(&self, at: usize, end: usize) -> bool {
        at < end && {
            let byte = self.text.as_bytes()[at];
            byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'-' | b'\\') || byte >= 0x80
        }
    }

    /// 1-based `(line, column)` of byte `at`; the column counts bytes.
    pub fn position(&self, at: usize) -> (u32, u32) {
        let line = self.line_starts.partition_point(|start| *start <= at);
        (line as u32, (at - self.line_starts[line - 1] + 1) as u32)
    }
}

/// The text to parse for `file` and whether `//` starts a comment: plain CSS has no line
/// comments, indented Sass is given braces first ([`braces_from_indentation`]).
pub(crate) fn sheet_text<'a>(file: &Path, source: &'a str) -> (Cow<'a, str>, bool) {
    match file.extension().and_then(|extension| extension.to_str()) {
        Some("css") => (Cow::Borrowed(source), false),
        Some("sass") => (Cow::Owned(braces_from_indentation(source)), true),
        _ => (Cow::Borrowed(source), true),
    }
}

/// Whether `byte` continues a name: ASCII alphanumerics, `-`, `_`, or part of a non-ASCII
/// character.
pub(crate) fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') || byte >= 0x80
}

/// Byte length of the UTF-8 character whose first byte is `first` (1 for none).
fn char_len(first: Option<u8>) -> usize {
    match first {
        Some(byte) if byte >= 0xf0 => 4,
        Some(byte) if byte >= 0xe0 => 3,
        Some(byte) if byte >= 0xc0 => 2,
        _ => 1,
    }
}

/// Gives indented Sass the braces and semicolons of SCSS: a line followed by a more indented one
/// opens a block, any other line ends a statement, and dedents close blocks. Only appends at line
/// ends (before a trailing ` //` comment), so every original byte keeps its line and column.
///
/// ponytail: multi-line `/* */` comments and `//` inside values are not recognized here; a
/// full Sass parser if indented Sass matters more.
pub(crate) fn braces_from_indentation(source: &str) -> String {
    let lines: Vec<&str> = source.split('\n').collect();
    let indent = |line: &str| line.len() - line.trim_start().len();
    let significant = |line: &str| {
        let trimmed = line.trim();
        !trimmed.is_empty() && !trimmed.starts_with("//") && !trimmed.starts_with("/*")
    };
    let mut out = String::with_capacity(source.len() + lines.len() * 2);
    let mut open: Vec<usize> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let (code, comment) = line
            .find(" //")
            .map_or((*line, ""), |cut| line.split_at(cut));
        out.push_str(code.trim_end_matches('\r'));
        if significant(line) {
            let mine = indent(line);
            let next = lines[index + 1..]
                .iter()
                .find(|line| significant(line))
                .map(|line| indent(line));
            if next.is_some_and(|next| next > mine) {
                out.push_str(" {");
                open.push(mine);
            } else {
                out.push(';');
                let next = next.unwrap_or(0);
                while open.last().is_some_and(|&level| level >= next) {
                    out.push('}');
                    open.pop();
                }
            }
        }
        out.push_str(comment);
        if index + 1 < lines.len() {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Blocks nest, statements split on `;`, and braces or semicolons inside comments, strings,
    /// interpolation and `url()` never count.
    #[test]
    fn parses_blocks_and_statements_around_opaque_runs() {
        let text = ".a { color: red; /* } ; */ content: \"}\"; b: url(x;y) }\n@media x { .b{} }";
        let sheet = Sheet::parse(text, false);
        assert_eq!(sheet.blocks.len(), 2);
        let first = &sheet.blocks[0];
        assert_eq!(&text[first.prelude.clone()], ".a");
        let statements: Vec<&str> = first
            .statements
            .iter()
            .map(|span| &text[span.clone()])
            .collect();
        assert_eq!(statements, ["color: red", "content: \"}\"", "b: url(x;y)"]);
        assert_eq!(&text[sheet.blocks[1].prelude.clone()], "@media x");
        assert_eq!(&text[sheet.blocks[1].blocks[0].prelude.clone()], ".b");
        let scss = ".a-#{$x} { // }\n c: d }";
        let sheet = Sheet::parse(scss, true);
        assert_eq!(&scss[sheet.blocks[0].prelude.clone()], ".a-#{$x}");
        assert_eq!(sheet.blocks[0].statements.len(), 1);
    }

    /// Names decode escapes (`\:`, `\/`, hex) and stop at the first non-name byte.
    #[test]
    fn names_decode_escapes() {
        let sheet = Sheet::parse(".sm\\:p-4 .w-1\\/2 .\\31 0x .é:hover", false);
        assert_eq!(sheet.name(1, sheet.text.len()), ("sm:p-4".into(), 8));
        assert_eq!(sheet.name(10, sheet.text.len()).0, "w-1/2");
        assert_eq!(sheet.name(18, sheet.text.len()).0, "10x");
        assert_eq!(sheet.name(26, sheet.text.len()).0, "é");
        assert_eq!(sheet.position(26), (1, 27));
    }

    /// Indented Sass gains braces at line ends only; original bytes keep their positions.
    #[test]
    fn indented_sass_gains_braces() {
        let sass = ".a\n  color: red\n  &-b\n    x: y // note\n.c\n  d: e\n";
        let text = braces_from_indentation(sass);
        assert_eq!(
            text,
            ".a {\n  color: red;\n  &-b {\n    x: y;}} // note\n.c {\n  d: e;}\n"
        );
        let sheet = Sheet::parse(&text, true);
        assert_eq!(sheet.blocks.len(), 2);
        assert_eq!(&text[sheet.blocks[0].blocks[0].prelude.clone()], "&-b");
    }
}
