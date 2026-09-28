//! HTML symbol support: presence detection, outlines from source, and the absent runner and
//! formatter.
//!
//! The outline holds landmark elements (`header`, `nav`, `main`, `section`, `article`, `aside`,
//! `footer`, `form`, `table`), elements with an `id`, and `script`/`style` elements, nested by the
//! element tree (elements in between are skipped). An element with an id is named `tag#id`
//! (`index.html#main#content` addresses it); any other is named by its tag and first class
//! (`nav.site-nav`). The signature is the start tag on one line.

use std::path::Path;

use async_lsp::lsp_types as lsp;

use agent_ide_core::lang::{
    InsertSite, InsertWhere, LangError, Language, LanguageProject, LanguageSupport, LineRange,
    Outline, ProjectCommands, Symbol, SymbolKind, SymbolPath, TestReport, TestSelection,
    TestTarget, line_count, render::clip, text::has_files_with,
};

use crate::{
    LANGUAGE,
    scan::{Document, Element, decode},
};

/// Elements outlined for their role alone.
const OUTLINED: [&str; 11] = [
    "header", "nav", "main", "section", "article", "aside", "footer", "form", "table", "script",
    "style",
];
/// Longest signature, in characters.
const MAX_SIGNATURE_CHARS: usize = 120;

/// Stateless HTML implementation of [`LanguageSupport`].
#[derive(Clone, Copy, Debug, Default)]
pub struct HtmlSupport;

impl LanguageSupport for HtmlSupport {
    /// Always the HTML [`LANGUAGE`].
    fn language(&self) -> Language {
        LANGUAGE
    }

    /// Present when the bounded presence walk finds an `.html` or `.htm` file. Documents have no
    /// manifest, environment, commands or entry points.
    fn detect(&self, root: &Path) -> Option<LanguageProject> {
        has_files_with(root, LANGUAGE.descriptor().extensions).then(|| LanguageProject {
            language: LANGUAGE,
            manifests: Vec::new(),
            environment: Vec::new(),
            interpreter: None,
            commands: ProjectCommands::default(),
            entry_points: Vec::new(),
        })
    }

    /// HTML has no language server; the outline always comes from the text.
    fn normalize(&self, file: &Path, source: &str, _symbols: Vec<lsp::DocumentSymbol>) -> Outline {
        self.outline_from_source(file, source)
            .expect("documents always outline from source")
    }

    /// Outlined elements as nested symbols (see the module docs).
    fn outline_from_source(&self, file: &Path, source: &str) -> Option<Outline> {
        let document = Document::parse(source);
        Some(Outline {
            file: file.to_path_buf(),
            language: LANGUAGE,
            line_count: line_count(source),
            symbols: symbols(&document, &document.elements, file, &[]),
        })
    }

    /// No insertion rules for markup.
    fn insert_site(
        &self,
        _source: &str,
        _outline: &Outline,
        _anchor: &SymbolPath,
        _where_: InsertWhere,
    ) -> Result<InsertSite, LangError> {
        Err(LangError::Unsupported(
            "documents have no insertion rules".to_owned(),
        ))
    }

    /// Documents are never tests.
    fn is_test_file(&self, _file: &Path) -> bool {
        false
    }

    /// No test runner.
    fn test_selection(
        &self,
        _project: &LanguageProject,
        _target: &TestTarget,
    ) -> Result<TestSelection, LangError> {
        Err(LangError::Unsupported(
            "documents have no test runner".to_owned(),
        ))
    }

    /// No runner output to parse; never claims success.
    fn parse_test_output(&self, _stdout: &str, _stderr: &str) -> TestReport {
        TestReport {
            incomplete: true,
            ..TestReport::default()
        }
    }

    /// No formatter.
    fn format_command(&self, _project: &LanguageProject, _file: &Path) -> Option<Vec<String>> {
        None
    }

    /// No formatter.
    fn format_stdin_command(
        &self,
        _project: &LanguageProject,
        _file: &Path,
    ) -> Option<Vec<String>> {
        None
    }

    /// Documents hold no tests, so `ide.test` on one answers "no tests".
    fn tests_only_in_test_files(&self) -> bool {
        true
    }
}

/// Symbols of the outlined elements among `elements` and their descendants, under `segments`.
fn symbols(
    document: &Document<'_>,
    elements: &[Element],
    file: &Path,
    segments: &[String],
) -> Vec<Symbol> {
    let mut found = Vec::new();
    for element in elements {
        let text = document.text;
        let attribute = |name| {
            element
                .attribute(name)
                .map(|value| decode(text[value.clone()].trim()))
                .filter(|value| !value.is_empty())
        };
        let id = attribute("id");
        if id.is_none() && !OUTLINED.contains(&element.tag.as_str()) {
            found.extend(symbols(document, &element.children, file, segments));
            continue;
        }
        let name = match id {
            Some(id) => format!("{}#{id}", element.tag),
            None => match attribute("class")
                .as_deref()
                .and_then(|classes| classes.split_ascii_whitespace().next())
            {
                Some(class) => format!("{}.{class}", element.tag),
                None => element.tag.clone(),
            },
        };
        let mut path = segments.to_vec();
        path.push(name.clone());
        let last_byte = element.end.saturating_sub(1).max(element.start_tag.start);
        let range = LineRange::new(
            document.position(element.start_tag.start).0,
            document.position(last_byte).0,
        );
        let start_tag = text[element.start_tag.clone()]
            .split_ascii_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        found.push(Symbol {
            path: SymbolPath::new(Some(file.to_path_buf()), path.clone()),
            kind: SymbolKind::Other,
            range,
            body: range,
            signature: clip(&start_tag, MAX_SIGNATURE_CHARS),
            doc: None,
            children: symbols(document, &element.children, file, &path),
            name,
        });
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `depth-indented name range` lines of an outline.
    fn tree(outline: &Outline) -> Vec<String> {
        fn walk(symbols: &[Symbol], depth: usize, out: &mut Vec<String>) {
            for symbol in symbols {
                out.push(format!(
                    "{}{} {}",
                    "  ".repeat(depth),
                    symbol.name,
                    symbol.range
                ));
                walk(&symbol.children, depth + 1, out);
            }
        }
        let mut out = Vec::new();
        walk(&outline.symbols, 0, &mut out);
        out
    }

    /// Landmarks, id'd elements and script/style elements nest by the element tree; other
    /// elements are skipped but their outlined descendants surface.
    #[test]
    fn outline_nests_landmarks_and_ids() {
        let source = "<html><body>\n<nav class=\"site-nav x\"><ul><li id=\"home\">H</li></ul></nav>\n\
                      <main id=\"content\">\n  <div><section>\n    <p>t</p>\n  </section></div>\n</main>\n\
                      <script src=\"a.js\"></script>\n</body></html>\n";
        let outline = HtmlSupport
            .outline_from_source(Path::new("index.html"), source)
            .unwrap();
        assert_eq!(
            tree(&outline),
            [
                "nav.site-nav 2",
                "  li#home 2",
                "main#content 3–7",
                "  section 4–6",
                "script 8",
            ]
        );
        let main = outline
            .find(&SymbolPath::parse("index.html#main#content").unwrap())
            .unwrap();
        assert_eq!(main.signature, "<main id=\"content\">");
    }
}
