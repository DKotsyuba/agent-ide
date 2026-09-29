//! Lexical outline of one Rust source file: the outline the server path
//! ([`crate::support::RustSupport::normalize`]) makes of rust-analyzer's document symbols,
//! computed from the text alone so symbol-addressed tools answer while the analyzer is still
//! loading.
//!
//! The text is parsed by `syn`, a complete Rust parser, and every symbol rust-analyzer's file
//! structure reports is rebuilt as the `DocumentSymbol` the server would send — name, LSP kind,
//! node range, selection range and children in source order — then handed to the same
//! `normalize` that converts the server's answer, so naming, kind refinement, test detection,
//! headers, signatures and docs are one code path for both. The symbols rust-analyzer reports:
//!
//! * functions (methods when they take `self`), structs, unions, enums, their variants, named
//!   fields, traits, modules, type aliases, constants, statics and `macro_rules!` definitions, at
//!   any depth — inside function bodies, closures, blocks, initializers, types, impl and trait
//!   bodies — each a child of the nearest enclosing symbol;
//! * impl blocks, labelled `impl Type`, `impl Trait for Type` or `impl !Trait for Type` from the
//!   verbatim source text of the trait path and the self type, selected at the self type;
//! * no symbol for `const _`: the items inside its initializer belong to the enclosing symbol;
//! * no symbol for macro invocations, `use` or `extern crate`, nor for items a macro would expand
//!   to (document symbols are syntax only).
//!
//! A node range runs from the item's first outer attribute or doc comment (syn keeps doc
//! comments as attributes, rust-analyzer as attached trivia) to its last token, so a field or a
//! variant ends before its comma; the selection range is the name.
//!
//! Refusal over guessing: whatever the parse cannot reproduce with the server's exact answer
//! makes the whole outline `None`, and the file keeps waiting for the server —
//!
//! * text syn does not parse: rust-analyzer recovers from a syntax error with a tree no parser
//!   here reproduces; also a shebang line, syntax syn keeps only as verbatim tokens (unstable
//!   forms, a macro 2.0 definition), a trait alias, and an `extern` block (the server reports
//!   the block itself as a symbol);
//! * nesting the recursive parser could not take within its stack ([`too_nested`]): brackets
//!   deeper than [`MAX_DEPTH`] or an operator chain above [`MAX_NESTING`]; and more than
//!   [`MAX_SYMBOLS`] symbols;
//! * whitespace rustc does not accept in code (a no-break space, …), which the parser skips;
//! * a `// region:` comment, which the server reports as a symbol;
//! * comments rust-analyzer attaches to an item's node but a parser drops: comment text on the
//!   nearest non-blank line above the item's first token (a `//!` inner doc line excepted, which
//!   is never attached), or a blank line between the item's first attribute or doc comment and
//!   its first token;
//! * two same-named same-kind siblings where either carries a `cfg`/`cfg_attr` attribute.
//!
//! The corpus under `tests/fixtures/lexical` pins this against recorded rust-analyzer answers:
//! every file there is either equal to the server path's outline or refused. The recordings come
//! from the rust-analyzer build named in `tests/fixtures/lexical/VERSION`; after a rust-analyzer
//! upgrade they are recorded again (`record.mjs`) and the corpus test decides whether the
//! outline still agrees.
//!
//! The parse runs on a short-lived thread with a stack of its own ([`PARSE_STACK`]): how deep it
//! may recurse does not depend on the caller's stack or the build profile, a parser panic refuses
//! instead of unwinding into the caller, and `proc-macro2`'s thread-local span table (which keeps
//! the text of every source parsed on a thread) is freed with the thread.

use std::collections::HashMap;
use std::path::Path;

use agent_ide_core::lang::brace::{line_at, source_lines};
use agent_ide_core::lang::{LanguageSupport, Outline, Symbol, SymbolKind};
use async_lsp::lsp_types as lsp;
use proc_macro2::{Delimiter, LineColumn, Spacing, Span, TokenStream, TokenTree};
use quote::ToTokens;
use syn::visit::{self, Visit};

use crate::module_graph::blanked_code;
use crate::support::{RustSupport, attr_path};

/// Deepest `(…)`/`[…]`/`{…}` nesting the parser is given; deeper text is refused before the
/// recursive-descent parser (and the recursive walk, conversion and drop after it) could exhaust
/// its stack.
const MAX_DEPTH: usize = 128;

/// Largest nesting estimate ([`too_nested`]) the parser is given; far above hand-written code,
/// far below what [`PARSE_STACK`] holds in an unoptimized build.
const MAX_NESTING: usize = 1024;

/// Stack of the parse thread, in bytes: address space reserved per outline, touched only as deep
/// as the parse recurses.
const PARSE_STACK: usize = 64 << 20;

/// Most symbols one outline may carry; a larger file waits for the server.
const MAX_SYMBOLS: usize = 20_000;

/// Builds the lexical outline of `file` (the path the outline and its symbol paths carry) from
/// its text `source`, or `None` when the text is refused (see the module docs). Pure function of
/// the text: no filesystem, server or subprocess; it blocks the caller while one short-lived
/// parse thread runs (about 50 ms for a 570 KB file in a release build).
pub(crate) fn lexical_outline(file: &Path, source: &str) -> Option<Outline> {
    // rust-analyzer reports every `// region: name` comment as a symbol of its own, which a
    // parser drops; any mention refuses (a string that contains it too).
    if source.contains("// region:") {
        return None;
    }
    let symbols = document_symbols(source)?;
    let outline = RustSupport.normalize(file, source, symbols);
    let lines = source_lines(source);
    let mut duplicate = cfg_duplicate(&outline.symbols, &lines);
    for symbol in &outline.symbols {
        symbol.walk(&mut |symbol| duplicate |= cfg_duplicate(&symbol.children, &lines));
    }
    (!duplicate).then_some(outline)
}

/// rust-analyzer's `textDocument/documentSymbol` answer for `source` rebuilt from a `syn` parse,
/// or `None` when the text is refused (see the module docs). Positions are 0-based lines and
/// character columns; only the lines and the selection line are consumed downstream.
///
/// The parse runs on a thread of its own with a [`PARSE_STACK`]-byte stack, so how deep it may
/// recurse does not depend on the caller's stack or on the build profile, and its thread-local
/// span table dies with it. A thread that cannot be started, or a parser panic, refuses.
fn document_symbols(source: &str) -> Option<Vec<lsp::DocumentSymbol>> {
    let chars: Vec<char> = source.chars().collect();
    let code = blanked_code(&chars);
    // Whitespace the parser here skips (`char::is_whitespace`) but rustc and rust-analyzer do
    // not (a no-break space, …) is an error token to the server, never a separator.
    if code
        .iter()
        .any(|&character| character.is_whitespace() && !rust_whitespace(character))
    {
        return None;
    }
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("rust-lexical-outline".to_owned())
            .stack_size(PARSE_STACK)
            .spawn_scoped(scope, || parsed_symbols(source, &chars, &code))
            .ok()?
            .join()
            .ok()?
    })
}

/// The body of [`document_symbols`] on the parse thread: tokenizes `source` (whose characters
/// are `chars` and code-only form `code`), refuses nesting the parser could not take within its
/// stack ([`too_nested`]), parses and walks it.
fn parsed_symbols(source: &str, chars: &[char], code: &[char]) -> Option<Vec<lsp::DocumentSymbol>> {
    let tokens: TokenStream = source.parse().ok()?;
    if too_nested(tokens.clone()) {
        return None;
    }
    let file: syn::File = syn::parse2(tokens).ok()?;
    let mut line_starts = vec![0usize];
    line_starts.extend(
        chars
            .iter()
            .enumerate()
            .filter(|(_, character)| **character == '\n')
            .map(|(index, _)| index + 1),
    );
    let mut walker = Walker {
        source,
        lines: source_lines(source),
        chars,
        code,
        line_starts,
        frames: vec![Vec::new()],
        symbols: 0,
        refused: false,
    };
    walker.visit_file(&file);
    if walker.refused {
        return None;
    }
    walker.frames.pop()
}

/// Whether `character` is whitespace to rustc and rust-analyzer: Unicode `Pattern_White_Space`,
/// which includes the left-to-right and right-to-left marks and excludes the no-break and other
/// typographic spaces `char::is_whitespace` accepts.
fn rust_whitespace(character: char) -> bool {
    matches!(
        character,
        '\t' | '\n'
            | '\u{b}'
            | '\u{c}'
            | '\r'
            | ' '
            | '\u{85}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// Keywords that nest the syntax tree without a delimiter: prefix expressions (`return x`,
/// `move || x`), casts, `else if` chains, `let` conditions, `dyn`/`impl` types.
const NESTING_KEYWORDS: [&str; 12] = [
    "as", "async", "become", "box", "break", "dyn", "else", "impl", "let", "move", "return",
    "yield",
];

/// Whether `tokens` may nest the parser's recursion (and the tree walk and drop after it) deeper
/// than its stack allows: delimited groups deeper than [`MAX_DEPTH`], or a nesting estimate
/// above [`MAX_NESTING`]. Measured without recursion — the check must not need the stack it
/// protects — in one pass over every token.
///
/// The estimate is an upper bound on the syntax tree's depth. Syntax nests only through tokens:
/// a delimited group, an operator (prefix `!`/`-`/`*`/`&`, binary, `.`, `?`, `..`, `@`, `->`,
/// generic `<`/`>`) or a keyword of [`NESTING_KEYWORDS`]. Within one group those tokens are
/// counted per segment, a stretch the parser treats as one subtree; segments end at `;`, at `=>`,
/// at a `,` outside generic arguments (a `<` still open makes the comma part of the stretch),
/// and after a `{…}` block followed by an item, a statement or an attribute (an identifier other
/// than `as`/`else`, or `#`). A group's estimate is its enclosing group's plus the count of the
/// whole segment holding it (the tokens after it nest it too: `(a) + b + c`). The bound is
/// conservative: a long chain of operators in one expression (hundreds of `else if` arms, a
/// thousand-term sum) is refused rather than parsed.
fn too_nested(tokens: TokenStream) -> bool {
    let mut pending = vec![(tokens, 0usize, 0usize)];
    while let Some((stream, base, depth)) = pending.pop() {
        let trees: Vec<TokenTree> = stream.into_iter().collect();
        let mut segments = vec![0usize];
        let mut groups = Vec::new();
        let mut angles = 0usize;
        for (index, tree) in trees.iter().enumerate() {
            let next = trees.get(index + 1);
            let joined = |previous: char| {
                index.checked_sub(1).is_some_and(|before| {
                    matches!(&trees[before], TokenTree::Punct(punct)
                        if punct.as_char() == previous && punct.spacing() == Spacing::Joint)
                })
            };
            let segment = segments.len() - 1;
            let mut split = false;
            match tree {
                TokenTree::Group(group) => {
                    segments[segment] += 1;
                    groups.push((group.stream(), segment));
                    split = group.delimiter() == Delimiter::Brace
                        && match next {
                            Some(TokenTree::Ident(ident)) => ident != "as" && ident != "else",
                            Some(TokenTree::Punct(punct)) => punct.as_char() == '#',
                            _ => false,
                        };
                }
                TokenTree::Punct(punct) => match punct.as_char() {
                    ';' => split = true,
                    ',' => split = angles == 0,
                    ':' | '#' | '$' | '\'' => {}
                    // The `=` of `=>`; the `>` splits.
                    '=' if punct.spacing() == Spacing::Joint
                        && matches!(next, Some(TokenTree::Punct(arrow)) if arrow.as_char() == '>') =>
                        {}
                    '>' if joined('=') => split = true,
                    '<' => {
                        angles += 1;
                        segments[segment] += 1;
                    }
                    '>' => {
                        if !joined('-') {
                            angles = angles.saturating_sub(1);
                        }
                        segments[segment] += 1;
                    }
                    _ => segments[segment] += 1,
                },
                TokenTree::Ident(ident) => {
                    if NESTING_KEYWORDS.iter().any(|keyword| ident == keyword) {
                        segments[segment] += 1;
                    }
                }
                TokenTree::Literal(_) => {}
            }
            if split {
                segments.push(0);
                angles = 0;
            }
        }
        if segments.iter().any(|count| base + count > MAX_NESTING) {
            return true;
        }
        for (stream, segment) in groups {
            if depth + 1 > MAX_DEPTH {
                return true;
            }
            pending.push((stream, base + segments[segment], depth + 1));
        }
    }
    false
}

/// Builds the document symbols of one parsed file, one visitor pass in source order.
struct Walker<'a> {
    /// The source text, for impl labels sliced verbatim.
    source: &'a str,
    /// Source lines, for the blank-line rule.
    lines: Vec<&'a str>,
    /// Source characters (span columns and `line_starts` count characters, not bytes).
    chars: &'a [char],
    /// Code-only form of the source (comments and literal contents blanked, offsets unchanged),
    /// to find the last code character before an item.
    code: &'a [char],
    /// Character offset of each line's first character (line 1 starts at 0).
    line_starts: Vec<usize>,
    /// Symbols collected per open symbol: the file level first, then one frame per symbol whose
    /// contents are being walked; the innermost frame receives the next finished symbol.
    frames: Vec<Vec<lsp::DocumentSymbol>>,
    /// Symbols built so far.
    symbols: usize,
    /// Whether anything was seen that makes the outline a guess; the caller returns `None`.
    refused: bool,
}

impl Walker<'_> {
    /// Adds one symbol for `node` to the innermost frame. `name` and `kind` are rust-analyzer's,
    /// `selection` the span it selects (the name, or an impl's self type); `walk` visits the
    /// node's contents, whose symbols become its children. The range runs from the node's first
    /// token to its last; the node is checked for comments the server would attach to it.
    fn symbol(
        &mut self,
        node: &dyn ToTokens,
        name: String,
        kind: lsp::SymbolKind,
        selection: Span,
        walk: impl FnOnce(&mut Self),
    ) {
        let tokens: Vec<TokenTree> = node.to_token_stream().into_iter().collect();
        let (Some(first), Some(last)) = (tokens.first(), tokens.last()) else {
            self.refused = true;
            return;
        };
        // The first token after the outer attributes (`#` then `[…]`, doc comments included).
        let mut index = 0;
        while matches!(&tokens.get(index), Some(TokenTree::Punct(pound)) if pound.as_char() == '#')
            && matches!(
                &tokens.get(index + 1),
                Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Bracket
            )
        {
            index += 2;
        }
        let declaration = tokens.get(index).unwrap_or(first).span().start().line;
        let (start, end) = (first.span().start(), last.span().end());
        // Every parsed token has a source position (line 1 or later); a synthesized one would not.
        if start.line == 0 {
            self.refused = true;
            return;
        }
        self.check_attached_trivia(start, declaration);
        self.symbols += 1;
        self.refused |= self.symbols > MAX_SYMBOLS;
        self.frames.push(Vec::new());
        walk(self);
        let children = self.frames.pop().unwrap_or_default();
        #[allow(deprecated)] // `deprecated` is a required field of the LSP type.
        let symbol = lsp::DocumentSymbol {
            name,
            detail: None,
            kind,
            tags: None,
            deprecated: None,
            range: lsp::Range::new(position(start), position(end)),
            selection_range: lsp::Range::new(
                position(selection.start()),
                position(selection.end()),
            ),
            children: (!children.is_empty()).then_some(children),
        };
        if let Some(frame) = self.frames.last_mut() {
            frame.push(symbol);
        }
    }

    /// Refuses when rust-analyzer may attach comments to the item starting at `start` (its first
    /// attribute, doc comment or keyword), which a parser does not see: comment text on the
    /// nearest non-blank line above `start` — between the last code before the item and the
    /// item — unless it is a `//!` inner doc line (never attached, nor anything above it); or a
    /// blank line between `start` and the item's first token after its attributes, on line
    /// `declaration` (rust-analyzer attaches a doc comment across one only when no plain comment
    /// sits between).
    fn check_attached_trivia(&mut self, start: LineColumn, declaration: usize) {
        let line_start = self.line_starts[start.line - 1];
        let first = line_start + start.column;
        let gap = self.code[..first]
            .iter()
            .rposition(|&character| !rust_whitespace(character))
            .map_or(0, |at| at + 1);
        let above: String = self.chars[gap.min(line_start)..line_start].iter().collect();
        let commented = above
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .is_some_and(|nearest| !nearest.starts_with("//!"));
        let blank = (start.line..declaration)
            .any(|line| line_at(&self.lines, line as u32).trim().is_empty());
        self.refused |= commented || blank;
    }

    /// The verbatim source text of `node`, from its first token to its last, as rust-analyzer's
    /// syntax text; `None` for a node without tokens or without a source position.
    fn text(&self, node: &dyn ToTokens) -> Option<String> {
        let mut tokens = node.to_token_stream().into_iter();
        let first = tokens.next()?.span();
        let last = tokens.last().map_or(first, |token| token.span());
        self.source
            .get(first.byte_range().start..last.byte_range().end)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    }

    /// Adds a function or method symbol for `node` with signature `sig`, named and selected by
    /// the function's name: rust-analyzer's `METHOD` when it takes `self`, `FUNCTION` otherwise
    /// (the conversion refines both by owner and test attribute alike). `walk` visits the
    /// function's contents, as in [`Walker::symbol`].
    fn function(
        &mut self,
        node: &dyn ToTokens,
        sig: &syn::Signature,
        walk: impl FnOnce(&mut Self),
    ) {
        let kind = if sig.receiver().is_some() {
            lsp::SymbolKind::METHOD
        } else {
            lsp::SymbolKind::FUNCTION
        };
        self.symbol(node, sig.ident.to_string(), kind, sig.ident.span(), walk);
    }

    /// Adds a constant symbol for `node` named and selected by `ident`, whose contents `walk`
    /// visits; for `const _` there is no symbol and `walk` runs in the enclosing one, so the
    /// items of the initializer become its children.
    fn constant(&mut self, node: &dyn ToTokens, ident: &syn::Ident, walk: impl FnOnce(&mut Self)) {
        if ident == "_" {
            walk(self);
        } else {
            let name = ident.to_string();
            self.symbol(node, name, lsp::SymbolKind::CONSTANT, ident.span(), walk);
        }
    }

    /// Adds a symbol for `node` named and selected by `ident`, with rust-analyzer's `kind`;
    /// `walk` visits its contents, as in [`Walker::symbol`].
    fn named(
        &mut self,
        node: &dyn ToTokens,
        ident: &syn::Ident,
        kind: lsp::SymbolKind,
        walk: impl FnOnce(&mut Self),
    ) {
        self.symbol(node, ident.to_string(), kind, ident.span(), walk);
    }
}

/// The LSP position of a span boundary: 0-based line, character column.
fn position(at: LineColumn) -> lsp::Position {
    lsp::Position::new(at.line.saturating_sub(1) as u32, at.column as u32)
}

impl<'ast> Visit<'ast> for Walker<'_> {
    /// Refuses items the outline cannot reproduce: verbatim (unparsed) items, trait aliases and
    /// `extern` blocks; walks every other item.
    fn visit_item(&mut self, node: &'ast syn::Item) {
        match node {
            syn::Item::Verbatim(_) | syn::Item::TraitAlias(_) | syn::Item::ForeignMod(_) => {
                self.refused = true;
            }
            _ => visit::visit_item(self, node),
        }
    }

    /// Refuses a verbatim impl member; walks every other.
    fn visit_impl_item(&mut self, node: &'ast syn::ImplItem) {
        match node {
            syn::ImplItem::Verbatim(_) => self.refused = true,
            _ => visit::visit_impl_item(self, node),
        }
    }

    /// Refuses a verbatim trait member; walks every other.
    fn visit_trait_item(&mut self, node: &'ast syn::TraitItem) {
        match node {
            syn::TraitItem::Verbatim(_) => self.refused = true,
            _ => visit::visit_trait_item(self, node),
        }
    }

    /// Refuses a verbatim expression, whose tokens may hold items; walks every other.
    fn visit_expr(&mut self, node: &'ast syn::Expr) {
        match node {
            syn::Expr::Verbatim(_) => self.refused = true,
            _ => visit::visit_expr(self, node),
        }
    }

    /// Refuses a verbatim type; walks every other.
    fn visit_type(&mut self, node: &'ast syn::Type) {
        match node {
            syn::Type::Verbatim(_) => self.refused = true,
            _ => visit::visit_type(self, node),
        }
    }

    /// Refuses a verbatim pattern; walks every other.
    fn visit_pat(&mut self, node: &'ast syn::Pat) {
        match node {
            syn::Pat::Verbatim(_) => self.refused = true,
            _ => visit::visit_pat(self, node),
        }
    }

    /// Refuses a verbatim bound; walks every other.
    fn visit_type_param_bound(&mut self, node: &'ast syn::TypeParamBound) {
        match node {
            syn::TypeParamBound::Verbatim(_) => self.refused = true,
            _ => visit::visit_type_param_bound(self, node),
        }
    }

    /// A function symbol.
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.function(node, &node.sig, |walker| visit::visit_item_fn(walker, node));
    }

    /// A method symbol of an impl.
    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        self.function(node, &node.sig, |walker| {
            visit::visit_impl_item_fn(walker, node)
        });
    }

    /// A method symbol of a trait.
    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        self.function(node, &node.sig, |walker| {
            visit::visit_trait_item_fn(walker, node)
        });
    }

    /// A struct symbol; its named fields are its children.
    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
        let kind = lsp::SymbolKind::STRUCT;
        self.named(node, &node.ident, kind, |walker| {
            visit::visit_item_struct(walker, node)
        });
    }

    /// A union symbol, which rust-analyzer reports as a struct.
    fn visit_item_union(&mut self, node: &'ast syn::ItemUnion) {
        let kind = lsp::SymbolKind::STRUCT;
        self.named(node, &node.ident, kind, |walker| {
            visit::visit_item_union(walker, node)
        });
    }

    /// An enum symbol; its variants are its children.
    fn visit_item_enum(&mut self, node: &'ast syn::ItemEnum) {
        let kind = lsp::SymbolKind::ENUM;
        self.named(node, &node.ident, kind, |walker| {
            visit::visit_item_enum(walker, node)
        });
    }

    /// An enum variant symbol; a record variant's fields are its children.
    fn visit_variant(&mut self, node: &'ast syn::Variant) {
        let kind = lsp::SymbolKind::ENUM_MEMBER;
        self.named(node, &node.ident, kind, |walker| {
            visit::visit_variant(walker, node)
        });
    }

    /// A named field symbol; a tuple field is no symbol (its type is still walked).
    fn visit_field(&mut self, node: &'ast syn::Field) {
        match &node.ident {
            Some(ident) => self.named(node, ident, lsp::SymbolKind::FIELD, |walker| {
                visit::visit_field(walker, node)
            }),
            None => visit::visit_field(self, node),
        }
    }

    /// A trait symbol, which rust-analyzer reports as an interface.
    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        let kind = lsp::SymbolKind::INTERFACE;
        self.named(node, &node.ident, kind, |walker| {
            visit::visit_item_trait(walker, node)
        });
    }

    /// A module symbol, inline or `mod name;`.
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        let kind = lsp::SymbolKind::MODULE;
        self.named(node, &node.ident, kind, |walker| {
            visit::visit_item_mod(walker, node)
        });
    }

    /// A type alias symbol, which rust-analyzer reports as a type parameter.
    fn visit_item_type(&mut self, node: &'ast syn::ItemType) {
        let kind = lsp::SymbolKind::TYPE_PARAMETER;
        self.named(node, &node.ident, kind, |walker| {
            visit::visit_item_type(walker, node)
        });
    }

    /// An associated type symbol of an impl.
    fn visit_impl_item_type(&mut self, node: &'ast syn::ImplItemType) {
        let kind = lsp::SymbolKind::TYPE_PARAMETER;
        self.named(node, &node.ident, kind, |walker| {
            visit::visit_impl_item_type(walker, node)
        });
    }

    /// An associated type symbol of a trait.
    fn visit_trait_item_type(&mut self, node: &'ast syn::TraitItemType) {
        let kind = lsp::SymbolKind::TYPE_PARAMETER;
        self.named(node, &node.ident, kind, |walker| {
            visit::visit_trait_item_type(walker, node)
        });
    }

    /// A constant symbol (none for `const _`).
    fn visit_item_const(&mut self, node: &'ast syn::ItemConst) {
        self.constant(node, &node.ident, |walker| {
            visit::visit_item_const(walker, node)
        });
    }

    /// An associated constant symbol of an impl (none for `const _`).
    fn visit_impl_item_const(&mut self, node: &'ast syn::ImplItemConst) {
        self.constant(node, &node.ident, |walker| {
            visit::visit_impl_item_const(walker, node)
        });
    }

    /// An associated constant symbol of a trait.
    fn visit_trait_item_const(&mut self, node: &'ast syn::TraitItemConst) {
        self.constant(node, &node.ident, |walker| {
            visit::visit_trait_item_const(walker, node)
        });
    }

    /// A static symbol, which rust-analyzer reports as a constant.
    fn visit_item_static(&mut self, node: &'ast syn::ItemStatic) {
        let kind = lsp::SymbolKind::CONSTANT;
        self.named(node, &node.ident, kind, |walker| {
            visit::visit_item_static(walker, node)
        });
    }

    /// A `macro_rules!` definition symbol, which rust-analyzer reports as a function; a macro
    /// invocation at item position is no symbol.
    fn visit_item_macro(&mut self, node: &'ast syn::ItemMacro) {
        match &node.ident {
            Some(ident) => self.named(node, ident, lsp::SymbolKind::FUNCTION, |walker| {
                visit::visit_item_macro(walker, node)
            }),
            None => visit::visit_item_macro(self, node),
        }
    }

    /// An impl symbol labelled as rust-analyzer labels it, `impl Type` or
    /// `impl Trait for Type` (`!` kept before a negative impl's trait) from the verbatim text of
    /// the trait path and the self type, and selected at the self type.
    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        let target = self.text(&node.self_ty);
        let label = match (&node.trait_, target) {
            (None, Some(target)) => Some(format!("impl {target}")),
            (Some((bang, path, _)), Some(target)) => self.text(path).map(|path| {
                let bang = if bang.is_some() { "!" } else { "" };
                format!("impl {bang}{path} for {target}")
            }),
            (_, None) => None,
        };
        let (Some(label), Some(selection)) = (label, first_span(&node.self_ty)) else {
            self.refused = true;
            return;
        };
        let kind = lsp::SymbolKind::OBJECT;
        self.symbol(node, label, kind, selection, |walker| {
            visit::visit_item_impl(walker, node)
        });
    }
}

/// The span of `node`'s first token, `None` for a node without tokens.
fn first_span(node: &dyn ToTokens) -> Option<Span> {
    node.to_token_stream()
        .into_iter()
        .next()
        .map(|token| token.span())
}

/// Whether two same-named same-kind siblings carry a `cfg`/`cfg_attr` attribute in either
/// header: which branch is live is semantic knowledge, so the file refuses to guess. Same-named
/// siblings without a gate stay (a type and its impl blocks, repeated trait impls), exactly as
/// the server reports them. Linear in the number of siblings.
fn cfg_duplicate(children: &[Symbol], lines: &[&str]) -> bool {
    let mut groups: HashMap<(&str, SymbolKind), (usize, bool)> = HashMap::new();
    for symbol in children {
        let gated = (symbol.range.start..symbol.body.start).any(|line| {
            matches!(
                attr_path(line_at(lines, line).trim()),
                Some("cfg" | "cfg_attr")
            )
        });
        let group = groups
            .entry((symbol.name.as_str(), symbol.kind))
            .or_default();
        group.0 += 1;
        group.1 |= gated;
    }
    groups.values().any(|&(count, gated)| count > 1 && gated)
}

/// Unit tests: the recorded corpus against rust-analyzer's answers, the refusal rules, the
/// nesting bound on a small stack, and the outline shapes the edit path relies on.
#[cfg(test)]
mod tests {
    use super::*;
    use agent_ide_core::lang::{InsertWhere, LineRange, SymbolPath};

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

    /// The lexical outline of [`SOURCE`], which must scan cleanly.
    fn outline() -> Outline {
        lexical_outline(Path::new("src/guard.rs"), SOURCE).expect("the fixture scans cleanly")
    }

    /// One line per symbol, depth-first: everything [`Symbol`] carries, for a readable diff.
    fn flat(outline: &Outline) -> Vec<String> {
        let mut lines = Vec::new();
        for symbol in &outline.symbols {
            symbol.walk(&mut |symbol| {
                lines.push(format!(
                    "{} {:?} range {} body {} sig {:?} doc {:?} children {}",
                    symbol.path,
                    symbol.kind,
                    symbol.range,
                    symbol.body,
                    symbol.signature,
                    symbol.doc,
                    symbol.children.len()
                ));
            });
        }
        lines
    }

    /// Line ranges of `symbols` and their children as rust-analyzer reported them, in the
    /// depth-first order [`Symbol::walk`] visits the normalized outline.
    fn recorded_ranges(symbols: &[async_lsp::lsp_types::DocumentSymbol], into: &mut Vec<String>) {
        for symbol in symbols {
            into.push(agent_ide_core::lang::lines_of(&symbol.range).to_string());
            recorded_ranges(symbol.children.as_deref().unwrap_or_default(), into);
        }
    }

    /// The corpus under `tests/fixtures/lexical`: every `<name>.rs` sits next to the live
    /// rust-analyzer's `textDocument/documentSymbol` answer for it, `<name>.json`, recorded with
    /// `record.mjs` from the build named in `VERSION`. Each file's lexical outline either equals
    /// the server path's outline ([`RustSupport::normalize`] of the recording) exactly — names,
    /// kinds, ranges, bodies, signatures, docs, children and order — or is refused; a different
    /// outline is never allowed. Files named `refused_*` must be refused (a syntax error
    /// rust-analyzer recovers from, or comments and blocks the parse does not reproduce), every
    /// other file must match.
    /// The server path is pinned too: every normalized range is the one rust-analyzer reported,
    /// so a header rule shared by both paths cannot widen both the same wrong way.
    #[test]
    fn the_corpus_equals_the_recorded_server_outline_or_is_refused() {
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lexical");
        let mut sources: Vec<_> = std::fs::read_dir(&corpus)
            .expect("corpus directory")
            .map(|entry| entry.expect("corpus entry").path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
            .collect();
        sources.sort();
        assert!(sources.len() >= 24, "corpus: {sources:?}");
        let mut exact = 0;
        for path in sources {
            let source = std::fs::read_to_string(&path).expect("corpus source");
            let recorded = std::fs::read_to_string(path.with_extension("json"))
                .unwrap_or_else(|_| panic!("{} has no recording; run record.mjs", path.display()));
            let recorded: Vec<async_lsp::lsp_types::DocumentSymbol> =
                serde_json::from_str(&recorded).expect("recorded document symbols");
            let file = Path::new(path.file_name().expect("file name"));
            let mut reported = Vec::new();
            recorded_ranges(&recorded, &mut reported);
            let server = RustSupport.normalize(file, &source, recorded);
            let mut normalized = Vec::new();
            for symbol in &server.symbols {
                symbol.walk(&mut |symbol| normalized.push(symbol.range.to_string()));
            }
            assert_eq!(
                normalized,
                reported,
                "{}: normalized ranges",
                file.display()
            );
            let lexical = lexical_outline(file, &source);
            if file.to_string_lossy().starts_with("refused_") {
                assert!(
                    lexical.is_none(),
                    "{}: must be refused, scanned as {:#?}",
                    file.display(),
                    lexical.as_ref().map(flat)
                );
                continue;
            }
            let lexical =
                lexical.unwrap_or_else(|| panic!("{}: must scan cleanly", file.display()));
            assert_eq!(flat(&lexical), flat(&server), "{}", file.display());
            assert_eq!(lexical, server, "{}", file.display());
            exact += 1;
        }
        assert!(exact >= 11, "{exact} exact corpus files");
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

    /// The lexical outline is the one the edit path places inserts against.
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

    /// Every refusal rule outside the corpus answers `None`, so the file keeps waiting for the
    /// server instead of guessing a range: text that does not parse (an unbalanced brace, a
    /// statement at item position, a shebang), syntax kept only as verbatim tokens (a
    /// macro 2.0 definition), a trait alias, an extern block, whitespace rustc rejects, and
    /// cfg-duplicated items with the same name.
    #[test]
    fn unclean_sources_are_refused() {
        let refused = [
            "fn a() {\n",
            "let x = 5;\n",
            "mod m {\n    fn only() {}\n",
            "#!/usr/bin/env run-cargo-script\nfn main() {}\n",
            "macro m() {}\n",
            "trait Both = Clone + Send;\n",
            "extern \"C\" {\n    fn abs(input: i32) -> i32;\n}\n",
            "fn spaced() {\u{a0}}\n",
            "#[cfg(unix)]\nfn platform() {}\n#[cfg(windows)]\nfn platform() {}\n",
            "#[cfg(test)]\nimpl Guard {}\nimpl Guard {}\n",
        ];
        for source in refused {
            assert!(
                lexical_outline(Path::new("a.rs"), source).is_none(),
                "must be refused: {source:?}"
            );
        }
    }

    /// Pathological nesting — the review's 10 000 nested braces (a 20 KB file), a 10 000-deep
    /// generic type, a 10 000-long prefix operator chain and a 10 000-term sum whose blocks
    /// separate every term — is refused before the parser recurses, from a caller whose stack is
    /// only 256 KiB; ordinary nesting still outlines from that caller, because the parse runs
    /// on its own stack.
    #[test]
    fn pathological_nesting_is_refused_without_overflow() {
        let deep = 10_000;
        let refused = [
            format!("fn f() {{{}{}}}\n", "{".repeat(deep), "}".repeat(deep)),
            format!(
                "fn f() {{ let _: {}u8{} = x; }}\n",
                "Vec<".repeat(deep),
                ">".repeat(deep)
            ),
            format!("fn f() {{ let _ = {}x; }}\n", "!".repeat(deep)),
            format!("fn f() {{ let _ = 1{}; }}\n", " + {1} as u8".repeat(deep)),
        ];
        let nested = format!(
            "fn f() {{{}\nfn inner() {{}}\n{}}}\n",
            "{".repeat(100),
            "}".repeat(100)
        );
        let (refused, inner) = std::thread::Builder::new()
            .stack_size(256 << 10)
            .spawn(move || {
                let refused = refused
                    .iter()
                    .map(|source| lexical_outline(Path::new("a.rs"), source).is_none())
                    .collect::<Vec<_>>();
                let inner = lexical_outline(Path::new("a.rs"), &nested)
                    .map(|outline| find(&outline, "f/inner").range);
                (refused, inner)
            })
            .expect("small-stack thread")
            .join()
            .expect("no overflow");
        assert_eq!(refused, [true; 4]);
        assert_eq!(inner, Some(LineRange::new(2, 2)));
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
