//! Cross-language name facts of TypeScript and JavaScript sources (JSX included).
//!
//! A lexical scan (no parser) turns the source into identifiers, punctuation, string literals and
//! template literals, skipping comments and regular-expression literals; matching on that token
//! stream finds:
//!
//! * `className=` / `class=` with a string literal (or `{"…"}`): each whitespace token uses a
//!   class, exactly;
//! * a template literal there: its static tokens that touch no `${…}`, `Heuristic("template
//!   literal")`;
//! * inside `className={…}`: string literals and object keys within a `clsx`, `classnames`,
//!   `classNames`, `cx`, `cn`, `twMerge` or `twJoin` call, `Heuristic("clsx call")`; any other
//!   string literal, `Heuristic("expression")`;
//! * `getElementById("x")` uses id `x`; `querySelector`/`querySelectorAll` with one simple
//!   selector (`"#x"`, `".x"`) uses that id or class, exactly.
//!
//! Regex-versus-division is decided from the previous token and a quoted string never spans a
//! line. In `.tsx`/`.jsx`/`.js` files JSX elements are tracked, so JSX text (`<p>Don't</p>`) is
//! skipped rather than read as code. Minified files (`*.min.*`, or an average line over 2 000
//! bytes) are skipped.

use std::{ops::Range, path::Path};

use agent_ide_core::lang::names::{
    Certainty, FactSink, FileVerdict, NameFact, NameFacts, NameKey, Namespace, NamespaceCoverage,
    Role, ns,
};

/// Average line length past which a file counts as minified.
const MINIFIED_LINE_BYTES: usize = 2_000;
/// Class-joining helpers whose string arguments name classes.
const CLASS_HELPERS: [&str; 7] = [
    "clsx",
    "classnames",
    "classNames",
    "cx",
    "cn",
    "twMerge",
    "twJoin",
];
/// Keywords after which `/` starts a regular expression.
const REGEX_KEYWORDS: [&str; 11] = [
    "return", "typeof", "case", "in", "of", "new", "delete", "void", "throw", "yield", "await",
];

/// The TypeScript [`NameFacts`] provider.
#[derive(Clone, Copy, Debug, Default)]
pub struct TsFacts;

/// What scripts use (they define no names).
const COVERAGE: &[NamespaceCoverage] = &[
    NamespaceCoverage {
        namespace: ns::CLASS,
        defines: false,
        uses: true,
    },
    NamespaceCoverage {
        namespace: ns::ELEMENT_ID,
        defines: false,
        uses: true,
    },
];

impl NameFacts for TsFacts {
    /// Class and element-id uses.
    fn coverage(&self) -> &'static [NamespaceCoverage] {
        COVERAGE
    }

    /// `2`: JSX text is skipped instead of lexed as code.
    fn revision(&self) -> &'static str {
        "2"
    }

    /// Facts of one script (see the module docs).
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
        let jsx = !matches!(
            file.extension().and_then(|extension| extension.to_str()),
            Some("ts" | "mts" | "cts")
        );
        let tokens = tokens(source, jsx);
        let mut facts = Facts {
            source,
            line_starts: std::iter::once(0)
                .chain(source.match_indices('\n').map(|(index, _)| index + 1))
                .collect(),
            sink,
            full: false,
        };
        facts.scan(&tokens);
        FileVerdict::Indexed
    }
}

/// One lexical token.
#[derive(Clone, Debug, PartialEq)]
enum Token {
    /// An identifier, keyword or number.
    Word(Range<usize>),
    /// One punctuation byte at its offset.
    Punct(u8, usize),
    /// A quoted string literal's content (without quotes).
    Str(Range<usize>),
    /// A template literal's static chunks; `true` when the chunk is followed by `${`.
    Template(Vec<(Range<usize>, bool)>),
}

/// JSX state of the lexer: open JSX trees and where their text resumes.
#[derive(Default)]
struct Jsx {
    /// Open elements of each JSX tree being lexed (a tree inside a `{…}` child pushes its own).
    trees: Vec<usize>,
    /// Brace depths at which a `{…}` child of JSX text returns to that text.
    children: Vec<usize>,
    /// Brace depth of the tag being lexed and whether it is a closing tag.
    tag: Option<(usize, bool)>,
    /// The lexer is in JSX text (not code): quotes and slashes there are plain text.
    text: bool,
}

/// Lexes `source` into tokens (see the module docs). With `jsx`, JSX elements are recognized so
/// that their text (`<p>Don't</p>`) is skipped rather than read as code.
fn tokens(source: &str, jsx: bool) -> Vec<Token> {
    let bytes = source.as_bytes();
    let mut found: Vec<Token> = Vec::new();
    // Open template literals, each with the brace depth of its current `${…}` expression.
    let mut templates: Vec<(usize, usize)> = Vec::new();
    let mut depth = 0usize;
    let mut markup = Jsx::default();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        if markup.text {
            match byte {
                b'{' => {
                    depth += 1;
                    markup.children.push(depth);
                    markup.text = false;
                    found.push(Token::Punct(byte, at));
                }
                b'<' => {
                    let closing = bytes.get(at + 1) == Some(&b'/');
                    markup.tag = Some((depth, closing));
                    markup.text = false;
                    found.push(Token::Punct(byte, at));
                }
                _ => {}
            }
            at += 1;
            continue;
        }
        match byte {
            b'<' if jsx
                && markup.tag.is_none()
                && regex_allowed(source, found.last())
                && bytes
                    .get(at + 1)
                    .is_some_and(|next| next.is_ascii_alphabetic() || *next == b'>') =>
            {
                // A JSX element starts a tree in code.
                markup.trees.push(0);
                markup.tag = Some((depth, false));
                found.push(Token::Punct(byte, at));
                at += 1;
            }
            b'>' if markup.tag.is_some_and(|(open, _)| open == depth) => {
                let (_, closing) = markup.tag.take().unwrap_or((depth, false));
                let self_closing = at > 0 && bytes[at - 1] == b'/';
                if let Some(open) = markup.trees.last_mut() {
                    if closing {
                        *open = open.saturating_sub(1);
                    } else if !self_closing {
                        *open += 1;
                    }
                    if *open == 0 {
                        markup.trees.pop();
                    } else {
                        markup.text = true;
                    }
                }
                found.push(Token::Punct(byte, at));
                at += 1;
            }
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                at = find(bytes, at, b"\n").unwrap_or(bytes.len());
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at = find(bytes, at + 2, b"*/").map_or(bytes.len(), |end| end + 2);
            }
            b'/' if markup.tag.is_none() && regex_allowed(source, found.last()) => {
                at = regex_end(bytes, at)
            }
            b'"' | b'\'' => {
                let mut end = at + 1;
                while end < bytes.len() && bytes[end] != byte && bytes[end] != b'\n' {
                    end += if bytes[end] == b'\\' { 2 } else { 1 };
                }
                let end = end.min(bytes.len());
                found.push(Token::Str(at + 1..end));
                at = if bytes.get(end) == Some(&byte) {
                    end + 1
                } else {
                    end
                };
            }
            b'`' => {
                found.push(Token::Template(Vec::new()));
                templates.push((found.len() - 1, depth));
                let (next, entered) = template_chunk(bytes, at + 1, &mut found, &mut templates);
                depth += usize::from(entered);
                at = next;
            }
            b'{' => {
                depth += 1;
                found.push(Token::Punct(byte, at));
                at += 1;
            }
            b'}' if templates.last().is_some_and(|(_, open)| *open + 1 == depth) => {
                depth -= 1;
                let (next, entered) = template_chunk(bytes, at + 1, &mut found, &mut templates);
                depth += usize::from(entered);
                at = next;
            }
            b'}' => {
                // The end of a `{…}` child returns to its JSX text.
                if markup.children.last() == Some(&depth) {
                    markup.children.pop();
                    markup.text = true;
                }
                depth = depth.saturating_sub(1);
                found.push(Token::Punct(byte, at));
                at += 1;
            }
            _ if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$') || byte >= 0x80 => {
                let start = at;
                while at < bytes.len()
                    && (bytes[at].is_ascii_alphanumeric()
                        || matches!(bytes[at], b'_' | b'$')
                        || bytes[at] >= 0x80)
                {
                    at += 1;
                }
                found.push(Token::Word(start..at));
            }
            _ if byte.is_ascii_whitespace() => at += 1,
            _ => {
                found.push(Token::Punct(byte, at));
                at += 1;
            }
        }
    }
    found
}

/// Scans one static chunk of the innermost open template from `at` and records it. Returns the
/// byte after the closing backtick (the template is closed) or after `${` with `true` (an
/// expression is entered; the caller counts its brace).
fn template_chunk(
    bytes: &[u8],
    mut at: usize,
    found: &mut [Token],
    templates: &mut Vec<(usize, usize)>,
) -> (usize, bool) {
    let start = at;
    let Some(&(index, _)) = templates.last() else {
        return (at, false);
    };
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'`' => {
                if let Token::Template(chunks) = &mut found[index] {
                    chunks.push((start..at, false));
                }
                templates.pop();
                return (at + 1, false);
            }
            b'$' if bytes.get(at + 1) == Some(&b'{') => {
                if let Token::Template(chunks) = &mut found[index] {
                    chunks.push((start..at, true));
                }
                return (at + 2, true);
            }
            _ => at += 1,
        }
    }
    let end = bytes.len().min(at);
    if let Token::Template(chunks) = &mut found[index] {
        chunks.push((start..end, false));
    }
    templates.pop();
    (bytes.len(), false)
}

/// Whether `/` after `previous` starts a regular expression rather than a division.
fn regex_allowed(source: &str, previous: Option<&Token>) -> bool {
    match previous {
        None => true,
        Some(Token::Word(range)) => REGEX_KEYWORDS.contains(&&source[range.clone()]),
        // `</` closes a JSX element; it is not a regular expression.
        Some(Token::Punct(byte, _)) => !matches!(byte, b')' | b']' | b'}' | b'<'),
        Some(Token::Str(_) | Token::Template(_)) => false,
    }
}

/// The byte after a regular-expression literal starting at `at` (its flags included); a regex
/// never spans a line.
fn regex_end(bytes: &[u8], mut at: usize) -> usize {
    at += 1;
    let mut class = false;
    while at < bytes.len() && bytes[at] != b'\n' {
        match bytes[at] {
            b'\\' => at += 1,
            b'[' => class = true,
            b']' => class = false,
            b'/' if !class => {
                at += 1;
                while at < bytes.len() && bytes[at].is_ascii_alphabetic() {
                    at += 1;
                }
                return at;
            }
            _ => {}
        }
        at += 1;
    }
    at
}

/// First index of `needle` at or after `from`.
fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|found| from + found)
}

/// Index of the token closing the bracket opened at `open` (`(`/`)`, `{`/`}`, `[`/`]`), or the
/// token count when unclosed.
fn closing(tokens: &[Token], open: usize) -> usize {
    let Token::Punct(opener, _) = tokens[open] else {
        return open;
    };
    let closer = match opener {
        b'(' => b')',
        b'[' => b']',
        _ => b'}',
    };
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        match token {
            Token::Punct(byte, _) if *byte == opener => depth += 1,
            Token::Punct(byte, _) if *byte == closer => {
                depth -= 1;
                if depth == 0 {
                    return index;
                }
            }
            _ => {}
        }
    }
    tokens.len()
}

/// One extraction pass.
struct Facts<'a> {
    /// The source.
    source: &'a str,
    /// Byte offset of every line start.
    line_starts: Vec<usize>,
    /// Where facts go.
    sink: &'a mut FactSink,
    /// The sink refused a fact; stop emitting.
    full: bool,
}

impl Facts<'_> {
    /// Facts of the whole token stream.
    fn scan(&mut self, tokens: &[Token]) {
        let word = |index: usize| match tokens.get(index) {
            Some(Token::Word(range)) => Some(&self.source[range.clone()]),
            _ => None,
        };
        let punct = |index: usize, byte: u8| matches!(tokens.get(index), Some(Token::Punct(found, _)) if *found == byte);
        let mut index = 0;
        while index < tokens.len() {
            match word(index) {
                Some("className" | "class") if punct(index + 1, b'=') => {
                    match tokens.get(index + 2) {
                        Some(Token::Str(range)) => {
                            self.classes(range.clone(), Certainty::Exact);
                        }
                        Some(Token::Template(chunks)) => self.template(chunks),
                        Some(Token::Punct(b'{', _)) => {
                            let end = closing(tokens, index + 2);
                            self.expression(&tokens[index + 3..end.min(tokens.len())]);
                            index = end;
                        }
                        _ => {}
                    }
                }
                Some(call @ ("getElementById" | "querySelector" | "querySelectorAll"))
                    if punct(index + 1, b'(') && punct(index + 3, b')') =>
                {
                    if let Some(Token::Str(range)) = tokens.get(index + 2) {
                        self.query(call, range.clone());
                    }
                }
                _ => {}
            }
            index += 1;
        }
    }

    /// Facts of a `className={…}` expression's tokens.
    fn expression(&mut self, tokens: &[Token]) {
        if let [Token::Str(range)] = tokens {
            return self.classes(range.clone(), Certainty::Exact);
        }
        let mut index = 0;
        while index < tokens.len() {
            let helper = matches!(&tokens[index], Token::Word(range)
                if CLASS_HELPERS.contains(&&self.source[range.clone()]))
                && matches!(tokens.get(index + 1), Some(Token::Punct(b'(', _)));
            if helper {
                let end = closing(tokens, index + 1).min(tokens.len());
                self.helper(&tokens[index + 2..end]);
                index = end + 1;
                continue;
            }
            match &tokens[index] {
                Token::Str(range) => {
                    self.classes(range.clone(), Certainty::Heuristic("expression"));
                }
                Token::Template(chunks) => self.template(chunks),
                _ => {}
            }
            index += 1;
        }
    }

    /// Facts of a class helper call's argument tokens: string literals and object keys.
    fn helper(&mut self, tokens: &[Token]) {
        let certainty = Certainty::Heuristic("clsx call");
        for (index, token) in tokens.iter().enumerate() {
            // An object key: `{ key:` or `, key:`.
            let key = index > 0
                && matches!(tokens[index - 1], Token::Punct(b'{' | b',', _))
                && matches!(tokens.get(index + 1), Some(Token::Punct(b':', _)));
            match token {
                Token::Str(range) => self.classes(range.clone(), certainty),
                Token::Word(range) if key => self.classes(range.clone(), certainty),
                Token::Template(chunks) => self.template(chunks),
                _ => {}
            }
        }
    }

    /// Class uses of a template literal's static tokens that touch no substitution.
    fn template(&mut self, chunks: &[(Range<usize>, bool)]) {
        let substituted = chunks.len() > 1;
        let certainty = if substituted {
            Certainty::Heuristic("template literal")
        } else {
            Certainty::Exact
        };
        for (position, (chunk, before_substitution)) in chunks.iter().enumerate() {
            let text = &self.source[chunk.clone()];
            let after_substitution = position > 0;
            for (start, token) in words(text) {
                let touches_start = after_substitution && start == 0;
                let touches_end = *before_substitution && start + token.len() == text.len();
                if !touches_start && !touches_end {
                    let at = chunk.start + start;
                    self.emit(ns::CLASS, token, at, certainty);
                }
            }
        }
    }

    /// Class uses of each whitespace token of a string literal's content.
    fn classes(&mut self, content: Range<usize>, certainty: Certainty) {
        let text = &self.source[content.clone()];
        if text.contains('\\') {
            return;
        }
        for (start, token) in words(text) {
            self.emit(ns::CLASS, token, content.start + start, certainty);
        }
    }

    /// The id or class a DOM lookup names.
    fn query(&mut self, call: &str, content: Range<usize>) {
        let text = &self.source[content.clone()];
        let simple = |name: &str| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        };
        let (namespace, name) = if call == "getElementById" {
            (ns::ELEMENT_ID, text)
        } else if let Some(name) = text.strip_prefix('#') {
            (ns::ELEMENT_ID, name)
        } else if let Some(name) = text.strip_prefix('.') {
            (ns::CLASS, name)
        } else {
            return;
        };
        if simple(name) {
            self.emit(namespace, name, content.start, Certainty::Exact);
        }
    }

    /// Pushes one global use positioned at byte `at`, until the sink is full.
    fn emit(&mut self, namespace: Namespace, name: &str, at: usize, certainty: Certainty) {
        if self.full {
            return;
        }
        let line = self.line_starts.partition_point(|start| *start <= at);
        let column = at - self.line_starts[line - 1] + 1;
        self.full = !self.sink.push(NameFact {
            key: NameKey::global(namespace, name),
            role: Role::Use,
            line: line as u32,
            column: column as u32,
            certainty,
        });
    }
}

/// Whitespace-separated words of `text` with their byte offsets.
fn words(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.split_ascii_whitespace()
        .map(move |word| (word.as_ptr() as usize - text.as_ptr() as usize, word))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(namespace id, name, line, column, heuristic reason)` of each fact.
    type Row = (&'static str, String, u32, u32, Option<&'static str>);

    /// Extracts `source` as `file` and returns the verdict and compact rows.
    fn facts(file: &str, source: &str) -> (FileVerdict, Vec<Row>) {
        let mut sink = FactSink::new();
        let verdict = TsFacts.extract(Path::new(file), source, &mut sink);
        let rows = sink
            .facts()
            .iter()
            .map(|fact| {
                assert_eq!(fact.role, Role::Use);
                (
                    fact.key.namespace.id(),
                    fact.key.name.to_string(),
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

    /// A class row.
    fn class(name: &str, line: u32, column: u32, reason: Option<&'static str>) -> Row {
        ("class/v1", name.into(), line, column, reason)
    }

    /// String literals and `{"…"}` are exact; templates keep only tokens away from `${}`.
    #[test]
    fn class_attributes_by_certainty() {
        let (verdict, rows) = facts(
            "a.tsx",
            "<a className=\"btn  big\" />\n<b className={'x'} class=\"y\" />\n\
             <c className={`card ${on ? \"on\" : \"\"} btn-${size} wide`} />\n",
        );
        assert_eq!(verdict, FileVerdict::Indexed);
        let template = Some("template literal");
        assert_eq!(
            rows,
            [
                class("btn", 1, 15, None),
                class("big", 1, 20, None),
                class("x", 2, 16, None),
                class("y", 2, 27, None),
                class("card", 3, 16, template),
                class("wide", 3, 51, template),
                class("on", 3, 29, Some("expression")),
            ]
        );
    }

    /// Helper calls mark their strings and object keys `clsx call`; other strings in the
    /// expression are `expression`.
    #[test]
    fn helper_calls_and_expressions() {
        let (_, rows) = facts(
            "m.jsx",
            "<m className={clsx(\"btn\", { \"btn-lg\": big, active: on }, cond && 'x')} />\n\
             <n className={on ? \"a\" : \"b\"} />\n",
        );
        let clsx = Some("clsx call");
        assert_eq!(
            rows,
            [
                class("btn", 1, 21, clsx),
                class("btn-lg", 1, 30, clsx),
                class("active", 1, 44, clsx),
                class("x", 1, 67, clsx),
                class("a", 2, 21, Some("expression")),
                class("b", 2, 27, Some("expression")),
            ]
        );
    }

    /// DOM lookups use ids and classes exactly; compound selectors name nothing.
    #[test]
    fn dom_queries() {
        let (_, rows) = facts(
            "d.ts",
            "document.getElementById(\"main\");\nel.querySelector('.btn');\n\
             document.querySelectorAll(\"#nav\");\ndocument.querySelector(\"div .x\");\n",
        );
        assert_eq!(
            rows,
            [
                ("id/v1", "main".into(), 1, 26, None),
                class("btn", 2, 19, None),
                ("id/v1", "nav".into(), 3, 28, None),
            ]
        );
    }

    /// Regex literals, divisions, JSX text apostrophes, nested templates and comments never
    /// derail the scan.
    #[test]
    fn corpus_survives_lexical_hazards() {
        let source = "const r = /className=\"no\"[/]'/g; // className=\"no\"\n\
                      const half = a / 2; const q = b / c / d;\n\
                      /* className=\"no\" */\n\
                      const t = <p>Don't click</p>;\n\
                      const u = `outer ${`inner ${x} deep`} tail`;\n\
                      if (/^x/.test(s)) { y = 1 }\n\
                      return <a className=\"yes\">it's</a>;\n\
                      const v = <span>Save</span><i className=\"icon\" />;\n\
                      const w = <p>Don't <b>stop</b> {n > 1 ? 'x' : \"y\"} <i className=\"later\" /></p>;\n\
                      const z = <><p>it's</p><u className=\"frag\" /></>;\n";
        let (_, rows) = facts("c.tsx", source);
        assert_eq!(
            rows,
            [
                class("yes", 7, 22, None),
                class("icon", 8, 42, None),
                class("later", 9, 66, None),
                class("frag", 10, 38, None),
            ]
        );
    }

    /// `*.min.*` scripts and scripts with very long average lines are skipped.
    #[test]
    fn minified_scripts_are_skipped() {
        assert_eq!(facts("a.min.js", "x").0, FileVerdict::Skipped("minified"));
        assert_eq!(
            facts("a.js", &"a=1;".repeat(1_000)).0,
            FileVerdict::Skipped("minified")
        );
    }
}
