//! Lexical outline of one Rust source file: the addresses, ranges, kinds, signatures and docs the
//! server path ([`crate::support::RustSupport::normalize`]) derives from rust-analyzer, computed
//! from the text alone so symbol-addressed tools answer while the analyzer is still loading.
//!
//! The scanner walks the code-only form of the file ([`crate::module_graph::blanked_code`]:
//! comments and string or char literal contents blanked, offsets and line numbers unchanged), so
//! braces, `;` and keywords inside strings or comments are never syntax. Items nest exactly as
//! rust-analyzer reports them: struct fields and enum variants are children, functions inside an
//! impl or trait are methods, nested items inside a function body are its children, and items
//! inside a `const _ = { … }` initializer are hoisted to the enclosing level (which is what the
//! server does with the underscore constant itself absent).
//!
//! Honest refusal over guessing: anything the scanner cannot reproduce with the server's exact
//! answer makes the whole outline `None` and the file keeps waiting for the server — unbalanced
//! braces, an unrecognized token at item position, a macro invocation at item position (its body
//! may expand to items the text does not show), and two same-named same-kind siblings where
//! either header carries a `cfg`/`cfg_attr` attribute (which branch is live is semantic).
//! `ponytail:` a `{` inside const-generic defaults (`fn f<const N: usize = { … }>`) is read as
//! the body opener; tokenize generics if a real file ever hits it.

use std::path::Path;

use agent_ide_core::lang::brace::{collapse_whitespace, line_at, signature, source_lines};
use agent_ide_core::lang::{LineRange, Outline, Symbol, SymbolKind, SymbolPath, line_count};

use crate::LANGUAGE;
use crate::module_graph::blanked_code;
use crate::support::{attr_path, doc_of, header_start, impl_segment, is_cfg_test, is_test_attr};

/// Item keywords the scanner understands; everything else at item position is a refusal.
const ITEM_KEYWORDS: [&str; 13] = [
    "fn",
    "struct",
    "enum",
    "trait",
    "impl",
    "mod",
    "const",
    "static",
    "type",
    "union",
    "macro_rules",
    "use",
    "extern",
];

/// How the tokens after an item's modifiers continue: a block body, or a `;`-terminated leaf.
enum Continuation {
    /// The offset of the `{` that opens the body (not consumed).
    Body(usize),
    /// A `;` ended the item; its line, already consumed.
    Semi(u32),
    /// The text ended first; the scan is already marked uncertain.
    Eof,
}

/// Builds the lexical outline of `source`, or `None` when it does not scan cleanly (see the
/// module docs). Pure function of the text; no filesystem, server or subprocess.
pub(crate) fn lexical_outline(file: &Path, source: &str) -> Option<Outline> {
    // Line starts in the same coordinate space as the scanner's cursor: character offsets,
    // so a multi-byte character earlier in the file shifts no line number.
    let mut line_starts = vec![0usize];
    for (index, character) in source.chars().enumerate() {
        if character == '\n' {
            line_starts.push(index + 1);
        }
    }
    let mut scanner = Scanner {
        lines: source_lines(source),
        text: blanked_code(&source.chars().collect::<Vec<_>>()),
        line_starts,
        at: 0,
        uncertain: false,
    };
    let root = SymbolPath::new(Some(file.to_path_buf()), Vec::new());
    let symbols = scanner.scan_file(&root);
    (!scanner.uncertain).then(|| Outline {
        file: file.to_path_buf(),
        language: LANGUAGE,
        line_count: line_count(source),
        symbols,
    })
}

/// One parsed item handed to [`Scanner::symbol`]: where it starts, how it is addressed, and
/// where it ends.
struct Parsed {
    /// Cursor offset of the item's first token (visibility or keyword).
    decl_at: usize,
    /// Path segment: the item's name, or an impl's server-style header.
    name: String,
    /// Kind before test/method refinement.
    kind: SymbolKind,
    /// Line the item ends on.
    end: u32,
    /// Nested symbols.
    children: Vec<Symbol>,
}

impl Parsed {
    /// A leaf item with no nested symbols.
    fn leaf(decl_at: usize, name: String, kind: SymbolKind, end: u32) -> Self {
        Self {
            decl_at,
            name,
            kind,
            end,
            children: Vec::new(),
        }
    }
}

/// One pass over the blanked text. `at` is the cursor; every method that consumes tokens
/// advances it, and anything unexpected sets [`Scanner::uncertain`].
struct Scanner<'a> {
    /// Real source lines, for headers, signatures and docs.
    lines: Vec<&'a str>,
    /// Code-only form of the source; offsets match the source exactly.
    text: Vec<char>,
    /// Offset of each line's first character (line 1 starts at 0).
    line_starts: Vec<usize>,
    /// Cursor.
    at: usize,
    /// Whether anything was seen that makes the outline a guess; the caller returns `None`.
    uncertain: bool,
}

impl<'a> Scanner<'a> {
    /// The 1-based line `at` sits on.
    fn line_of(&self, at: usize) -> u32 {
        self.line_starts.partition_point(|&start| start <= at) as u32
    }

    /// The text between `from` (inclusive) and `to` (exclusive), whitespace collapsed.
    fn word(&self, from: usize, to: usize) -> String {
        collapse_whitespace(
            &self.text[from.min(to)..to.min(self.text.len())]
                .iter()
                .collect::<String>(),
        )
    }

    fn peek(&self) -> Option<char> {
        self.text.get(self.at).copied()
    }

    fn char_at(&self, at: usize) -> Option<char> {
        self.text.get(at).copied()
    }

    /// First offset at or after `at` that is not whitespace.
    fn ws_end(&self, at: usize) -> usize {
        let mut at = at;
        while matches!(self.char_at(at), Some(' ' | '\n' | '\t' | '\r')) {
            at += 1;
        }
        at
    }

    fn skip_ws(&mut self) {
        self.at = self.ws_end(self.at);
    }

    /// Marks the scan uncertain.
    fn refuse(&mut self) {
        self.uncertain = true;
    }

    /// `(start, end)` of the identifier at `at` (a raw `r#ident` included), `None` otherwise.
    fn ident_span(&self, at: usize) -> Option<(usize, usize)> {
        let ident_char =
            |character: Option<char>| character.is_some_and(|c| c.is_alphanumeric() || c == '_');
        if !ident_char(self.char_at(at)) {
            return None;
        }
        let start = at;
        let mut end = at;
        if self.char_at(end) == Some('r')
            && self.char_at(end + 1) == Some('#')
            && ident_char(self.char_at(end + 2))
        {
            end += 2;
        }
        while ident_char(self.char_at(end)) {
            end += 1;
        }
        (end > start).then_some((start, end))
    }

    /// The identifier at the cursor, consumed, `None` when none starts there.
    fn read_ident(&mut self) -> Option<String> {
        let (start, end) = self.ident_span(self.at)?;
        let word = self.word(start, end);
        self.at = end;
        Some(word)
    }

    /// Offset just past the balanced tree whose opener is at `at`, `None` when it never closes.
    fn tree_end(&self, at: usize) -> Option<usize> {
        let close = match self.char_at(at) {
            Some('(') => ')',
            Some('[') => ']',
            Some('{') => '}',
            _ => return None,
        };
        let open = self.text[at];
        let mut depth = 0usize;
        let mut at = at;
        while at < self.text.len() {
            if self.text[at] == open {
                depth += 1;
            } else if self.text[at] == close {
                depth -= 1;
                if depth == 0 {
                    return Some(at + 1);
                }
            }
            at += 1;
        }
        None
    }

    /// Skips the balanced tree at the cursor and returns the offset of its closing delimiter; an
    /// unterminated tree marks the scan uncertain and consumes the rest of the text.
    fn skip_tree(&mut self) -> Option<usize> {
        let Some(end) = self.tree_end(self.at) else {
            self.refuse();
            self.at = self.text.len();
            return None;
        };
        let close = end - 1;
        self.at = end;
        Some(close)
    }

    /// Skips the outer (or, after `#!`, inner) attribute at the cursor.
    fn skip_attribute(&mut self) {
        if self.char_at(self.at + 1) == Some('!') && self.char_at(self.at + 2) == Some('[') {
            self.at += 2;
        } else if self.char_at(self.at + 1) == Some('[') {
            self.at += 1;
        } else {
            self.refuse();
            self.at += 1;
            return;
        }
        self.skip_tree();
    }

    /// The item keyword the tokens at `at` start, and the offset just past its modifiers, once
    /// visibility and `unsafe`/`async`/`extern` prefixes are stepped over. `const` is a modifier
    /// only before another function modifier, `extern` before `fn`/`crate` (a blanked string
    /// literal in `extern "C" fn` is invisible). `None` when the tokens are not an item.
    fn item_keyword(&self, at: usize) -> Option<(&'static str, usize)> {
        let mut at = at;
        let mut after_extern = false;
        loop {
            at = self.ws_end(at);
            let (start, end) = self.ident_span(at)?;
            let word = self.word(start, end);
            match word.as_str() {
                "pub" => {
                    at = end;
                    let paren = self.ws_end(at);
                    if self.char_at(paren) == Some('(') {
                        at = self.tree_end(paren)?;
                    }
                    after_extern = false;
                }
                "unsafe" | "async" | "default" | "auto" => {
                    at = end;
                    after_extern = false;
                }
                "extern" => {
                    at = end;
                    after_extern = true;
                }
                "const" => {
                    let after = self.ws_end(end);
                    if let Some((_, next_end)) = self.ident_span(after)
                        && matches!(
                            self.word(after, next_end).as_str(),
                            "fn" | "unsafe" | "async" | "extern"
                        )
                    {
                        at = end;
                        after_extern = false;
                    } else {
                        return Some(("const", end));
                    }
                }
                "crate" if after_extern => return Some(("extern_crate", end)),
                other => {
                    return ITEM_KEYWORDS
                        .iter()
                        .find(|keyword| **keyword == other)
                        .map(|keyword| (*keyword, end));
                }
            }
        }
    }

    /// Whether an identifier at the cursor is directly followed by `!` (a macro call).
    fn macro_call_ahead(&self) -> bool {
        match self.ident_span(self.at) {
            Some((_, end)) => self.char_at(self.ws_end(end)) == Some('!'),
            None => false,
        }
    }

    /// Skips a macro invocation at the cursor: identifier, `!`, and its token tree if any.
    fn skip_macro_call(&mut self) {
        if self.read_ident().is_none() {
            self.at += 1;
            return;
        }
        self.skip_ws();
        if self.peek() == Some('!') {
            self.at += 1;
            self.skip_ws();
            if matches!(self.peek(), Some('(' | '[' | '{')) {
                self.skip_tree();
            }
        }
    }

    /// Consumes one unrecognized token (an identifier or a single character), guaranteeing
    /// progress.
    fn skip_token(&mut self) {
        if self.ident_span(self.at).is_some() {
            self.read_ident();
        } else {
            self.at += 1;
        }
    }

    /// Scans from the cursor for the body opener or the `;` that ends the item, skipping
    /// balanced `(...)`/`[...]` groups on the way (generics, arguments, array types, bounds).
    fn scan_to_body(&mut self) -> Continuation {
        loop {
            self.skip_ws();
            match self.peek() {
                Some('(' | '[') => {
                    self.skip_tree();
                }
                Some('{') => return Continuation::Body(self.at),
                Some(';') => {
                    let line = self.line_of(self.at);
                    self.at += 1;
                    return Continuation::Semi(line);
                }
                Some(_) => self.at += 1,
                None => {
                    self.refuse();
                    return Continuation::Eof;
                }
            }
        }
    }

    /// Scans to the `;` that ends a leaf item (`use`, `const`, `static`, `type`), skipping
    /// balanced groups so initializer blocks and use trees never end it early; returns the `;`
    /// line.
    fn scan_to_semi(&mut self) -> u32 {
        loop {
            match self.peek() {
                Some('(' | '[' | '{') => {
                    self.skip_tree();
                }
                Some(';') => {
                    let line = self.line_of(self.at);
                    self.at += 1;
                    return line;
                }
                Some(_) => self.at += 1,
                None => {
                    self.refuse();
                    return self.line_of(self.text.len().saturating_sub(1));
                }
            }
        }
    }

    /// Scans the file's top level in item mode; a stray `}` marks the scan uncertain.
    fn scan_file(&mut self, root: &SymbolPath) -> Vec<Symbol> {
        let mut symbols = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                None => break,
                Some('}') => {
                    self.refuse();
                    self.at += 1;
                }
                Some('#') => self.skip_attribute(),
                Some(_) => {
                    if self.item_keyword(self.at).is_some() {
                        symbols.extend(self.parse_item(root, None));
                    } else if self.macro_call_ahead() {
                        // A macro invocation at item position may expand to items the text does
                        // not show; the file keeps waiting for the server instead of guessing.
                        self.refuse();
                        self.skip_macro_call();
                    } else {
                        self.refuse();
                        self.skip_token();
                    }
                }
            }
        }
        self.uncertain |= cfg_duplicate(&symbols, &self.lines);
        symbols
    }

    /// Scans one container body from its opening `{` (consumed) to the matching `}` (consumed),
    /// returning its symbols and the closing line. `statements` selects function-body mode,
    /// where arbitrary statement tokens are skipped silently; otherwise every position must be
    /// an item, and anything else marks the scan uncertain.
    fn scan_items(
        &mut self,
        owner: &SymbolPath,
        owner_kind: Option<SymbolKind>,
        statements: bool,
    ) -> (Vec<Symbol>, u32) {
        self.at += 1; // the opening `{`
        let mut symbols = Vec::new();
        let mut close = self.line_of(self.at.max(1));
        let mut statement_start = true;
        loop {
            self.skip_ws();
            match self.peek() {
                None => {
                    self.refuse();
                    break;
                }
                Some('}') => {
                    close = self.line_of(self.at);
                    self.at += 1;
                    break;
                }
                Some('#') => self.skip_attribute(),
                Some(_) => {
                    if !statements {
                        if self.item_keyword(self.at).is_some() {
                            symbols.extend(self.parse_item(owner, owner_kind));
                        } else if self.macro_call_ahead() {
                            self.refuse();
                            self.skip_macro_call();
                        } else {
                            self.refuse();
                            self.skip_token();
                        }
                    } else if statement_start && self.item_keyword(self.at).is_some() {
                        symbols.extend(self.parse_item(owner, owner_kind));
                        statement_start = true;
                    } else if statement_start && self.macro_call_ahead() {
                        self.skip_macro_call();
                        statement_start = true;
                    } else {
                        statement_start =
                            self.consume_statement_token(&mut symbols, owner, owner_kind);
                    }
                }
            }
        }
        self.uncertain |= cfg_duplicate(&symbols, &self.lines);
        (symbols, close)
    }

    /// Consumes one statement token, recursing into `{…}` blocks (their items join this
    /// function's children, as the server reports them) and treating `identifier !` as a macro
    /// call whose tree is skipped. Returns whether the next token starts a statement.
    fn consume_statement_token(
        &mut self,
        symbols: &mut Vec<Symbol>,
        owner: &SymbolPath,
        owner_kind: Option<SymbolKind>,
    ) -> bool {
        if self.ident_span(self.at).is_some() {
            self.read_ident();
            self.skip_ws();
            if self.peek() == Some('!') {
                self.at += 1;
                self.skip_ws();
                if matches!(self.peek(), Some('(' | '[' | '{')) {
                    self.skip_tree();
                }
                return true;
            }
            return false;
        }
        match self.peek() {
            Some(';') => {
                self.at += 1;
                true
            }
            Some('{') => {
                let (nested, _) = self.scan_items(owner, owner_kind, true);
                symbols.extend(nested);
                true
            }
            Some(_) => {
                self.at += 1;
                false
            }
            None => false,
        }
    }

    /// Parses the item at the cursor (whose modifiers and keyword [`Scanner::item_keyword`]
    /// accepts) as a child of `owner`; returns nothing for items that are not symbols (`use`,
    /// `extern crate`, extern blocks) and the hoisted items of a `const _ = { … }` initializer.
    fn parse_item(&mut self, owner: &SymbolPath, owner_kind: Option<SymbolKind>) -> Vec<Symbol> {
        let decl_at = self.at;
        let Some((keyword, after_modifiers)) = self.item_keyword(decl_at) else {
            return Vec::new();
        };
        self.at = after_modifiers;
        match keyword {
            "use" | "extern_crate" => {
                self.scan_to_semi();
                Vec::new()
            }
            "extern" => {
                // An `extern { … }` block: its items belong to no addressable parent here.
                self.refuse();
                self.skip_ws();
                if self.peek() == Some('{') {
                    self.skip_tree();
                }
                Vec::new()
            }
            "macro_rules" => {
                self.skip_ws();
                if self.peek() == Some('!') {
                    self.at += 1;
                    self.skip_ws();
                }
                let Some(mut name) = self.read_ident() else {
                    self.refuse();
                    return Vec::new();
                };
                if let Some(stripped) = name.strip_prefix("r#") {
                    name = stripped.to_owned();
                }
                let end = match self.scan_to_body() {
                    Continuation::Body(open) => {
                        let close = self.skip_tree().unwrap_or(open);
                        self.line_of(close)
                    }
                    Continuation::Semi(line) => line,
                    Continuation::Eof => return Vec::new(),
                };
                vec![self.symbol(
                    owner,
                    owner_kind,
                    Parsed {
                        decl_at,
                        name,
                        kind: SymbolKind::Function,
                        end,
                        children: Vec::new(),
                    },
                )]
            }
            "const" | "static" => {
                self.skip_ws();
                let Some(name) = self.read_ident() else {
                    self.refuse();
                    return Vec::new();
                };
                if keyword == "const" && name == "_" {
                    // The server reports no symbol for the underscore constant and hoists the
                    // items inside its initializer blocks to the enclosing level.
                    let mut hoisted = Vec::new();
                    loop {
                        self.skip_ws();
                        match self.peek() {
                            Some('(' | '[') => {
                                self.skip_tree();
                            }
                            Some('{') => {
                                let (nested, _) = self.scan_items(owner, owner_kind, true);
                                hoisted.extend(nested);
                            }
                            Some(';') => {
                                self.at += 1;
                                break;
                            }
                            Some(_) => self.at += 1,
                            None => {
                                self.refuse();
                                break;
                            }
                        }
                    }
                    return hoisted;
                }
                let end = self.scan_to_semi();
                vec![self.symbol(
                    owner,
                    owner_kind,
                    Parsed {
                        decl_at,
                        name,
                        kind: SymbolKind::Constant,
                        end,
                        children: Vec::new(),
                    },
                )]
            }
            "type" => {
                self.skip_ws();
                let Some(name) = self.read_ident() else {
                    self.refuse();
                    return Vec::new();
                };
                let end = self.scan_to_semi();
                vec![self.symbol(
                    owner,
                    owner_kind,
                    Parsed {
                        decl_at,
                        name,
                        kind: SymbolKind::TypeAlias,
                        end,
                        children: Vec::new(),
                    },
                )]
            }
            "mod" => {
                self.skip_ws();
                let Some(name) = self.read_ident() else {
                    self.refuse();
                    return Vec::new();
                };
                let (end, children) = match self.scan_to_body() {
                    Continuation::Body(_) => {
                        let path = owner.child(&name);
                        let (children, close) =
                            self.scan_items(&path, Some(SymbolKind::Module), false);
                        (close, children)
                    }
                    Continuation::Semi(line) => (line, Vec::new()),
                    Continuation::Eof => return Vec::new(),
                };
                vec![self.symbol(
                    owner,
                    owner_kind,
                    Parsed {
                        decl_at,
                        name,
                        kind: SymbolKind::Module,
                        end,
                        children,
                    },
                )]
            }
            "trait" => {
                self.skip_ws();
                let Some(name) = self.read_ident() else {
                    self.refuse();
                    return Vec::new();
                };
                let (end, children) = match self.scan_to_body() {
                    Continuation::Body(_) => {
                        let path = owner.child(&name);
                        let (children, close) =
                            self.scan_items(&path, Some(SymbolKind::Trait), false);
                        (close, children)
                    }
                    Continuation::Semi(line) => (line, Vec::new()),
                    Continuation::Eof => return Vec::new(),
                };
                vec![self.symbol(
                    owner,
                    owner_kind,
                    Parsed {
                        decl_at,
                        name,
                        kind: SymbolKind::Trait,
                        end,
                        children,
                    },
                )]
            }
            "impl" => {
                let (name, end, children) = match self.scan_to_body() {
                    Continuation::Body(open) => {
                        let name = impl_segment(&self.impl_header(decl_at, open));
                        let path = owner.child(&name);
                        let (children, close) =
                            self.scan_items(&path, Some(SymbolKind::Impl), false);
                        (name, close, children)
                    }
                    Continuation::Semi(line) => (String::new(), line, Vec::new()),
                    Continuation::Eof => return Vec::new(),
                };
                if name.is_empty() {
                    return Vec::new();
                }
                vec![self.symbol(
                    owner,
                    owner_kind,
                    Parsed {
                        decl_at,
                        name,
                        kind: SymbolKind::Impl,
                        end,
                        children,
                    },
                )]
            }
            "fn" => {
                self.skip_ws();
                let Some(mut name) = self.read_ident() else {
                    self.refuse();
                    return Vec::new();
                };
                if let Some(stripped) = name.strip_prefix("r#") {
                    name = stripped.to_owned();
                }
                let (end, children) = match self.scan_to_body() {
                    Continuation::Body(_) => {
                        let path = owner.child(&name);
                        let (children, close) =
                            self.scan_items(&path, Some(SymbolKind::Function), true);
                        (close, children)
                    }
                    Continuation::Semi(line) => (line, Vec::new()),
                    Continuation::Eof => return Vec::new(),
                };
                vec![self.symbol(
                    owner,
                    owner_kind,
                    Parsed {
                        decl_at,
                        name,
                        kind: SymbolKind::Function,
                        end,
                        children,
                    },
                )]
            }
            "struct" | "union" => {
                self.skip_ws();
                let Some(name) = self.read_ident() else {
                    self.refuse();
                    return Vec::new();
                };
                let (end, children) = match self.scan_to_body() {
                    Continuation::Body(_) => {
                        let path = owner.child(&name);
                        let (children, close) = self.scan_fields(&path);
                        (close, children)
                    }
                    Continuation::Semi(line) => (line, Vec::new()),
                    Continuation::Eof => return Vec::new(),
                };
                vec![self.symbol(
                    owner,
                    owner_kind,
                    Parsed {
                        decl_at,
                        name,
                        kind: SymbolKind::Struct,
                        end,
                        children,
                    },
                )]
            }
            "enum" => {
                self.skip_ws();
                let Some(name) = self.read_ident() else {
                    self.refuse();
                    return Vec::new();
                };
                let (end, children) = match self.scan_to_body() {
                    Continuation::Body(_) => {
                        let path = owner.child(&name);
                        let (children, close) = self.scan_variants(&path);
                        (close, children)
                    }
                    Continuation::Semi(line) => (line, Vec::new()),
                    Continuation::Eof => return Vec::new(),
                };
                vec![self.symbol(
                    owner,
                    owner_kind,
                    Parsed {
                        decl_at,
                        name,
                        kind: SymbolKind::Enum,
                        end,
                        children,
                    },
                )]
            }
            _ => Vec::new(),
        }
    }

    /// Scans the field list of a struct, union or record variant from its opening `{`
    /// (consumed) to the matching `}` (consumed); returns the fields and the closing line.
    fn scan_fields(&mut self, owner: &SymbolPath) -> (Vec<Symbol>, u32) {
        self.at += 1; // the opening `{`
        let mut fields = Vec::new();
        let mut close = self.line_of(self.at.max(1));
        loop {
            self.skip_ws();
            match self.peek() {
                None => {
                    self.refuse();
                    break;
                }
                Some('}') => {
                    close = self.line_of(self.at);
                    self.at += 1;
                    break;
                }
                Some('#') => self.skip_attribute(),
                Some(_) => {
                    let decl_at = self.at;
                    if self.word_equals("pub") {
                        self.at = self.ws_end(self.at + 3);
                        if self.peek() == Some('(') {
                            self.skip_tree();
                        }
                        self.skip_ws();
                    }
                    let Some(mut name) = self.read_ident() else {
                        self.refuse();
                        self.skip_token();
                        continue;
                    };
                    if let Some(stripped) = name.strip_prefix("r#") {
                        name = stripped.to_owned();
                    }
                    let Some(end) = self.scan_member_end() else {
                        continue;
                    };
                    fields.push(self.symbol(
                        owner,
                        None,
                        Parsed::leaf(decl_at, name, SymbolKind::Field, end),
                    ));
                }
            }
        }
        (fields, close)
    }

    /// Scans the variant list of an enum from its opening `{` (consumed) to the matching `}`
    /// (consumed); returns the variants (record variants carry their fields) and the closing line.
    fn scan_variants(&mut self, owner: &SymbolPath) -> (Vec<Symbol>, u32) {
        self.at += 1; // the opening `{`
        let mut variants = Vec::new();
        let mut close = self.line_of(self.at.max(1));
        loop {
            self.skip_ws();
            match self.peek() {
                None => {
                    self.refuse();
                    break;
                }
                Some('}') => {
                    close = self.line_of(self.at);
                    self.at += 1;
                    break;
                }
                Some('#') => self.skip_attribute(),
                Some(_) => {
                    let decl_at = self.at;
                    let Some(mut name) = self.read_ident() else {
                        self.refuse();
                        self.skip_token();
                        continue;
                    };
                    if let Some(stripped) = name.strip_prefix("r#") {
                        name = stripped.to_owned();
                    }
                    self.skip_ws();
                    let (end, children) = match self.peek() {
                        Some('(') => {
                            self.skip_tree();
                            (self.scan_member_end(), Vec::new())
                        }
                        Some('{') => {
                            let path = owner.child(&name);
                            let (children, variant_close) = self.scan_fields(&path);
                            // A record variant may carry a trailing `,` after its `}`.
                            self.skip_ws();
                            if self.peek() == Some(',') {
                                self.at += 1;
                            }
                            (Some(variant_close), children)
                        }
                        _ => (self.scan_member_end(), Vec::new()),
                    };
                    let Some(end) = end else {
                        continue;
                    };
                    variants.push(self.symbol(
                        owner,
                        None,
                        Parsed {
                            decl_at,
                            name,
                            kind: SymbolKind::Variant,
                            end,
                            children,
                        },
                    ));
                }
            }
        }
        (variants, close)
    }

    /// The terminator of one struct field or enum variant: the `,` that ends it (consumed), or
    /// the last code line before the `}` that closes the list (not consumed). `None` when the
    /// text ends first.
    fn scan_member_end(&mut self) -> Option<u32> {
        loop {
            self.skip_ws();
            match self.peek() {
                Some('(' | '[' | '{') => {
                    self.skip_tree();
                }
                Some(',') => {
                    let line = self.line_of(self.at);
                    self.at += 1;
                    return Some(line);
                }
                Some('}') => {
                    let mut at = self.at;
                    while at > 0 && matches!(self.char_at(at - 1), Some(' ' | '\n' | '\t' | '\r')) {
                        at -= 1;
                    }
                    return Some(self.line_of(at.saturating_sub(1)));
                }
                Some(_) => self.at += 1,
                None => {
                    self.refuse();
                    return None;
                }
            }
        }
    }

    /// Whether the identifier at the cursor equals `word`.
    fn word_equals(&self, word: &str) -> bool {
        match self.ident_span(self.at) {
            Some((start, end)) => self.word(start, end) == word,
            None => false,
        }
    }

    /// The impl's declaration as the server names it: leading `unsafe`/`default` modifiers,
    /// the generic parameter list right after `impl` and the where clause are dropped, so
    /// `impl<T: Clone> Wrap<T> for Guard where T: Debug` names `impl Wrap<T> for Guard`;
    /// [`impl_segment`] then collapses an inherent impl to its bare type name, the same rule
    /// the server path normalizes with.
    fn impl_header(&self, decl_at: usize, body_open: usize) -> String {
        let mut text = self.word(decl_at, body_open);
        while let Some(first) = text.split_whitespace().next()
            && matches!(first, "unsafe" | "default")
        {
            text = text[first.len()..].trim_start().to_owned();
        }
        if let Some(at) = text.find(" where ") {
            text.truncate(at);
        }
        match text.strip_prefix("impl") {
            Some(rest) if rest.trim_start().starts_with('<') => {
                let generics = rest.trim_start();
                let after = crate::support::skip_balanced(generics, '<', '>');
                format!("impl {after}")
            }
            _ => text,
        }
    }

    /// Builds one symbol: header recomputed from the real source exactly as the server path
    /// normalizes it, kind refined by the same rules (test attributes, methods in impls and
    /// traits, test modules).
    fn symbol(&self, owner: &SymbolPath, owner_kind: Option<SymbolKind>, item: Parsed) -> Symbol {
        let Parsed {
            decl_at,
            name,
            kind,
            end,
            children,
        } = item;
        let decl = self.line_of(decl_at);
        let start = header_start(&self.lines, decl);
        let header: Vec<&str> = (start..decl)
            .map(|line| line_at(&self.lines, line).trim())
            .collect();
        let kind = match kind {
            SymbolKind::Function | SymbolKind::Method
                if header.iter().any(|line| is_test_attr(line)) =>
            {
                SymbolKind::Test
            }
            SymbolKind::Function | SymbolKind::Method
                if matches!(owner_kind, Some(SymbolKind::Impl | SymbolKind::Trait)) =>
            {
                SymbolKind::Method
            }
            SymbolKind::Function | SymbolKind::Method => SymbolKind::Function,
            SymbolKind::Module
                if name == "tests" || header.iter().any(|line| is_cfg_test(line)) =>
            {
                SymbolKind::Test
            }
            other => other,
        };
        let body = LineRange::new(decl, end);
        Symbol {
            signature: signature(&self.lines, body, true),
            doc: doc_of(&header),
            path: owner.child(&name),
            kind,
            name,
            range: LineRange::new(start, end),
            body,
            children,
        }
    }
}

/// Whether two same-named same-kind siblings carry a `cfg`/`cfg_attr` attribute in either
/// header: which branch is live is semantic knowledge, so the file refuses to guess. Same-named
/// siblings without a gate stay (a type and its impl blocks, repeated trait impls), exactly as
/// the server reports them.
fn cfg_duplicate(children: &[Symbol], lines: &[&str]) -> bool {
    for (index, one) in children.iter().enumerate() {
        for other in &children[index + 1..] {
            if one.name != other.name || one.kind != other.kind {
                continue;
            }
            let gated = |symbol: &Symbol| {
                (symbol.range.start..symbol.body.start).any(|line| {
                    matches!(
                        attr_path(line_at(lines, line).trim()),
                        Some("cfg" | "cfg_attr")
                    )
                })
            };
            if gated(one) || gated(other) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::RustSupport;
    use agent_ide_core::lang::{InsertWhere, LanguageSupport};

    /// The symbol whose children include one whose path ends with `#member`.
    fn owner_of<'a>(outline: &'a Outline, member: &str) -> &'a Symbol {
        let mut found: Option<&'a Symbol> = None;
        for symbol in &outline.symbols {
            symbol.walk(&mut |candidate| {
                if candidate.children.iter().any(|child| {
                    child
                        .path
                        .segments()
                        .last()
                        .is_some_and(|last| last == member)
                }) {
                    found = Some(candidate);
                }
            });
        }
        found.unwrap_or_else(|| panic!("owner of {member}"))
    }

    /// Finds one symbol by its `file#Owner/name` style path suffix (`Guard/new`).
    fn find<'a>(outline: &'a Outline, path: &str) -> &'a Symbol {
        outline
            .symbols
            .iter()
            .flat_map(|symbol| {
                let mut found = Vec::new();
                symbol.walk(&mut |candidate| found.push(candidate));
                found
            })
            .find(|symbol| symbol.path.to_string().ends_with(&format!("#{path}")))
            .unwrap_or_else(|| panic!("{path} not found"))
    }

    /// The fixture source; the expected values are the live rust-analyzer's answers for the
    /// same shapes (verified against the server outline of a scratch crate).
    const SOURCE: &str = "\
//! Module docs.
use std::fmt;

/// A guard.
#[derive(Debug)]
pub struct Guard {
    pub id: u32,
}

/// An enum.
pub enum State {
    On(u32),
    Off { why: String },
}

pub trait Shape {
    const NAME: &'static str;
    type Out;
    fn area(&self) -> u32;
    fn name_of(&self) -> String {
        String::from(Self::NAME)
    }
}

impl Guard {
    /// Builds.
    pub fn new(id: u32) -> Self {
        fn helper(inner: u32) -> u32 {
            inner + 1
        }
        helper(id)
    }
}

impl<T: Clone> crate::ext::Wrap<T> for Guard
where
    T: fmt::Debug,
{
    fn wrap(&self, t: T) -> T {
        t
    }
}

pub mod inner {
    pub fn deep() {}
}

macro_rules! make {
    () => {};
}

pub const C: u8 = 1;
pub static S: u8 = 2;
pub type Alias = u32;

pub fn outer() {
    let text = \"} not a brace\";
    let raw = r#\"}{\"#;
    let ch = '}';
    fn nested(a: u8) -> u8 {
        a
    }
    let _ = (text, raw, ch, nested(1));
    let _ = format!(\"{}\", 1);
}

#[cfg(test)]
mod tests {
    #[test]
    fn builds() {}
}
";

    fn outline() -> Outline {
        lexical_outline(Path::new("src/guard.rs"), SOURCE).expect("the fixture scans cleanly")
    }

    /// Addresses, kinds, ranges, signatures and docs match the server path's answers.
    #[test]
    fn addresses_kinds_ranges_and_signatures_match_the_server() {
        let outline = outline();
        assert_eq!(outline.line_count, 71);
        let at = |path: &str| find(&outline, path);
        for (path, kind, range, body) in [
            (
                "Guard",
                SymbolKind::Struct,
                LineRange::new(4, 8),
                LineRange::new(6, 8),
            ),
            (
                "Guard/id",
                SymbolKind::Field,
                LineRange::new(7, 7),
                LineRange::new(7, 7),
            ),
            (
                "State",
                SymbolKind::Enum,
                LineRange::new(10, 14),
                LineRange::new(11, 14),
            ),
            (
                "State/On",
                SymbolKind::Variant,
                LineRange::new(12, 12),
                LineRange::new(12, 12),
            ),
            (
                "State/Off",
                SymbolKind::Variant,
                LineRange::new(13, 13),
                LineRange::new(13, 13),
            ),
            (
                "State/Off/why",
                SymbolKind::Field,
                LineRange::new(13, 13),
                LineRange::new(13, 13),
            ),
            (
                "Shape",
                SymbolKind::Trait,
                LineRange::new(16, 23),
                LineRange::new(16, 23),
            ),
            (
                "Shape/NAME",
                SymbolKind::Constant,
                LineRange::new(17, 17),
                LineRange::new(17, 17),
            ),
            (
                "Shape/Out",
                SymbolKind::TypeAlias,
                LineRange::new(18, 18),
                LineRange::new(18, 18),
            ),
            (
                "Shape/area",
                SymbolKind::Method,
                LineRange::new(19, 19),
                LineRange::new(19, 19),
            ),
            (
                "Shape/name_of",
                SymbolKind::Method,
                LineRange::new(20, 22),
                LineRange::new(20, 22),
            ),
        ] {
            let symbol = at(path);
            assert_eq!(
                (symbol.kind, symbol.range, symbol.body),
                (kind, range, body),
                "{path}"
            );
        }
        // The inherent impl shares the struct's segment; its methods are methods.
        let inherent = owner_of(&outline, "new");
        assert_eq!(inherent.kind, SymbolKind::Impl);
        assert_eq!(inherent.name, "Guard");
        assert_eq!(inherent.range, LineRange::new(25, 33));
        let new = at("Guard/new");
        assert_eq!(new.range, LineRange::new(26, 32));
        assert_eq!(new.body, LineRange::new(27, 32));
        assert_eq!(new.signature, "pub fn new(id: u32) -> Self");
        assert_eq!(new.doc.as_deref(), Some("Builds."));
        // A nested function is a child of the function that encloses it, a plain fn.
        let helper = at("Guard/new/helper");
        assert_eq!(helper.kind, SymbolKind::Function);
        assert_eq!(helper.range, LineRange::new(28, 30));
        // The trait impl is addressed by the same name the server reports: the generic
        // parameter list and the where clause dropped, the trait path kept.
        let trait_impl = at("impl crate::ext::Wrap<T> for Guard/wrap");
        let owner = owner_of(&outline, "wrap");
        assert_eq!(owner.kind, SymbolKind::Impl);
        assert_eq!(owner.name, "impl crate::ext::Wrap<T> for Guard");
        assert_eq!(owner.range, LineRange::new(35, 42));
        assert_eq!(
            owner.signature,
            "impl<T: Clone> crate::ext::Wrap<T> for Guard where T: fmt::Debug"
        );
        assert_eq!(trait_impl.range, LineRange::new(39, 41));
        for (path, kind, range, signature) in [
            (
                "inner",
                SymbolKind::Module,
                LineRange::new(44, 46),
                "pub mod inner",
            ),
            (
                "inner/deep",
                SymbolKind::Function,
                LineRange::new(45, 45),
                "pub fn deep()",
            ),
            (
                "make",
                SymbolKind::Function,
                LineRange::new(48, 50),
                "macro_rules! make",
            ),
            (
                "C",
                SymbolKind::Constant,
                LineRange::new(52, 52),
                "pub const C: u8",
            ),
            (
                "S",
                SymbolKind::Constant,
                LineRange::new(53, 53),
                "pub static S: u8",
            ),
            (
                "Alias",
                SymbolKind::TypeAlias,
                LineRange::new(54, 54),
                "pub type Alias",
            ),
            (
                "outer",
                SymbolKind::Function,
                LineRange::new(56, 65),
                "pub fn outer()",
            ),
            (
                "outer/nested",
                SymbolKind::Function,
                LineRange::new(60, 62),
                "fn nested(a: u8) -> u8",
            ),
            (
                "tests",
                SymbolKind::Test,
                LineRange::new(67, 71),
                "mod tests",
            ),
            (
                "tests/builds",
                SymbolKind::Test,
                LineRange::new(69, 70),
                "fn builds()",
            ),
        ] {
            let symbol = at(path);
            assert_eq!(
                (symbol.kind, symbol.range, symbol.signature.as_str()),
                (kind, range, signature),
                "{path}"
            );
        }
        assert_eq!(at("Guard").doc.as_deref(), Some("A guard."));
        assert_eq!(at("Guard").signature, "pub struct Guard");
        assert_eq!(at("Guard/id").signature, "pub id: u32");
        assert_eq!(at("State/Off").signature, "Off");
        assert_eq!(at("State/Off/why").signature, "Off");
    }

    /// The outline the scanner builds is the one the edit path places inserts against.
    #[test]
    fn insertion_sites_resolve_over_the_lexical_outline() {
        let outline = outline();
        let anchor = SymbolPath::parse("Guard/new").unwrap();
        let site = RustSupport
            .insert_site(SOURCE, &outline, &anchor, InsertWhere::Before)
            .unwrap();
        assert_eq!(site.line, 26);
        assert_eq!(site.indent, "    ");
    }

    /// A `const _` initializer's items are hoisted to the enclosing level and the underscore
    /// constant itself is not a symbol, exactly as the server reports it.
    #[test]
    fn const_underscore_hoists_initializer_items() {
        let source = "const _: () = {\n    fn hidden() {}\n};\n";
        let outline = lexical_outline(Path::new("a.rs"), source).unwrap();
        assert_eq!(outline.symbols.len(), 1);
        assert_eq!(outline.symbols[0].name, "hidden");
        assert_eq!(outline.symbols[0].range, LineRange::new(2, 2));
    }

    /// Braces, `;` and keywords inside strings, raw strings, char literals and comments are
    /// never syntax; a statement-level macro call is skipped silently.
    #[test]
    fn literals_and_statement_macros_are_not_items() {
        let source = "pub fn a() {\n    let s = \"fn fake() {}\";\n    let r = r#\"};\"#;\n    \
let c = '{';\n    let _ = format!(\"{}\", c);\n    let _ = s.len() + r.len();\n}\n";
        let outline = lexical_outline(Path::new("a.rs"), source).unwrap();
        assert_eq!(outline.symbols.len(), 1);
        assert_eq!(outline.symbols[0].name, "a");
        assert!(outline.symbols[0].children.is_empty());
    }

    /// Every refusal rule: an unbalanced brace, an unknown token at item position, a macro
    /// invocation at item position, and cfg-duplicated items with the same name all answer
    /// `None`, so the file keeps waiting for the server instead of guessing a range.
    #[test]
    fn unclean_sources_are_refused() {
        let refused = [
            "fn a() {\n",
            "let x = 5;\n",
            "foo! { fn generated() {} }\n",
            "item!(x);\n",
            "#[cfg(unix)]\nfn platform() {}\n#[cfg(windows)]\nfn platform() {}\n",
            "#[cfg(test)]\nimpl Guard {}\nimpl Guard {}\n",
            "mod m {\n    fn only() {}\n",
        ];
        for source in refused {
            assert!(
                lexical_outline(Path::new("a.rs"), source).is_none(),
                "must be refused: {source:?}"
            );
        }
    }

    /// Same-named siblings without a gate stay, exactly as the server reports them: a type and
    /// its impl blocks share a segment, and `find` backtracks across them.
    #[test]
    fn same_named_siblings_without_a_gate_stay() {
        let source = "pub struct Guard;\nimpl Guard {}\nimpl Guard {\n    pub fn twice() {}\n}\n";
        let outline = lexical_outline(Path::new("a.rs"), source).unwrap();
        assert_eq!(outline.symbols.len(), 3);
        assert_eq!(outline.symbols[0].kind, SymbolKind::Struct);
        assert_eq!(outline.symbols[1].kind, SymbolKind::Impl);
        assert_eq!(outline.symbols[2].kind, SymbolKind::Impl);
        let path = SymbolPath::parse("Guard/twice").unwrap();
        assert_eq!(outline.find(&path).unwrap().range, LineRange::new(4, 4));
    }

    /// Tuple structs report no fields; a union, a unit struct and a `mod x;` leaf all scan.
    #[test]
    fn tuple_structs_unions_and_leaf_modules_scan() {
        let source = "pub struct Pair(u32, String);\npub union Overlap {\n    a: u32,\n    \
b: u32,\n}\npub struct Unit;\nmod leaf;\n";
        let outline = lexical_outline(Path::new("a.rs"), source).unwrap();
        let names: Vec<(&str, SymbolKind)> = outline
            .symbols
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.kind))
            .collect();
        assert_eq!(
            names,
            [
                ("Pair", SymbolKind::Struct),
                ("Overlap", SymbolKind::Struct),
                ("Unit", SymbolKind::Struct),
                ("leaf", SymbolKind::Module)
            ]
        );
        assert!(outline.symbols[0].children.is_empty());
        assert_eq!(outline.symbols[1].children.len(), 2);
        assert_eq!(outline.symbols[3].range, LineRange::new(7, 7));
    }

    /// The trait-support entry used by the core returns the lexical outline unchanged.
    #[test]
    fn the_support_trait_answers_from_source() {
        let from_source = RustSupport
            .outline_from_source(Path::new("src/guard.rs"), SOURCE)
            .expect("scans cleanly");
        assert_eq!(from_source, outline());
    }

    /// A `#[cfg(test)]` module inside a function body still marks its tests.
    #[test]
    fn test_kinds_refine_inside_nested_bodies() {
        let source = "pub fn outer() {\n    #[cfg(test)]\n    mod tests {\n        \
#[tokio::test]\n        async fn waits() {}\n    }\n}\n";
        let outline = lexical_outline(Path::new("a.rs"), source).unwrap();
        let tests = find(&outline, "outer/tests");
        assert_eq!(tests.kind, SymbolKind::Test);
        assert_eq!(tests.range, LineRange::new(2, 6));
        assert_eq!(find(&outline, "outer/tests/waits").kind, SymbolKind::Test);
    }
}
