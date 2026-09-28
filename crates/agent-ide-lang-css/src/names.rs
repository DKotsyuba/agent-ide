//! Cross-language name facts of style sheets.
//!
//! * Every `.x` in a rule's selector list defines class `x` (`.card .btn` defines both), `#x`
//!   uses element id `x`; attribute selectors (`[href="#x"]`) and at-rule preludes are not read.
//! * SCSS/LESS `&-x` / `&__x` define the parent's class plus the suffix: exact when the parent
//!   selector is one class, heuristic (`nested suffix`) for each class ending a parent selector
//!   otherwise.
//! * `--x: …` defines style variable `x`; `var(--x)` uses it. `@extend .x` uses class `x`.
//! * A name touching interpolation (`#{…}`, `@{…}`) yields no fact; the rest of the file still
//!   does.
//! * Classes of `*.module.*` files get the file's worktree-relative path as their domain, so they
//!   never join global class names.
//! * Minified files (`*.min.*`, or an average line over 2 000 bytes) are skipped.

use std::{ops::Range, path::Path};

use agent_ide_core::lang::names::{
    Certainty, FactSink, FileVerdict, NameFact, NameFacts, NameKey, Namespace, NamespaceCoverage,
    Role, ns,
};

use crate::scan::{Block, Sheet, is_name_byte, sheet_text};

/// Average line length past which a file counts as minified.
const MINIFIED_LINE_BYTES: usize = 2_000;

/// The style-sheet [`NameFacts`] provider.
#[derive(Clone, Copy, Debug, Default)]
pub struct CssFacts;

/// What style sheets define and use.
const COVERAGE: &[NamespaceCoverage] = &[
    NamespaceCoverage {
        namespace: ns::CLASS,
        defines: true,
        uses: true,
    },
    NamespaceCoverage {
        namespace: ns::ELEMENT_ID,
        defines: false,
        uses: true,
    },
    NamespaceCoverage {
        namespace: ns::STYLE_VARIABLE,
        defines: true,
        uses: true,
    },
];

impl NameFacts for CssFacts {
    /// Classes (define, and use through `@extend`), element ids (use) and style variables.
    fn coverage(&self) -> &'static [NamespaceCoverage] {
        COVERAGE
    }

    /// Facts of one style sheet (see the module docs).
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
        let (text, line_comments) = sheet_text(file, source);
        let sheet = Sheet::parse(&text, line_comments);
        let module = file_name.rsplit('.').nth(1) == Some("module");
        let mut facts = Facts {
            sheet: &sheet,
            sink,
            class_domain: if module {
                file.display().to_string().into()
            } else {
                "".into()
            },
            full: false,
        };
        facts.items(&sheet.blocks, &sheet.statements, &Parent::default());
        FileVerdict::Indexed
    }
}

/// What a nested rule's `&` suffix resolves against.
#[derive(Default)]
struct Parent {
    /// The class, when the selector list is exactly one class.
    single: Option<String>,
    /// The class ending each selector of the list, where one does.
    last: Vec<String>,
}

/// One extraction pass.
struct Facts<'a, 's> {
    /// The parsed sheet.
    sheet: &'a Sheet<'s>,
    /// Where facts go.
    sink: &'a mut FactSink,
    /// Domain of class facts (the file path for module files, else empty).
    class_domain: Box<str>,
    /// The sink refused a fact; stop emitting.
    full: bool,
}

impl Facts<'_, '_> {
    /// Facts of statements and blocks at one level; at-rules pass `parent` through.
    fn items(&mut self, blocks: &[Block], statements: &[Range<usize>], parent: &Parent) {
        for statement in statements {
            self.statement(statement);
        }
        for block in blocks {
            if self.sheet.text[block.prelude.clone()].starts_with('@') {
                self.items(&block.blocks, &block.statements, parent);
            } else {
                let own = self.selectors(&block.prelude, parent);
                self.items(&block.blocks, &block.statements, &own);
            }
        }
    }

    /// Class defines and id uses of one selector list; returns what nested `&` resolves to.
    fn selectors(&mut self, span: &Range<usize>, parent: &Parent) -> Parent {
        let sheet = self.sheet;
        let bytes = sheet.text.as_bytes();
        let end = span.end;
        let mut own = Parent::default();
        // (name, start, end) of the last class token, and counts for the single-class test.
        let mut last: Option<(String, usize, usize)> = None;
        let (mut tokens, mut commas, mut depth) = (0, 0, 0usize);
        let mut selector_start = span.start;
        let mut at = span.start;
        let finish = |own: &mut Parent, last: &Option<(String, usize, usize)>, from, to: usize| {
            let mut to = to;
            while to > from && bytes[to - 1].is_ascii_whitespace() {
                to -= 1;
            }
            if let Some((name, _, token_end)) = last
                && *token_end == to
            {
                own.last.push(name.clone());
            }
        };
        while at < end {
            if let Some(next) = sheet.skip(at) {
                at = next.min(end);
                continue;
            }
            match bytes[at] {
                b'[' => {
                    // Attribute selectors name no classes or ids.
                    while at < end && bytes[at] != b']' {
                        at = sheet.skip(at).unwrap_or(at + 1);
                    }
                }
                b'(' => depth += 1,
                b')' => depth = depth.saturating_sub(1),
                b',' if depth == 0 => {
                    finish(&mut own, &last, selector_start, at);
                    last = None;
                    commas += 1;
                    selector_start = at + 1;
                }
                b'.' | b'#' if sheet.starts_name(at + 1, end) => {
                    let (name, after) = sheet.name(at + 1, end);
                    if !self.touches_interpolation(after, end) {
                        tokens += 1;
                        if bytes[at] == b'.' {
                            self.class(&name, at, Role::Define, Certainty::Exact);
                            last = Some((name, at, after));
                        } else {
                            self.emit(ns::ELEMENT_ID, "", &name, at, Role::Use, Certainty::Exact);
                            last = None;
                        }
                    }
                    at = after;
                    continue;
                }
                b'&' if at + 1 < end && matches!(bytes[at + 1], b'-' | b'_') => {
                    let (suffix, after) = sheet.name(at + 1, end);
                    if !self.touches_interpolation(after, end) {
                        tokens += 1;
                        last = None;
                        if let Some(class) = &parent.single {
                            let name = format!("{class}{suffix}");
                            self.class(&name, at, Role::Define, Certainty::Exact);
                            last = Some((name, at, after));
                        } else {
                            for class in &parent.last {
                                let name = format!("{class}{suffix}");
                                let guess = Certainty::Heuristic("nested suffix");
                                self.class(&name, at, Role::Define, guess);
                            }
                        }
                    }
                    at = after;
                    continue;
                }
                _ => {}
            }
            at += 1;
        }
        finish(&mut own, &last, selector_start, end);
        if commas == 0
            && tokens == 1
            && let Some((name, start, token_end)) = last
            && start == span.start
            && token_end == end
        {
            own.single = Some(name);
        }
        own
    }

    /// Style-variable defines (`--x:`), uses (`var(--x)`) and `@extend .x` class uses of one
    /// statement.
    fn statement(&mut self, span: &Range<usize>) {
        let sheet = self.sheet;
        let bytes = sheet.text.as_bytes();
        let (start, end) = (span.start, span.end);
        let text = &sheet.text[span.clone()];
        if text.starts_with("--") {
            let (name, after) = sheet.name(start + 2, end);
            let colon = bytes[after..end]
                .iter()
                .find(|byte| !byte.is_ascii_whitespace());
            if !name.is_empty() && colon == Some(&b':') {
                let variable = ns::STYLE_VARIABLE;
                self.emit(variable, "", &name, start, Role::Define, Certainty::Exact);
            }
        }
        let extend = text.starts_with("@extend");
        let mut at = start;
        while at < end {
            if let Some(next) = sheet.skip(at) {
                at = next.min(end);
                continue;
            }
            if extend && bytes[at] == b'.' && sheet.starts_name(at + 1, end) {
                let (name, after) = sheet.name(at + 1, end);
                if !self.touches_interpolation(after, end) {
                    self.class(&name, at, Role::Use, Certainty::Exact);
                }
                at = after;
                continue;
            }
            if bytes[at..end].len() >= 4
                && bytes[at..at + 4].eq_ignore_ascii_case(b"var(")
                && !(at > start && is_name_byte(bytes[at - 1]))
            {
                let mut dashes = at + 4;
                while dashes < end && bytes[dashes].is_ascii_whitespace() {
                    dashes += 1;
                }
                if bytes[dashes..end].starts_with(b"--") {
                    let (name, after) = sheet.name(dashes + 2, end);
                    if !name.is_empty() && !self.touches_interpolation(after, end) {
                        let variable = ns::STYLE_VARIABLE;
                        self.emit(variable, "", &name, dashes, Role::Use, Certainty::Exact);
                    }
                    at = after.max(dashes + 2);
                    continue;
                }
            }
            at += 1;
        }
    }

    /// Whether interpolation directly follows a name ending at `after` (the name is incomplete).
    fn touches_interpolation(&self, after: usize, end: usize) -> bool {
        after < end && self.sheet.is_interpolation(after)
    }

    /// One class fact in the file's class domain.
    fn class(&mut self, name: &str, at: usize, role: Role, certainty: Certainty) {
        let domain = self.class_domain.clone();
        self.emit(ns::CLASS, &domain, name, at, role, certainty);
    }

    /// Pushes one fact positioned at byte `at`, until the sink is full.
    fn emit(
        &mut self,
        namespace: Namespace,
        domain: &str,
        name: &str,
        at: usize,
        role: Role,
        certainty: Certainty,
    ) {
        if self.full {
            return;
        }
        let (line, column) = self.sheet.position(at);
        self.full = !self.sink.push(NameFact {
            key: NameKey {
                namespace,
                domain: domain.into(),
                name: name.into(),
            },
            role,
            line,
            column,
            certainty,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(namespace id, domain, name, role, line, column, heuristic reason)` of each fact.
    type Row = (
        &'static str,
        String,
        String,
        Role,
        u32,
        u32,
        Option<&'static str>,
    );

    /// Extracts `source` as `file` and returns the verdict and compact rows.
    fn facts(file: &str, source: &str) -> (FileVerdict, Vec<Row>) {
        let mut sink = FactSink::new();
        let verdict = CssFacts.extract(Path::new(file), source, &mut sink);
        let rows = sink
            .facts()
            .iter()
            .map(|fact| {
                (
                    fact.key.namespace.id(),
                    fact.key.domain.to_string(),
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

    /// A compact row for assertions.
    fn row(namespace: &'static str, name: &str, role: Role, line: u32, column: u32) -> Row {
        (
            namespace,
            String::new(),
            name.into(),
            role,
            line,
            column,
            None,
        )
    }

    /// Compound selectors define every class; ids are uses; attribute selectors, comments,
    /// strings and at-rule preludes name nothing.
    #[test]
    fn selectors_define_classes_and_use_ids() {
        let (verdict, rows) = facts(
            "a.css",
            "/* .ghost */ .card .btn, #main > a.link:not(.off) {}\n\
             a[href=\"#top\"] { content: \".nope\" }\n@media (min-width: 1.5em) { .in {} }\n",
        );
        assert_eq!(verdict, FileVerdict::Indexed);
        assert_eq!(
            rows,
            [
                row("class/v1", "card", Role::Define, 1, 14),
                row("class/v1", "btn", Role::Define, 1, 20),
                row("id/v1", "main", Role::Use, 1, 26),
                row("class/v1", "link", Role::Define, 1, 35),
                row("class/v1", "off", Role::Define, 1, 45),
                row("class/v1", "in", Role::Define, 3, 29),
            ]
        );
    }

    /// Escapes decode, and columns count bytes after multibyte characters.
    #[test]
    fn escapes_and_unicode_positions() {
        let (_, rows) = facts("a.css", "/* é */ .sm\\:p-4, .w-1\\/2 {}\n.é-x {}\n");
        assert_eq!(
            rows,
            [
                row("class/v1", "sm:p-4", Role::Define, 1, 10),
                row("class/v1", "w-1/2", Role::Define, 1, 20),
                row("class/v1", "é-x", Role::Define, 2, 1),
            ]
        );
    }

    /// Custom properties define style variables, `var()` uses them (fallbacks included), and
    /// `@extend` uses classes.
    #[test]
    fn variables_and_extend() {
        let (_, rows) = facts(
            "a.scss",
            ":root { --brand: red; --gap:1px }\n.a { color: var(--brand, var( --gap)); }\n\
             .b { @extend .a; }\n",
        );
        assert_eq!(
            rows,
            [
                row("style-variable/v1", "brand", Role::Define, 1, 9),
                row("style-variable/v1", "gap", Role::Define, 1, 23),
                row("class/v1", "a", Role::Define, 2, 1),
                row("style-variable/v1", "brand", Role::Use, 2, 17),
                row("style-variable/v1", "gap", Role::Use, 2, 31),
                row("class/v1", "b", Role::Define, 3, 1),
                row("class/v1", "a", Role::Use, 3, 14),
            ]
        );
    }

    /// `&` suffixes resolve exactly against a single-class parent and heuristically otherwise;
    /// nested plain classes and `//` comments behave as in any rule.
    #[test]
    fn nested_suffixes_resolve_against_the_parent() {
        let (_, rows) = facts(
            "a.scss",
            ".btn {\n  // .ghost\n  &-primary { &__icon {} }\n  .x {}\n}\n.a, .b {\n  &--wide {}\n}\n",
        );
        let heuristic = |name: &str, column| {
            (
                "class/v1",
                String::new(),
                name.to_owned(),
                Role::Define,
                7,
                column,
                Some("nested suffix"),
            )
        };
        assert_eq!(
            rows,
            [
                row("class/v1", "btn", Role::Define, 1, 1),
                row("class/v1", "btn-primary", Role::Define, 3, 3),
                row("class/v1", "btn-primary__icon", Role::Define, 3, 15),
                row("class/v1", "x", Role::Define, 4, 3),
                row("class/v1", "a", Role::Define, 6, 1),
                row("class/v1", "b", Role::Define, 6, 5),
                heuristic("a--wide", 3),
                heuristic("b--wide", 3),
            ]
        );
    }

    /// A name touching interpolation yields no fact; its neighbours still do.
    #[test]
    fn interpolation_drops_only_the_touched_name() {
        let (verdict, rows) = facts(
            "a.scss",
            ".btn-#{$size}, .ok, .#{$x} {}\n.less-@{v} {}\n.p { &-#{$y} {} }\n",
        );
        assert_eq!(verdict, FileVerdict::Indexed);
        assert_eq!(
            rows,
            [
                row("class/v1", "ok", Role::Define, 1, 16),
                row("class/v1", "p", Role::Define, 3, 1),
            ]
        );
    }

    /// Module files scope their classes to the file; ids and variables stay global.
    #[test]
    fn module_classes_get_the_file_domain() {
        let (_, rows) = facts("src/Button.module.css", ".btn { --c: 1; } #id {}\n");
        assert_eq!(
            rows,
            [
                (
                    "class/v1",
                    "src/Button.module.css".to_owned(),
                    "btn".to_owned(),
                    Role::Define,
                    1,
                    1,
                    None
                ),
                row("style-variable/v1", "c", Role::Define, 1, 8),
                row("id/v1", "id", Role::Use, 1, 18),
            ]
        );
    }

    /// `*.min.*` files and files with very long average lines are skipped as minified.
    #[test]
    fn minified_files_are_skipped() {
        assert_eq!(
            facts("a.min.css", ".a{}").0,
            FileVerdict::Skipped("minified")
        );
        let long = ".a{}".repeat(1_000);
        assert_eq!(facts("a.css", &long).0, FileVerdict::Skipped("minified"));
        assert_eq!(facts("a.css", ".a{}\n").0, FileVerdict::Indexed);
    }

    /// Indented Sass yields the same facts as its SCSS equivalent, at the original positions.
    #[test]
    fn indented_sass() {
        let (_, rows) = facts("a.sass", ".btn\n  color: var(--c)\n  &-x\n    a: b\n");
        assert_eq!(
            rows,
            [
                row("class/v1", "btn", Role::Define, 1, 1),
                row("style-variable/v1", "c", Role::Use, 2, 14),
                row("class/v1", "btn-x", Role::Define, 3, 3),
            ]
        );
    }
}
