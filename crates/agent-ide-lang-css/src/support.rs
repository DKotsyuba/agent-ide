//! Style-sheet symbol support: presence detection, outlines from source, and the absent runner
//! and formatter.
//!
//! A rule is a symbol named by its selector text (comments dropped, whitespace collapsed), so
//! `styles.css#.card .btn` addresses `.card .btn { … }`; nested SCSS/LESS rules are its children
//! and at-rules (`@media …`) are containers. Declarations are not symbols.
//!
//! ponytail: a selector containing `/` (an escaped `\/`) cannot be addressed, since `/` separates
//! path segments; rename such rules in the outline if it matters.

use std::path::Path;

use async_lsp::lsp_types as lsp;

use agent_ide_core::lang::{
    InsertSite, InsertWhere, LangError, Language, LanguageProject, LanguageSupport, LineRange,
    Outline, ProjectCommands, Symbol, SymbolKind, SymbolPath, TestReport, TestSelection,
    TestTarget, brace::place, line_count, text::has_files_with,
};

use crate::{
    LANGUAGE,
    scan::{Block, Sheet, sheet_text},
};

/// Stateless style-sheet implementation of [`LanguageSupport`].
#[derive(Clone, Copy, Debug, Default)]
pub struct CssSupport;

impl LanguageSupport for CssSupport {
    /// Always the style-sheet [`LANGUAGE`].
    fn language(&self) -> Language {
        LANGUAGE
    }

    /// Present when a bounded walk (three levels, 64 directories, hidden and dependency or
    /// build directories skipped) finds a `.css`, `.scss`, `.sass` or `.less` file. Style sheets
    /// have no manifest, environment, commands or entry points.
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

    /// Style sheets have no language server; the outline always comes from the text.
    fn normalize(&self, file: &Path, source: &str, _symbols: Vec<lsp::DocumentSymbol>) -> Outline {
        self.outline_from_source(file, source)
            .expect("style sheets always outline from source")
    }

    /// Rules and at-rules as nested symbols (see the module docs).
    fn outline_from_source(&self, file: &Path, source: &str) -> Option<Outline> {
        let (text, line_comments) = sheet_text(file, source);
        let sheet = Sheet::parse(&text, line_comments);
        Some(Outline {
            file: file.to_path_buf(),
            language: LANGUAGE,
            line_count: line_count(source),
            symbols: symbols(&sheet, &sheet.blocks, file, &[]),
        })
    }

    /// Brace placement shared with the other brace-delimited languages; every block may hold
    /// members.
    fn insert_site(
        &self,
        source: &str,
        outline: &Outline,
        anchor: &SymbolPath,
        where_: InsertWhere,
    ) -> Result<InsertSite, LangError> {
        place(source, outline, anchor, where_, "  ", |_| true)
    }

    /// Style sheets are never tests.
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
            "style sheets have no test runner".to_owned(),
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

    /// Style sheets hold no tests, so `ide.test` on one answers "no tests".
    fn tests_only_in_test_files(&self) -> bool {
        true
    }
}

/// Symbols of `blocks` under the path `segments`.
fn symbols(sheet: &Sheet<'_>, blocks: &[Block], file: &Path, segments: &[String]) -> Vec<Symbol> {
    blocks
        .iter()
        .filter(|block| !block.prelude.is_empty())
        .map(|block| {
            let name = sheet.normalized(&block.prelude);
            let mut path = segments.to_vec();
            path.push(name.clone());
            let range = LineRange::new(
                sheet.position(block.prelude.start).0,
                sheet.position(block.close).0,
            );
            Symbol {
                path: SymbolPath::new(Some(file.to_path_buf()), path.clone()),
                kind: if name.starts_with('@') {
                    SymbolKind::Namespace
                } else {
                    SymbolKind::Other
                },
                range,
                body: range,
                signature: name.clone(),
                doc: None,
                children: symbols(sheet, &block.blocks, file, &path),
                name,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(depth-indented name, kind, range)` lines of an outline.
    fn tree(outline: &Outline) -> Vec<String> {
        fn walk(symbols: &[Symbol], depth: usize, out: &mut Vec<String>) {
            for symbol in symbols {
                out.push(format!(
                    "{}{} {} {}",
                    "  ".repeat(depth),
                    symbol.name,
                    symbol.kind.name(),
                    symbol.range
                ));
                walk(&symbol.children, depth + 1, out);
            }
        }
        let mut out = Vec::new();
        walk(&outline.symbols, 0, &mut out);
        out
    }

    /// Rules are named by their selector text, nest, and at-rules are containers; declarations
    /// are not symbols.
    #[test]
    fn outline_nests_rules_and_at_rules() {
        let source = "/* x */\n.card .btn,\n  .x /* y */ {\n  color: red;\n}\n\
                      @media (min-width: 10px) {\n  .btn { &-a { b: c } }\n}\n";
        let outline = CssSupport
            .outline_from_source(Path::new("s.scss"), source)
            .unwrap();
        assert_eq!(outline.line_count, 8);
        assert_eq!(
            tree(&outline),
            [
                ".card .btn, .x symbol 2–5",
                "@media (min-width: 10px) namespace 6–8",
                "  .btn symbol 7",
                "    &-a symbol 7",
            ]
        );
        let path = SymbolPath::parse("s.scss#@media (min-width: 10px)/.btn/&-a").unwrap();
        assert_eq!(outline.find(&path).unwrap().name, "&-a");
    }

    /// Plain CSS has no `//` comments; indented Sass outlines by indentation.
    #[test]
    fn dialects_follow_their_comment_and_block_rules() {
        let css = CssSupport
            .outline_from_source(Path::new("a.css"), "a[href='//x'] {}\n")
            .unwrap();
        assert_eq!(tree(&css), ["a[href='//x'] symbol 1"]);
        let sass = CssSupport
            .outline_from_source(
                Path::new("a.sass"),
                ".a\n  b: c\n  .d\n    e: f\n.g\n  h: i\n",
            )
            .unwrap();
        assert_eq!(
            tree(&sass),
            [".a symbol 1–4", "  .d symbol 3–4", ".g symbol 5–6"]
        );
    }

    /// Detection finds style sheets below the root but not in skipped directories.
    #[test]
    fn detects_style_sheets_with_a_bounded_walk() {
        let root =
            std::env::temp_dir().join(format!("agent-ide-css-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::write(root.join("node_modules/pkg/a.css"), "").unwrap();
        assert!(CssSupport.detect(&root).is_none());
        std::fs::create_dir_all(root.join("src/styles")).unwrap();
        std::fs::write(root.join("src/styles/a.less"), "").unwrap();
        let project = CssSupport.detect(&root).unwrap();
        assert_eq!(project.commands, ProjectCommands::default());
        assert!(project.manifests.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
