//! Cross-language name facts of HTML documents.
//!
//! * `class="a b"` uses class `a` and class `b`.
//! * `id="x"` defines element id `x`; `href="#x"`, `for`, `list` and `form` use one id;
//!   `aria-labelledby` and `aria-describedby` use each id of their list.
//! * Values decode character references before they become names.
//! * A class or id-list value holding template placeholders (`{{ }}`, `{% %}`, `<%= %>`, `${ }`)
//!   makes its other tokens heuristic (`template`); a token touching a placeholder, and a
//!   single-id value holding one, yield no fact.
//! * `<script src>` and `<link href>` use the local file they spell (`file-ref/v1`): a relative
//!   URL, query and fragment dropped, percent escapes decoded, joined to the document's directory.
//!   Absolute and network URLs, templated values and every reference of a document with a
//!   `<base href>` name nothing.
//! * Minified documents (`*.min.*`, or an average line over 2 000 bytes) are skipped.

use std::{ops::Range, path::Path};

use agent_ide_core::lang::names::{
    Certainty, FactSink, FileVerdict, NameFact, NameFacts, NameKey, Namespace, NamespaceCoverage,
    Role, ns,
};

use crate::scan::{Document, Element, decode};

/// Average line length past which a document counts as minified.
const MINIFIED_LINE_BYTES: usize = 2_000;
/// Template placeholder delimiters: opener and closer.
const PLACEHOLDERS: [(&str, &str); 4] = [("{{", "}}"), ("{%", "%}"), ("<%", "%>"), ("${", "}")];

/// The HTML [`NameFacts`] provider.
#[derive(Clone, Copy, Debug, Default)]
pub struct HtmlFacts;

/// What HTML defines and uses.
const COVERAGE: &[NamespaceCoverage] = &[
    NamespaceCoverage {
        namespace: ns::CLASS,
        defines: false,
        uses: true,
    },
    NamespaceCoverage {
        namespace: ns::ELEMENT_ID,
        defines: true,
        uses: true,
    },
    NamespaceCoverage {
        namespace: ns::FILE_REF,
        defines: false,
        uses: true,
    },
];

impl NameFacts for HtmlFacts {
    /// Classes (use), element ids (define and use) and file references (use).
    fn coverage(&self) -> &'static [NamespaceCoverage] {
        COVERAGE
    }

    /// `2`: `<script src>` and `<link href>` file references.
    fn revision(&self) -> &'static str {
        "2"
    }

    /// Facts of one document (see the module docs).
    fn extract(&self, file: &Path, source: &str, sink: &mut FactSink) -> FileVerdict {
        let file_name = file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let lines = source.lines().count().max(1);
        if file_name.rsplit('.').nth(1) == Some("min") || source.len() / lines > MINIFIED_LINE_BYTES
        {
            return FileVerdict::Skipped("minified");
        }
        let document = Document::parse(source);
        let mut facts = Facts {
            file,
            document: &document,
            sink,
            full: false,
            references: !has_base(&document.elements),
        };
        facts.elements(&document.elements);
        FileVerdict::Indexed
    }
}

/// One extraction pass.
struct Facts<'a, 's> {
    /// The worktree-relative document path.
    file: &'a Path,
    /// The parsed document.
    document: &'a Document<'s>,
    /// Where facts go.
    sink: &'a mut FactSink,
    /// The sink refused a fact; stop emitting.
    full: bool,
    /// File references may be read (no `<base href>` rebases the document's URLs).
    references: bool,
}

impl Facts<'_, '_> {
    /// Facts of `elements` and their descendants, in document order.
    fn elements(&mut self, elements: &[Element]) {
        for element in elements {
            match element.tag.as_str() {
                "script" if self.references => self.file_ref(element.attribute("src")),
                "link" if self.references => self.file_ref(element.attribute("href")),
                _ => {}
            }
            for attribute in &element.attributes {
                let Some(value) = &attribute.value else {
                    continue;
                };
                match attribute.name.as_str() {
                    "class" => self.list(ns::CLASS, value),
                    "aria-labelledby" | "aria-describedby" => self.list(ns::ELEMENT_ID, value),
                    "id" => self.single(value, 0, Role::Define),
                    "for" | "list" | "form" => self.single(value, 0, Role::Use),
                    "href" => {
                        let raw = &self.document.text[value.clone()];
                        if raw.trim_start().starts_with('#') {
                            let skip = raw.len() - raw.trim_start().len() + 1;
                            self.single(value, skip, Role::Use);
                        }
                    }
                    _ => {}
                }
            }
            self.elements(&element.children);
        }
    }

    /// Uses of each whitespace-separated token of `value` in `namespace`.
    fn list(&mut self, namespace: Namespace, value: &Range<usize>) {
        let raw = &self.document.text[value.clone()];
        let placeholders = placeholders(raw);
        let certainty = if placeholders.is_empty() {
            Certainty::Exact
        } else {
            Certainty::Heuristic("template")
        };
        for (token, templated) in tokens(raw, &placeholders) {
            if !templated {
                let name = decode(&raw[token.clone()]);
                self.emit(
                    namespace,
                    &name,
                    value.start + token.start,
                    Role::Use,
                    certainty,
                );
            }
        }
    }

    /// One element-id fact for the whole trimmed value after its first `skip` bytes, unless it
    /// holds a placeholder.
    fn single(&mut self, value: &Range<usize>, skip: usize, role: Role) {
        let raw = &self.document.text[value.start + skip..value.end];
        let name = raw.trim();
        if name.is_empty() || !placeholders(name).is_empty() {
            return;
        }
        let at = value.start + skip + (raw.len() - raw.trim_start().len());
        let at = if skip > 0 { at - 1 } else { at };
        self.emit(ns::ELEMENT_ID, &decode(name), at, role, Certainty::Exact);
    }

    /// A use of the local file a `src`/`href` value spells, positioned at the value.
    fn file_ref(&mut self, value: Option<&Range<usize>>) {
        let Some(value) = value else { return };
        let raw = &self.document.text[value.clone()];
        let trimmed = raw.trim();
        if trimmed.is_empty() || !placeholders(trimmed).is_empty() {
            return;
        }
        let decoded = decode(trimmed);
        let url = decoded.split(['?', '#']).next().unwrap_or("");
        let Some(path) = percent_decoded(url)
            .and_then(|path| agent_ide_core::lang::names::relative_reference(self.file, &path))
        else {
            return;
        };
        let at = value.start + (raw.len() - raw.trim_start().len());
        self.emit(ns::FILE_REF, &path, at, Role::Use, Certainty::Exact);
    }

    /// Pushes one global fact positioned at byte `at`, until the sink is full.
    fn emit(
        &mut self,
        namespace: Namespace,
        name: &str,
        at: usize,
        role: Role,
        certainty: Certainty,
    ) {
        if self.full {
            return;
        }
        let (line, column) = self.document.position(at);
        self.full = !self.sink.push(NameFact {
            key: NameKey::global(namespace, name),
            role,
            line,
            column,
            certainty,
        });
    }
}

/// Whether any element is a `<base href>`, which rebases every relative URL of the document.
fn has_base(elements: &[Element]) -> bool {
    elements.iter().any(|element| {
        (element.tag == "base" && element.attribute("href").is_some())
            || has_base(&element.children)
    })
}

/// `url` with its percent escapes decoded; `None` for a malformed or non-UTF-8 escape sequence
/// and for an escaped path separator (`%2F`, `%5C`), which does not separate path segments.
fn percent_decoded(url: &str) -> Option<String> {
    let bytes = url.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = url.get(at + 1..at + 3)?;
            let byte = u8::from_str_radix(hex, 16).ok()?;
            if matches!(byte, b'/' | b'\\') {
                return None;
            }
            out.push(byte);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Byte spans of template placeholders in `raw`; an unclosed one runs to the end.
fn placeholders(raw: &str) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    let mut at = 0;
    while at < raw.len() {
        let rest = &raw[at..];
        match PLACEHOLDERS.iter().find(|(open, _)| rest.starts_with(open)) {
            Some((open, close)) => {
                let end = rest[open.len()..]
                    .find(close)
                    .map_or(raw.len(), |found| at + open.len() + found + close.len());
                found.push(at..end);
                at = end;
            }
            None => at += rest.chars().next().map_or(1, char::len_utf8),
        }
    }
    found
}

/// Whitespace-separated tokens of `raw` (placeholders never split one), each with whether it
/// touches a placeholder.
fn tokens(raw: &str, placeholders: &[Range<usize>]) -> Vec<(Range<usize>, bool)> {
    let inside = |at: usize| placeholders.iter().any(|span| span.contains(&at));
    let mut found: Vec<(Range<usize>, bool)> = Vec::new();
    let mut current: Option<(usize, bool)> = None;
    for (at, ch) in raw.char_indices() {
        let templated = inside(at);
        if ch.is_ascii_whitespace() && !templated {
            if let Some((start, touched)) = current.take() {
                found.push((start..at, touched));
            }
        } else {
            let entry = current.get_or_insert((at, false));
            entry.1 |= templated;
        }
    }
    if let Some((start, touched)) = current {
        found.push((start..raw.len(), touched));
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(namespace id, name, role, line, column, heuristic reason)` of each fact.
    type Row = (&'static str, String, Role, u32, u32, Option<&'static str>);

    /// Extracts `source` as `file` and returns the verdict and compact rows.
    fn facts(file: &str, source: &str) -> (FileVerdict, Vec<Row>) {
        let mut sink = FactSink::new();
        let verdict = HtmlFacts.extract(Path::new(file), source, &mut sink);
        let rows = sink
            .facts()
            .iter()
            .map(|fact| {
                (
                    fact.key.namespace.id(),
                    fact.key.name.to_string(),
                    fact.role,
                    fact.line,
                    fact.column,
                    match fact.certainty {
                        Certainty::Exact => None,
                        Certainty::Heuristic(reason) => Some(reason),
                    },
                )
            })
            .collect();
        (verdict, rows)
    }

    /// An exact row.
    fn row(namespace: &'static str, name: &str, role: Role, line: u32, column: u32) -> Row {
        (namespace, name.into(), role, line, column, None)
    }

    /// Class tokens are uses, ids definitions, fragment links and label/list/form/aria
    /// references uses; entities decode; unquoted values and case-insensitive names work.
    #[test]
    fn attributes_become_facts() {
        let (verdict, rows) = facts(
            "index.html",
            "<main ID=\"content\" class=\"btn  btn&#45;primary\">\n\
             <a href=\"#content\">x</a><a href=\"page.html#no\"></a>\n\
             <label for=name>n</label><input list=\"l\" form=f aria-labelledby=\"a b\">\n</main>",
        );
        assert_eq!(verdict, FileVerdict::Indexed);
        assert_eq!(
            rows,
            [
                row("id/v1", "content", Role::Define, 1, 11),
                row("class/v1", "btn", Role::Use, 1, 27),
                row("class/v1", "btn-primary", Role::Use, 1, 32),
                row("id/v1", "content", Role::Use, 2, 10),
                row("id/v1", "name", Role::Use, 3, 12),
                row("id/v1", "l", Role::Use, 3, 39),
                row("id/v1", "f", Role::Use, 3, 47),
                row("id/v1", "a", Role::Use, 3, 66),
                row("id/v1", "b", Role::Use, 3, 68),
            ]
        );
    }

    /// Template placeholders make the other class tokens heuristic and drop the tokens they
    /// touch; an id holding one yields nothing.
    #[test]
    fn templates_make_class_tokens_heuristic() {
        let (_, rows) = facts(
            "t.html",
            "<div class=\"card {{ active }} btn-{{ size }} <%= x %> big\" id=\"{{ id }}\">",
        );
        let template = |name: &str, column| {
            (
                "class/v1",
                name.to_owned(),
                Role::Use,
                1,
                column,
                Some("template"),
            )
        };
        assert_eq!(rows, [template("card", 13), template("big", 55)]);
    }

    /// Comments and script/style bodies hold no facts; columns count bytes after multibyte
    /// characters.
    #[test]
    fn comments_scripts_and_unicode_positions() {
        let (_, rows) = facts(
            "u.html",
            "<!-- <p class=\"ghost\"> -->\n<script>x = '<p id=\"no\">'</script>\
             <style>.x { }</style>\n<p title=\"é\" class=\"é-x\">",
        );
        assert_eq!(rows, [row("class/v1", "é-x", Role::Use, 3, 22)]);
    }

    /// `*.min.*` documents and documents with very long average lines are skipped.
    #[test]
    fn minified_documents_are_skipped() {
        assert_eq!(
            facts("a.min.html", "<p>").0,
            FileVerdict::Skipped("minified")
        );
        assert_eq!(
            facts("a.html", &"<p class=a>".repeat(300)).0,
            FileVerdict::Skipped("minified")
        );
    }

    /// `<script src>` and `<link href>` use the local file they spell: query and fragment drop,
    /// escapes decode and the path joins the document's directory; absolute, network, data,
    /// templated, escaping and escaped-separator URLs name nothing, and neither do anchors.
    #[test]
    fn local_scripts_and_styles_are_file_references() {
        let (_, rows) = facts(
            "web/pages/index.html",
            "<link rel=stylesheet href=\"../css/site.css?v=2#top\">\n\
             <script src='./app%20main.js'></script>\n\
             <script src=\"https://cdn.example/x.js\"></script><script src=\"/root.js\"></script>\n\
             <script src=\"data:text/javascript,1\"></script><script src=\"{{ asset }}\"></script>\n\
             <script src=\"../../../outside.js\"></script><script src=\"a%2Fb.js\"></script>\n\
             <link rel=icon href=\"#frag\"><script>var src;</script><script src=\"\"></script>\n",
        );
        assert_eq!(
            rows,
            [
                row("file-ref/v1", "web/css/site.css", Role::Use, 1, 28),
                row("file-ref/v1", "web/pages/app main.js", Role::Use, 2, 14),
                row("id/v1", "frag", Role::Use, 6, 22),
            ]
        );
    }

    /// A `<base href>` rebases every relative URL, so no file reference is named.
    #[test]
    fn a_base_element_voids_file_references() {
        let (_, rows) = facts(
            "index.html",
            "<head><base href=\"/sub/\"><script src=\"a.js\"></script></head>",
        );
        assert!(rows.is_empty());
    }
}
