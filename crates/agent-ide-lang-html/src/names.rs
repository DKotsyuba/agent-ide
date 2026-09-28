//! Cross-language name facts of HTML documents.
//!
//! * `class="a b"` uses class `a` and class `b`.
//! * `id="x"` defines element id `x`; `href="#x"`, `for`, `list` and `form` use one id;
//!   `aria-labelledby` and `aria-describedby` use each id of their list.
//! * Values decode character references before they become names.
//! * A class or id-list value holding template placeholders (`{{ }}`, `{% %}`, `<%= %>`, `${ }`)
//!   makes its other tokens heuristic (`template`); a token touching a placeholder, and a
//!   single-id value holding one, yield no fact.
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
];

impl NameFacts for HtmlFacts {
    /// Classes (use) and element ids (define and use).
    fn coverage(&self) -> &'static [NamespaceCoverage] {
        COVERAGE
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
            document: &document,
            sink,
            full: false,
        };
        facts.elements(&document.elements);
        FileVerdict::Indexed
    }
}

/// One extraction pass.
struct Facts<'a, 's> {
    /// The parsed document.
    document: &'a Document<'s>,
    /// Where facts go.
    sink: &'a mut FactSink,
    /// The sink refused a fact; stop emitting.
    full: bool,
}

impl Facts<'_, '_> {
    /// Facts of `elements` and their descendants, in document order.
    fn elements(&mut self, elements: &[Element]) {
        for element in elements {
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
}
