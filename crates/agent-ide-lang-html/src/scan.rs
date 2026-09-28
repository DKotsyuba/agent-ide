//! Hand-written HTML tokenizer shared by the outline and the name facts.
//!
//! [`Document::parse`] reads start and end tags into an element tree: tag and attribute names
//! are lowercased, attribute values may be double-quoted, single-quoted or unquoted, comments
//! (`<!-- -->`), doctypes and processing instructions are skipped, void elements and `/>` never
//! open, and `<script>`/`<style>` bodies are skipped whole. An end tag closes the nearest open
//! element with its name (and everything opened inside it); an unmatched end tag is ignored.
//! Spans are byte ranges into the source.

use std::ops::Range;

/// Elements that never have content.
const VOID: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];
/// Elements whose body is raw text, skipped up to their end tag.
const RAW_TEXT: [&str; 2] = ["script", "style"];
/// Deepest element nesting kept open; deeper start tags are read but not nested.
const MAX_DEPTH: usize = 256;

/// One attribute of a start tag.
#[derive(Debug)]
pub(crate) struct Attribute {
    /// Lowercased name.
    pub name: String,
    /// Raw value bytes without quotes, if the attribute has a value.
    pub value: Option<Range<usize>>,
}

/// One element with its attributes and child elements.
#[derive(Debug)]
pub(crate) struct Element {
    /// Lowercased tag name.
    pub tag: String,
    /// The start tag, `<` through `>`.
    pub start_tag: Range<usize>,
    /// Byte after the end tag (or where the element was implicitly closed).
    pub end: usize,
    /// Attributes in source order.
    pub attributes: Vec<Attribute>,
    /// Child elements in source order.
    pub children: Vec<Element>,
}

impl Element {
    /// The raw value of attribute `name`, if present with a value.
    pub fn attribute(&self, name: &str) -> Option<&Range<usize>> {
        self.attributes
            .iter()
            .find(|attribute| attribute.name == name)
            .and_then(|attribute| attribute.value.as_ref())
    }
}

/// A parsed document.
#[derive(Debug)]
pub(crate) struct Document<'a> {
    /// The source.
    pub text: &'a str,
    /// Byte offset of every line start, for positions.
    line_starts: Vec<usize>,
    /// Top-level elements.
    pub elements: Vec<Element>,
}

impl<'a> Document<'a> {
    /// Parses `text`.
    pub fn parse(text: &'a str) -> Self {
        let bytes = text.as_bytes();
        let line_starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(index, _)| index + 1))
            .collect();
        let mut roots: Vec<Element> = Vec::new();
        let mut open: Vec<Element> = Vec::new();
        let mut at = 0;
        while let Some(found) = bytes[at..].iter().position(|byte| *byte == b'<') {
            at += found;
            let rest = &bytes[at..];
            if rest.starts_with(b"<!--") {
                at = find(bytes, at + 4, b"-->").map_or(bytes.len(), |end| end + 3);
            } else if rest.starts_with(b"<!") || rest.starts_with(b"<?") {
                at = find(bytes, at, b">").map_or(bytes.len(), |end| end + 1);
            } else if rest.starts_with(b"</") && rest.get(2).is_some_and(u8::is_ascii_alphabetic) {
                let (tag, after) = tag_name(bytes, at + 2);
                let end = find(bytes, after, b">").map_or(bytes.len(), |end| end + 1);
                if let Some(index) = open.iter().rposition(|element| element.tag == tag) {
                    while open.len() > index {
                        let mut element = open.pop().expect("nonempty");
                        element.end = if open.len() == index { end } else { at };
                        attach(&mut open, &mut roots, element);
                    }
                }
                at = end;
            } else if rest.get(1).is_some_and(u8::is_ascii_alphabetic) {
                let (tag, after) = tag_name(bytes, at + 1);
                let (attributes, closed, end) = attributes(bytes, after);
                let mut element = Element {
                    tag,
                    start_tag: at..end,
                    end,
                    attributes,
                    children: Vec::new(),
                };
                at = end;
                if RAW_TEXT.contains(&element.tag.as_str()) {
                    let close = format!("</{}", element.tag);
                    at = find_ignore_case(bytes, at, close.as_bytes())
                        .map_or(bytes.len(), |start| {
                            find(bytes, start, b">").map_or(bytes.len(), |end| end + 1)
                        });
                    element.end = at;
                    attach(&mut open, &mut roots, element);
                } else if closed || VOID.contains(&element.tag.as_str()) || open.len() >= MAX_DEPTH
                {
                    attach(&mut open, &mut roots, element);
                } else {
                    open.push(element);
                }
            } else {
                at += 1;
            }
        }
        while let Some(mut element) = open.pop() {
            element.end = bytes.len();
            attach(&mut open, &mut roots, element);
        }
        Self {
            text,
            line_starts,
            elements: roots,
        }
    }

    /// 1-based `(line, column)` of byte `at`; the column counts bytes.
    pub fn position(&self, at: usize) -> (u32, u32) {
        let line = self.line_starts.partition_point(|start| *start <= at);
        (line as u32, (at - self.line_starts[line - 1] + 1) as u32)
    }
}

/// Attaches a finished element to the innermost open element, or to the roots.
fn attach(open: &mut [Element], roots: &mut Vec<Element>, element: Element) {
    match open.last_mut() {
        Some(parent) => parent.children.push(element),
        None => roots.push(element),
    }
}

/// First index of `needle` at or after `from`.
fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|found| from + found)
}

/// First index of `needle` at or after `from`, ASCII case-insensitively.
fn find_ignore_case(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
        .map(|found| from + found)
}

/// Lowercased tag name starting at `at` and the byte after it.
fn tag_name(bytes: &[u8], at: usize) -> (String, usize) {
    let end = bytes[at..]
        .iter()
        .position(|byte| byte.is_ascii_whitespace() || matches!(byte, b'>' | b'/'))
        .map_or(bytes.len(), |length| at + length);
    (
        String::from_utf8_lossy(&bytes[at..end]).to_ascii_lowercase(),
        end,
    )
}

/// Attributes from `at` up to the tag's `>`: the attributes, whether the tag self-closed
/// (`/>`), and the byte after `>`.
fn attributes(bytes: &[u8], mut at: usize) -> (Vec<Attribute>, bool, usize) {
    let mut found = Vec::new();
    loop {
        while at < bytes.len() && (bytes[at].is_ascii_whitespace() || bytes[at] == b'/') {
            if bytes[at] == b'/' && bytes.get(at + 1) == Some(&b'>') {
                return (found, true, at + 2);
            }
            at += 1;
        }
        if at >= bytes.len() {
            return (found, false, bytes.len());
        }
        if bytes[at] == b'>' {
            return (found, false, at + 1);
        }
        let name_start = at;
        while at < bytes.len()
            && !bytes[at].is_ascii_whitespace()
            && !matches!(bytes[at], b'=' | b'>')
            && !(bytes[at] == b'/' && bytes.get(at + 1) == Some(&b'>'))
        {
            at += 1;
        }
        let name = String::from_utf8_lossy(&bytes[name_start..at]).to_ascii_lowercase();
        let mut after_name = at;
        while after_name < bytes.len() && bytes[after_name].is_ascii_whitespace() {
            after_name += 1;
        }
        let mut value = None;
        if bytes.get(after_name) == Some(&b'=') {
            at = after_name + 1;
            while at < bytes.len() && bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            match bytes.get(at) {
                Some(&quote @ (b'"' | b'\'')) => {
                    let end = bytes[at + 1..]
                        .iter()
                        .position(|byte| *byte == quote)
                        .map_or(bytes.len(), |length| at + 1 + length);
                    value = Some(at + 1..end);
                    at = (end + 1).min(bytes.len());
                }
                _ => {
                    let start = at;
                    while at < bytes.len() && !bytes[at].is_ascii_whitespace() && bytes[at] != b'>'
                    {
                        at += 1;
                    }
                    value = Some(start..at);
                }
            }
        }
        found.push(Attribute { name, value });
    }
}

/// Decodes character references in `raw`: the common named ones and numeric ones; unknown
/// references stay as written.
pub(crate) fn decode(raw: &str) -> String {
    const NAMED: [(&str, &str); 6] = [
        ("amp;", "&"),
        ("lt;", "<"),
        ("gt;", ">"),
        ("quot;", "\""),
        ("apos;", "'"),
        ("nbsp;", "\u{a0}"),
    ];
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        let numeric = rest.strip_prefix('#').and_then(|number| {
            let end = number.find(';')?;
            let (digits, radix) = match number[..end].strip_prefix(['x', 'X']) {
                Some(hex) => (hex, 16),
                None => (&number[..end], 10),
            };
            let ch = u32::from_str_radix(digits, radix)
                .ok()
                .and_then(char::from_u32)?;
            Some((ch.to_string(), end + 2))
        });
        let named = NAMED
            .iter()
            .find(|(name, _)| rest.starts_with(name))
            .map(|(name, text)| ((*text).to_owned(), name.len()));
        match numeric.or(named) {
            Some((text, consumed)) => {
                out.push_str(&text);
                rest = &rest[consumed..];
            }
            None => out.push('&'),
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(tag, children)` tree of a document, for compact assertions.
    fn tree(elements: &[Element]) -> String {
        elements
            .iter()
            .map(|element| {
                if element.children.is_empty() {
                    element.tag.clone()
                } else {
                    format!("{}({})", element.tag, tree(&element.children))
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Elements nest; void elements, `/>`, comments, doctypes and raw-text bodies never open; an
    /// end tag closes everything opened inside its element; case never matters.
    #[test]
    fn builds_the_element_tree() {
        let text = "<!DOCTYPE html><HTML><body><!-- <div> --><div><p>one<br><img src=x />\
                    <script>if (a < b) { '</div>' }</SCRIPT></div><span></body></html>";
        let document = Document::parse(text);
        assert_eq!(
            tree(&document.elements),
            "html(body(div(p(br img script)) span))"
        );
        let script = &document.elements[0].children[0].children[0].children[0].children[2];
        assert_eq!(&text[script.end - 9..script.end], "</SCRIPT>");
    }

    /// Attributes read double-quoted, single-quoted, unquoted and valueless forms, with names
    /// lowercased and values as raw spans.
    #[test]
    fn reads_attribute_forms() {
        let text = "<input CLASS=\"a b\" id='x y' data-v=1 disabled\nfor = z>";
        let document = Document::parse(text);
        let input = &document.elements[0];
        let attributes: Vec<(&str, Option<&str>)> = input
            .attributes
            .iter()
            .map(|attribute| {
                (
                    attribute.name.as_str(),
                    attribute.value.clone().map(|span| &text[span]),
                )
            })
            .collect();
        assert_eq!(
            attributes,
            [
                ("class", Some("a b")),
                ("id", Some("x y")),
                ("data-v", Some("1")),
                ("disabled", None),
                ("for", Some("z")),
            ]
        );
        assert_eq!(input.end, text.len());
    }

    /// Character references decode; unknown ones stay.
    #[test]
    fn decodes_character_references() {
        assert_eq!(
            decode("a&amp;b &#x41;&#66; &unknown; &"),
            "a&b AB &unknown; &"
        );
    }
}
