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
//!   bodies — each a child of the nearest enclosing symbol, siblings ordered by where they start;
//! * impl blocks, labelled `impl Type`, `impl Trait for Type` or `impl !Trait for Type` from the
//!   verbatim source text of the trait path and the self type, selected at the self type;
//! * no symbol for `const _`: the items inside its initializer belong to the enclosing symbol;
//! * no symbol for macro invocations, `use` or `extern crate`, nor for items a macro would expand
//!   to (document symbols are syntax only).
//!
//! A node range runs from the item's first outer attribute or doc comment (syn keeps doc
//! comments as attributes, rust-analyzer as attached trivia) to its last token, so a field or a
//! variant ends before its comma; the selection range is the name. Both come from the syntax
//! tree's own tokens (attributes, keywords, names, closing delimiters, `;`), except that a
//! field's type and a variant's discriminant are printed whole to find their last token, and an
//! impl's trait path and self type to take their text (at most [`MAX_LABEL_PART`] bytes each).
//! A printed node may hold nested symbols, whose own nodes are printed again, so that work is
//! bounded by the tokens times the bracket depth.
//!
//! Refusal over guessing: whatever the parse cannot reproduce with the server's exact answer
//! makes the whole outline `None`, and the file keeps waiting for the server —
//!
//! * text syn does not parse: rust-analyzer recovers from a syntax error with a tree no parser
//!   here reproduces; also a shebang line, syntax syn keeps only as verbatim tokens (unstable
//!   forms, a macro 2.0 definition), a trait alias, and an `extern` block (the server reports
//!   the block itself as a symbol);
//! * text syn reads differently from rust-analyzer: an item-like macro call other than
//!   `macro_rules!` (`foo! name { … }`), a field named `_`, anything named `gen` (a keyword to
//!   rust-analyzer in edition 2024), a visibility before a variant's name (syn drops it,
//!   rust-analyzer reads it as an error that detaches the variant's attributes), and an impl for
//!   `!`;
//! * more than [`MAX_TOKENS`] tokens or brackets nested deeper than [`MAX_DEPTH`] (see
//!   [`PARSE_STACK`]), more than [`MAX_SYMBOLS`] symbols, an impl whose trait path or self type
//!   is longer than [`MAX_LABEL_PART`] bytes or spans lines (its label is kept whole in every
//!   symbol path below it);
//! * unstable syntax syn parses in quadratic time — `box`, `become`, `dyn*` and `const` trait
//!   bounds, which it keeps as verbatim tokens copied again at every nesting level
//!   ([`slow_to_parse`]) — refused before the parse; and any parse slower than
//!   [`PARSE_TIMEOUT`];
//! * whitespace rustc does not accept in code (a no-break space, …), which the parser skips;
//! * a `// region:` comment, which the server reports as a symbol;
//! * comments rust-analyzer attaches to an item's node but a parser drops: comment text on the
//!   nearest non-blank line above the item's first token with no empty line in between (a `//!`
//!   inner doc line excepted, which is never attached, when no block comment sits in between),
//!   or a blank line between the
//!   item's first attribute or doc comment and its first token;
//! * two same-named same-kind siblings where either carries a `cfg`/`cfg_attr` attribute.
//!
//! The corpus under `tests/fixtures/lexical` pins this against recorded rust-analyzer answers:
//! every file there is either equal to the server path's outline or refused. The recordings come
//! from the rust-analyzer build named in `tests/fixtures/lexical/VERSION`; after a rust-analyzer
//! upgrade they are recorded again (`record.mjs`) and the corpus test decides whether the
//! outline still agrees.
//!
//! The parse runs on a short-lived thread with a stack of its own ([`PARSE_STACK`]), sized so
//! that no accepted text can exhaust it: every level the parser, the tree walk, the printing of
//! a field type, discriminant or impl header inside the walk, or the tree's drop recurses
//! consumes at least one token, so their stack grows at most linearly in the token count, and
//! the stack holds [`MAX_TOKENS`] tokens at twice the costliest growth per token measured
//! ([`STACK_PER_TOKEN`]). A parser panic refuses instead of unwinding into the caller, and
//! `proc-macro2`'s thread-local span table (which keeps the text of every source parsed on a
//! thread) is freed with the thread. The outline of a text is computed once: repeated calls for
//! the same file and bytes (a parked call retrying, name cards outlining a file per site) answer
//! from a small cache, refusals and timeouts included.
//!
//! `syn` is pinned to one release: the quadratic-parse triggers and the stack measurements hold
//! for that release only, and an upgrade repeats both.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError, mpsc};
use std::time::Duration;

use agent_ide_core::lang::brace::{line_at, source_lines};
use agent_ide_core::lang::{LanguageSupport, Outline, Symbol, SymbolKind, SyntaxVerdict};
use async_lsp::lsp_types as lsp;
use proc_macro2::{Delimiter, LineColumn, Span, TokenStream, TokenTree};
use quote::ToTokens;
use syn::visit::{self, Visit};

use crate::module_graph::blanked_code;
use crate::support::{RustSupport, attr_path};

/// Deepest `(…)`/`[…]`/`{…}` nesting accepted. Symbols nest only inside brackets, so this also
/// bounds how deep the outline itself nests — the conversion, walks and drops that recurse over
/// it run on the caller's stack.
const MAX_DEPTH: usize = 128;

/// Most tokens (`proc-macro2` token trees, a delimited group counting as one besides its
/// contents) a text may have to be parsed; a larger file waits for the server. It bounds the
/// parse's recursion; see [`PARSE_STACK`]. An optimized build takes 240 000 tokens (this
/// repository's largest file has about 90 000); an unoptimized one, whose frames are far
/// larger, 16 000.
const MAX_TOKENS: usize = if cfg!(debug_assertions) {
    16_000
} else {
    240_000
};

/// Stack bytes one token may cost the parse at worst: twice the costliest growth per token
/// measured over 49 shapes, each nested until it overflowed a known stack (nested closures,
/// prefix operators, references, pointer, tuple, array and function-pointer types, generics,
/// `dyn`/`impl` chains, qualified paths, casts, binary and assignment chains,
/// `return`/`break`/`yield`, blocks, `match` arms, `if`/`else` chains, postfix chains,
/// patterns, struct literals, nested items, attribute and macro token trees, and the printed
/// field types and discriminants): in an optimized build 4 223 bytes for nested blocks (which
/// [`MAX_DEPTH`] stops long before the token bound) and 3 116 for a `break` chain, the costliest
/// shape that can reach [`MAX_TOKENS`]; in an unoptimized one 29 051 bytes for a field whose type
/// is a chain of `&`. The `box`, `become`, `dyn*` and `const`-bound chains are refused before the
/// parse ([`slow_to_parse`]) and were not measured.
const STACK_PER_TOKEN: usize = if cfg!(debug_assertions) {
    57 << 10
} else {
    17 << 9
};

/// Stack of the parse thread, in bytes: [`MAX_TOKENS`] tokens at [`STACK_PER_TOKEN`] plus 8 MiB
/// for the fixed frames, under 2 GiB. It is address space reserved per outline, not memory:
/// pages are committed only as deep as a parse actually recurses (a few hundred KiB for ordinary
/// code; a 2 GiB reservation measured no resident memory of its own on macOS).
const PARSE_STACK: usize = (8 << 20) + MAX_TOKENS * STACK_PER_TOKEN;
const _: () = assert!(PARSE_STACK <= 2 << 30);

/// Most symbols one outline may carry; a larger file waits for the server.
const MAX_SYMBOLS: usize = 20_000;

/// Longest an impl's trait path or self type may be, in bytes, for the outline to label it:
/// the label is copied into the path of every symbol below it, so a long one would multiply.
const MAX_LABEL_PART: usize = 256;

/// Longest the parse thread may take before the outline is refused. Ordinary files take tens of
/// milliseconds; a parse still running then is abandoned, not stopped (see [`run_bounded`]).
const PARSE_TIMEOUT: Duration = Duration::from_secs(2);

/// How many outlines [`OUTLINES`] keeps.
const CACHED_OUTLINES: usize = 32;

/// Outlines already computed, oldest first, keyed by file path and the text's digest; each is
/// the answer given for that text, a refusal or a timeout included.
type Outlines = Mutex<VecDeque<(PathBuf, blake3::Hash, Option<Outline>)>>;

/// The process-wide [`Outlines`] of [`lexical_outline`], at most [`CACHED_OUTLINES`] long.
static OUTLINES: Outlines = Mutex::new(VecDeque::new());

/// Builds the lexical outline of `file` (the path the outline and its symbol paths carry) from
/// its text `source`, or `None` when the text is refused (see the module docs). A function of
/// the path and the text: no filesystem, server or subprocess. The first call for a text blocks
/// the caller while one short-lived parse thread runs — about 40 ms for a 570 KB file in an
/// optimized build, never more than [`PARSE_TIMEOUT`] — and its answer is kept (the last
/// [`CACHED_OUTLINES`] texts), so later calls for the same path and bytes return it at once
/// and a slow text never starts a second parse. Thread-safe.
pub(crate) fn lexical_outline(file: &Path, source: &str) -> Option<Outline> {
    remembered(&OUTLINES, file, source, || outline_of(file, source))
}

/// Structural verdict for one Rust text (see `LanguageSupport::syntax_verdict`): `syn` is a
/// complete Rust parser, so a text it accepts is [`SyntaxVerdict::Clean`] and a text it rejects
/// fails with the error's own 1-based line and message. Every refusal the lexical outline makes
/// for its own equality guarantee (a `// region:` comment, whitespace rustc does not accept,
/// text past the token or depth bounds, a parse slower than [`PARSE_TIMEOUT`], a thread that
/// could not start) is [`SyntaxVerdict::Unchecked`] here: those texts still edit exactly as
/// they did before this check existed, and the project check reports. Runs on the same bounded
/// parse thread as the outline; a function of the text only.
pub(crate) fn syntax_verdict(source: &str) -> SyntaxVerdict {
    if source.contains("// region:") {
        return SyntaxVerdict::Unchecked;
    }
    // A shebang line is legal Rust to rustc but not to `syn`, so no verdict is proven.
    if source.starts_with("#!") {
        return SyntaxVerdict::Unchecked;
    }
    let chars: Vec<char> = source.chars().collect();
    let code = blanked_code(&chars);
    if code
        .iter()
        .any(|&character| character.is_whitespace() && !rust_whitespace(character))
    {
        return SyntaxVerdict::Unchecked;
    }
    let source = source.to_owned();
    run_bounded(PARSE_STACK, PARSE_TIMEOUT, move || {
        let tokens: TokenStream = match source.parse() {
            Ok(tokens) => tokens,
            // The lex error's own span is 1-based over the parsed string (span-locations); its
            // Display is the generic "cannot parse string into token stream", so a delimiter
            // imbalance — the usual edit mistake — is named in plain terms instead.
            Err(error) => {
                let start = error.span().start();
                return Some(SyntaxVerdict::Failed {
                    line: start.line as u32,
                    message: unbalanced_delimiters(&source).unwrap_or_else(|| error.to_string()),
                });
            }
        };
        if !within_bounds(&tokens) {
            return Some(SyntaxVerdict::Unchecked);
        }
        Some(match syn::parse2::<syn::File>(tokens) {
            Ok(_) => SyntaxVerdict::Clean,
            Err(error) => {
                let start = error.span().start();
                SyntaxVerdict::Failed {
                    line: start.line as u32,
                    message: error.to_string(),
                }
            }
        })
    })
    .unwrap_or(SyntaxVerdict::Unchecked)
}

/// The first delimiter imbalance of `source` as a plain sentence the reply can quote: an opener
/// never closed (with the line it was opened on), a closer matching nothing, or a closer that
/// does not match the opener it met — `unbalanced delimiters: '{' opened at line 868 is never
/// closed`. One linear scan skipping strings, char literals, lifetimes, raw strings and
/// comments; `None` when the scan finds no imbalance, so the lexer's own text is kept.
fn unbalanced_delimiters(source: &str) -> Option<String> {
    /// The pairs a closer must match, indexed by its own character.
    const OPENERS: [(char, char); 3] = [(')', '('), (']', '['), ('}', '{')];
    let mut line: u32 = 1;
    let mut opened: Vec<(char, u32)> = Vec::new();
    let mut chars = source.chars().peekable();
    // Consumes the rest of a `//` line comment, counting the newline it ends with.
    let newline = |chars: &mut std::iter::Peekable<std::str::Chars>, line: &mut u32| {
        for character in chars.by_ref() {
            if character == '\n' {
                *line += 1;
                break;
            }
        }
    };
    while let Some(character) = chars.next() {
        match character {
            '\n' => line += 1,
            '/' if chars.peek() == Some(&'/') => newline(&mut chars, &mut line),
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut depth = 1;
                while let Some(character) = chars.next() {
                    match character {
                        '\n' => line += 1,
                        '*' if chars.peek() == Some(&'/') => {
                            chars.next();
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        '/' if chars.peek() == Some(&'*') => {
                            chars.next();
                            depth += 1;
                        }
                        _ => {}
                    }
                }
            }
            '"' => {
                while let Some(character) = chars.next() {
                    match character {
                        '\\' => {
                            chars.next();
                        }
                        '"' => break,
                        '\n' => line += 1,
                        _ => {}
                    }
                }
            }
            'r' if matches!(chars.peek(), Some('#' | '"')) => {
                let mut hashes = 0;
                while chars.peek() == Some(&'#') {
                    chars.next();
                    hashes += 1;
                }
                if chars.next() == Some('"') {
                    'raw: while let Some(character) = chars.next() {
                        if character == '\n' {
                            line += 1;
                        } else if character == '"' {
                            let mut matched = 0;
                            while matched < hashes && chars.peek() == Some(&'#') {
                                chars.next();
                                matched += 1;
                            }
                            if matched == hashes {
                                break 'raw;
                            }
                        }
                    }
                }
            }
            // A char literal (`'x'`, `'\n'`, `'\''`) closes within two characters of the quote;
            // anything longer is a lifetime, which opens nothing.
            '\'' => {
                if chars.peek() == Some(&'\\') {
                    chars.next();
                    chars.next();
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                    }
                } else {
                    let mut look = chars.clone();
                    if look.next().is_some_and(|character| character != '\'')
                        && look.peek() == Some(&'\'')
                    {
                        chars.next();
                        chars.next();
                    } else {
                        for character in chars.by_ref() {
                            if !character.is_alphanumeric() && character != '_' {
                                break;
                            }
                        }
                    }
                }
            }
            '(' | '[' | '{' => opened.push((character, line)),
            closer @ (')' | ']' | '}') => match opened.pop() {
                None => {
                    return Some(format!(
                        "unbalanced delimiters: '{closer}' closes nothing on line {line}"
                    ));
                }
                Some((opener, opened_at)) => {
                    let expected = OPENERS
                        .iter()
                        .find(|(close, _)| *close == closer)
                        .map(|(_, open)| *open);
                    if expected != Some(opener) {
                        return Some(format!(
                            "unbalanced delimiters: '{closer}' on line {line} does not match \
                             '{opener}' opened at line {opened_at}"
                        ));
                    }
                }
            },
            _ => {}
        }
    }
    let (opener, opened_at) = *opened.first()?;
    Some(format!(
        "unbalanced delimiters: '{opener}' opened at line {opened_at} is never closed"
    ))
}

/// The answer `outlines` keeps for `file` with the text `source`, or — when it keeps none —
/// `compute`'s, which it then keeps, dropping its oldest entry beyond [`CACHED_OUTLINES`].
/// `compute` runs without the lock held; two callers racing on the same new text both compute.
fn remembered(
    outlines: &Outlines,
    file: &Path,
    source: &str,
    compute: impl FnOnce() -> Option<Outline>,
) -> Option<Outline> {
    let digest = blake3::hash(source.as_bytes());
    let lock = || outlines.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((_, _, outline)) = lock()
        .iter()
        .find(|(path, hash, _)| *hash == digest && path == file)
    {
        return outline.clone();
    }
    let outline = compute();
    let mut kept = lock();
    if kept.len() >= CACHED_OUTLINES {
        kept.pop_front();
    }
    kept.push_back((file.to_path_buf(), digest, outline.clone()));
    outline
}

/// The lexical outline of `file` from `source`, computed (see [`lexical_outline`]).
fn outline_of(file: &Path, source: &str) -> Option<Outline> {
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
/// Everything from tokenizing to dropping the syntax tree runs on a thread of its own with a
/// [`PARSE_STACK`]-byte stack ([`run_bounded`]), after [`within_bounds`] has checked the tokens
/// against the limits that stack is sized for. A thread that cannot be started, a parser panic
/// or a parse slower than [`PARSE_TIMEOUT`] refuses.
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
    let source = source.to_owned();
    run_bounded(PARSE_STACK, PARSE_TIMEOUT, move || {
        let tokens: TokenStream = source.parse().ok()?;
        within_bounds(&tokens)
            .then(|| parsed_symbols(&source, &chars, &code, tokens))
            .flatten()
    })
}

/// Runs `work` on a new thread with a `stack`-byte stack and waits at most `timeout` for its
/// answer. `None` when the thread cannot be started, panics or has not answered in time; a late
/// thread is left to finish on its own and its answer is dropped.
// ponytail: an abandoned parse keeps its CPU and touched stack until syn returns (no way to stop
// it inside syn); the outline cache keeps the same bytes from starting a second one, but each
// distinct slow text starts its own. Parse in a killable subprocess if that ever matters.
fn run_bounded<T: Send + 'static>(
    stack: usize,
    timeout: Duration,
    work: impl FnOnce() -> Option<T> + Send + 'static,
) -> Option<T> {
    let (answer, answered) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("rust-lexical-outline".to_owned())
        .stack_size(stack)
        .spawn(move || {
            // The receiver is gone after a timeout; the late answer is dropped.
            let _ = answer.send(work());
        })
        .ok()?;
    answered.recv_timeout(timeout).ok().flatten()
}

/// Parses `tokens` (the tokens of `source`, whose characters are `chars` and code-only form
/// `code`) and walks the syntax tree into document symbols; `None` when syn does not parse them
/// or the walk refuses. Recurses as deep as the tokens nest: the caller provides the stack.
fn parsed_symbols(
    source: &str,
    chars: &[char],
    code: &[char],
    tokens: TokenStream,
) -> Option<Vec<lsp::DocumentSymbol>> {
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
    let mut symbols = walker.frames.pop()?;
    sort_by_start(&mut symbols);
    Some(symbols)
}

/// Whether `tokens` may be parsed: at most [`MAX_TOKENS`] tokens in all and groups nested at most
/// [`MAX_DEPTH`] deep (the limits [`PARSE_STACK`] is sized for), and no token that starts syntax
/// syn parses in quadratic time ([`slow_to_parse`]). One pass without recursion — the check must
/// not need the stack it protects.
fn within_bounds(tokens: &TokenStream) -> bool {
    let mut count = 0usize;
    // Each open level: its remaining tokens and the two tokens before the next one.
    let mut open = vec![(tokens.clone().into_iter().peekable(), None, None)];
    while let Some((level, earlier, previous)) = open.last_mut() {
        let Some(tree) = level.next() else {
            open.pop();
            continue;
        };
        count += 1;
        if count > MAX_TOKENS
            || slow_to_parse(earlier.as_ref(), previous.as_ref(), &tree, level.peek())
        {
            return false;
        }
        let nested = match &tree {
            TokenTree::Group(group) => Some(group.stream()),
            _ => None,
        };
        *earlier = previous.replace(tree);
        if let Some(stream) = nested {
            if open.len() > MAX_DEPTH {
                return false;
            }
            open.push((stream.into_iter().peekable(), None, None));
        }
    }
    true
}

/// Whether `tree` — after `previous` (itself after `earlier`) and before `next` in the same
/// group — starts unstable syntax syn 2.0.119 keeps as verbatim tokens: every level of it copies
/// all the tokens after it in the group again, so a chain of it takes quadratic time (a 40 000-
/// long `box` chain took 25 s). Every such chainable trigger found in syn's
/// `verbatim::between` call sites: `box` (box patterns), `become` (tail calls), `dyn*` types, and
/// `const` trait bounds — `const` right after `impl`, `dyn`, `+`, `:`, `~`, `?` or a `>` other
/// than `=>` (`for<'a> const Trait`), and a `[const]` bracket. The other call sites cover one
/// item, a fixed number of tokens or tokens inside a group of their own, which costs linear
/// time. Over-matching is harmless: a refusal only waits for the server.
fn slow_to_parse(
    earlier: Option<&TokenTree>,
    previous: Option<&TokenTree>,
    tree: &TokenTree,
    next: Option<&TokenTree>,
) -> bool {
    let punct = |token: Option<&TokenTree>, character: char| matches!(token, Some(TokenTree::Punct(punct)) if punct.as_char() == character);
    match tree {
        TokenTree::Ident(ident) if ident == "box" || ident == "become" => true,
        TokenTree::Ident(ident) if ident == "dyn" => punct(next, '*'),
        TokenTree::Ident(ident) if ident == "const" => match previous {
            Some(TokenTree::Ident(word)) => word == "impl" || word == "dyn",
            Some(TokenTree::Punct(mark)) => match mark.as_char() {
                '+' | ':' | '~' | '?' => true,
                '>' => !punct(earlier, '='),
                _ => false,
            },
            _ => false,
        },
        TokenTree::Group(group) if group.delimiter() == Delimiter::Bracket => {
            let mut inside = group.stream().into_iter();
            matches!(
                (inside.next(), inside.next()),
                (Some(TokenTree::Ident(word)), None) if word == "const"
            )
        }
        _ => false,
    }
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

/// Orders sibling symbols by where their ranges start, as rust-analyzer lists them (the syntax
/// tree walk visits some parts out of source order: a where clause before the parameters, a
/// body's inner attributes before the signature). Stable, so equal starts keep walk order.
fn sort_by_start(symbols: &mut [lsp::DocumentSymbol]) {
    symbols.sort_by_key(|symbol| (symbol.range.start.line, symbol.range.start.character));
}

/// Where one symbol's node lies in the source, as spans of three of its tokens.
struct Extent {
    /// The node's first token: its first outer attribute or doc comment, else `declaration`.
    first: Span,
    /// The first token after the outer attributes (visibility, keyword or name).
    declaration: Span,
    /// The node's last token (a closing delimiter, a `;`, or the end of a type or expression).
    last: Span,
}

impl Extent {
    /// The extent of a node whose attributes are `attrs` (outer ones first, as syn stores them),
    /// whose declaration starts with the first token of the first `head` part that has any
    /// (an absent visibility or modifier prints none), and whose last token is `last`. `None`
    /// when no head part has a token.
    fn of(attrs: &[syn::Attribute], head: &[&dyn ToTokens], last: Span) -> Option<Self> {
        let declaration = head.iter().find_map(|part| first_span(*part))?;
        let first = attrs
            .iter()
            .find(|attr| matches!(attr.style, syn::AttrStyle::Outer))
            .map_or(declaration, |attr| attr.pound_token.span);
        Some(Extent {
            first,
            declaration,
            last,
        })
    }
}

/// The parts of a function signature before its name, in order — `const`, `async`, `unsafe`,
/// `extern "abi"` and `fn` — whose first present token starts the function's declaration.
fn signature_head(sig: &syn::Signature) -> [&dyn ToTokens; 5] {
    [
        &sig.constness,
        &sig.asyncness,
        &sig.unsafety,
        &sig.abi,
        &sig.fn_token,
    ]
}

/// The span of `node`'s first token, `None` for a node without tokens. Prints the node, so it is
/// used on visibilities, modifiers, keywords, names and an impl's self type, never on an item.
fn first_span(node: &dyn ToTokens) -> Option<Span> {
    node.to_token_stream()
        .into_iter()
        .next()
        .map(|token| token.span())
}

/// The span of `node`'s last token, `None` for a node without tokens. Prints the node, so it is
/// used only on a field's type and a variant's discriminant.
fn last_span(node: &dyn ToTokens) -> Option<Span> {
    node.to_token_stream()
        .into_iter()
        .last()
        .map(|token| token.span())
}

/// Builds the document symbols of one parsed file, one visitor pass.
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
    /// Adds one symbol to the innermost frame. `extent` locates the node (`None` refuses), `name`
    /// and `kind` are rust-analyzer's, `selection` the span it selects (the name, or an impl's
    /// self type); `walk` visits the node's contents, whose symbols become its children, ordered
    /// by start. Refuses names rust-analyzer does not give (`_`, the edition-2024 keyword
    /// `gen`), tokens without a source position, and a node the server may see differently
    /// ([`Walker::check_start`]).
    fn symbol(
        &mut self,
        extent: Option<Extent>,
        name: String,
        kind: lsp::SymbolKind,
        selection: Span,
        walk: impl FnOnce(&mut Self),
    ) {
        let Some(Extent {
            first,
            declaration,
            last,
        }) = extent
        else {
            self.refused = true;
            return;
        };
        // A parsed token covers at least one character; a synthesized one covers none.
        if first.byte_range().is_empty()
            || last.byte_range().is_empty()
            || name == "_"
            || name == "gen"
        {
            self.refused = true;
            return;
        }
        let start = first.start();
        self.check_start(start, declaration.start().line);
        self.symbols += 1;
        self.refused |= self.symbols > MAX_SYMBOLS;
        self.frames.push(Vec::new());
        walk(self);
        let mut children = self.frames.pop().unwrap_or_default();
        sort_by_start(&mut children);
        #[allow(deprecated)] // `deprecated` is a required field of the LSP type.
        let symbol = lsp::DocumentSymbol {
            name,
            detail: None,
            kind,
            tags: None,
            deprecated: None,
            range: lsp::Range::new(position(start), position(last.end())),
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

    /// Refuses when the server's node may start elsewhere than `start`, the node's first token
    /// (its first attribute, doc comment or declaration token, on whose line `declaration` the
    /// declaration starts):
    ///
    /// * comment text sits on the nearest non-blank line above `start`, between the last code
    ///   before the item and the item, with no empty line (`\n\n`) after it — rust-analyzer
    ///   attaches such comments — unless that line
    ///   is a `//!` inner doc line (never attached, nor anything above it) and no block comment
    ///   opens or closes in between (a `//!` inside a block comment is no doc line);
    /// * a blank line separates `start` from the declaration (rust-analyzer attaches a doc
    ///   comment across one only when no plain comment sits between).
    fn check_start(&mut self, start: LineColumn, declaration: usize) {
        let line_start = self.line_starts[start.line - 1];
        // Just past the last code character before the item (comments and literal contents are
        // blanked in `code`).
        let gap = self.code[..line_start + start.column]
            .iter()
            .rposition(|&character| !rust_whitespace(character))
            .map_or(0, |at| at + 1);
        let above: String = self.chars[gap.min(line_start)..line_start].iter().collect();
        let inner_doc =
            |line: &str| line.starts_with("//!") && !above.contains("/*") && !above.contains("*/");
        // rust-analyzer stops attaching at whitespace holding an empty line (`\n\n`, so a line of
        // spaces does not stop it); only an outer doc comment continues past one, and syn already
        // counts those in `start`.
        let code_above = above.trim_end_matches(rust_whitespace);
        let separated = above[code_above.len()..].contains("\n\n");
        let commented = !separated
            && above
                .lines()
                .map(str::trim)
                .rfind(|line| !line.is_empty())
                .is_some_and(|nearest| !inner_doc(nearest));
        let blank = (start.line..declaration)
            .any(|line| line_at(&self.lines, line as u32).trim().is_empty());
        self.refused |= commented || blank;
    }

    /// The verbatim source text of `node`, from its first token to its last, as rust-analyzer's
    /// syntax text, for an impl label; `None` for a node without tokens or without a source
    /// position, and for a text that spans lines or runs longer than [`MAX_LABEL_PART`] bytes.
    /// Prints the node: used on an impl's trait path and self type only, after their length was
    /// bounded.
    fn text(&self, node: &dyn ToTokens) -> Option<String> {
        let mut tokens = node.to_token_stream().into_iter();
        let first = tokens.next()?.span();
        let last = tokens.last().map_or(first, |token| token.span());
        self.source
            .get(first.byte_range().start..last.byte_range().end)
            .filter(|text| !text.is_empty() && text.len() <= MAX_LABEL_PART && !text.contains('\n'))
            .map(str::to_owned)
    }

    /// Adds a function or method symbol located by `extent`, with signature `sig`, named and
    /// selected by the function's name: rust-analyzer's `METHOD` when it takes `self`,
    /// `FUNCTION` otherwise (the conversion refines both by owner and test attribute alike).
    /// `walk` visits the function's contents, as in [`Walker::symbol`].
    fn function(
        &mut self,
        extent: Option<Extent>,
        sig: &syn::Signature,
        walk: impl FnOnce(&mut Self),
    ) {
        let kind = if sig.receiver().is_some() {
            lsp::SymbolKind::METHOD
        } else {
            lsp::SymbolKind::FUNCTION
        };
        self.symbol(extent, sig.ident.to_string(), kind, sig.ident.span(), walk);
    }

    /// Adds a constant symbol located by `extent`, named and selected by `ident`, whose contents
    /// `walk` visits; for `const _` there is no symbol and `walk` runs in the enclosing one, so
    /// the items of the initializer become its children.
    fn constant(
        &mut self,
        extent: Option<Extent>,
        ident: &syn::Ident,
        walk: impl FnOnce(&mut Self),
    ) {
        if ident == "_" {
            walk(self);
        } else {
            let name = ident.to_string();
            self.symbol(extent, name, lsp::SymbolKind::CONSTANT, ident.span(), walk);
        }
    }

    /// Adds a symbol located by `extent`, named and selected by `ident`, with rust-analyzer's
    /// `kind`; `walk` visits its contents, as in [`Walker::symbol`].
    fn named(
        &mut self,
        extent: Option<Extent>,
        ident: &syn::Ident,
        kind: lsp::SymbolKind,
        walk: impl FnOnce(&mut Self),
    ) {
        self.symbol(extent, ident.to_string(), kind, ident.span(), walk);
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

    /// A function symbol, from its attributes to its body's closing brace.
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        let last = node.block.brace_token.span.close();
        let [constness, asyncness, unsafety, abi, keyword] = signature_head(&node.sig);
        let head = [
            &node.vis as &dyn ToTokens,
            constness,
            asyncness,
            unsafety,
            abi,
            keyword,
        ];
        let extent = Extent::of(&node.attrs, &head, last);
        self.function(extent, &node.sig, |walker| {
            visit::visit_item_fn(walker, node)
        });
    }

    /// A method symbol of an impl.
    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        let last = node.block.brace_token.span.close();
        let [constness, asyncness, unsafety, abi, keyword] = signature_head(&node.sig);
        let head = [
            &node.vis as &dyn ToTokens,
            &node.defaultness,
            constness,
            asyncness,
            unsafety,
            abi,
            keyword,
        ];
        let extent = Extent::of(&node.attrs, &head, last);
        self.function(extent, &node.sig, |walker| {
            visit::visit_impl_item_fn(walker, node)
        });
    }

    /// A method symbol of a trait, ending at its default body or its `;`.
    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        let last = match (&node.default, &node.semi_token) {
            (Some(block), _) => Some(block.brace_token.span.close()),
            (None, Some(semi)) => Some(semi.span),
            (None, None) => None,
        };
        let head = signature_head(&node.sig);
        let extent = last.and_then(|last| Extent::of(&node.attrs, &head, last));
        self.function(extent, &node.sig, |walker| {
            visit::visit_trait_item_fn(walker, node)
        });
    }

    /// A struct symbol; its named fields are its children.
    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
        let last = match (&node.semi_token, &node.fields) {
            (Some(semi), _) => Some(semi.span),
            (None, syn::Fields::Named(fields)) => Some(fields.brace_token.span.close()),
            (None, _) => None,
        };
        let head: [&dyn ToTokens; 2] = [&node.vis, &node.struct_token];
        let extent = last.and_then(|last| Extent::of(&node.attrs, &head, last));
        let kind = lsp::SymbolKind::STRUCT;
        self.named(extent, &node.ident, kind, |walker| {
            visit::visit_item_struct(walker, node)
        });
    }

    /// A union symbol, which rust-analyzer reports as a struct.
    fn visit_item_union(&mut self, node: &'ast syn::ItemUnion) {
        let last = node.fields.brace_token.span.close();
        let extent = Extent::of(&node.attrs, &[&node.vis, &node.union_token], last);
        let kind = lsp::SymbolKind::STRUCT;
        self.named(extent, &node.ident, kind, |walker| {
            visit::visit_item_union(walker, node)
        });
    }

    /// An enum symbol; its variants are its children.
    fn visit_item_enum(&mut self, node: &'ast syn::ItemEnum) {
        let last = node.brace_token.span.close();
        let extent = Extent::of(&node.attrs, &[&node.vis, &node.enum_token], last);
        let kind = lsp::SymbolKind::ENUM;
        self.named(extent, &node.ident, kind, |walker| {
            visit::visit_item_enum(walker, node)
        });
    }

    /// An enum variant symbol, ending at its discriminant, its fields or its name; a record
    /// variant's fields are its children. A visibility before a variant's name (rejected by
    /// rustc) refuses: syn drops it, rust-analyzer reads it as an error outside the variant, whose
    /// node then starts at the name without the attributes and comments above; it shows as code
    /// other than `{`, `,` or an attribute's `]` right before the name.
    fn visit_variant(&mut self, node: &'ast syn::Variant) {
        let name = node.ident.span().start();
        let before = self.code[..self.line_starts[name.line - 1] + name.column]
            .iter()
            .rposition(|&character| !rust_whitespace(character));
        if before.is_some_and(|at| !matches!(self.code[at], '{' | ',' | ']')) {
            self.refused = true;
            return;
        }
        let last = match (&node.discriminant, &node.fields) {
            (Some((_, discriminant)), _) => last_span(discriminant),
            (None, syn::Fields::Named(fields)) => Some(fields.brace_token.span.close()),
            (None, syn::Fields::Unnamed(fields)) => Some(fields.paren_token.span.close()),
            (None, syn::Fields::Unit) => Some(node.ident.span()),
        };
        let extent = last.and_then(|last| Extent::of(&node.attrs, &[&node.ident], last));
        let kind = lsp::SymbolKind::ENUM_MEMBER;
        self.named(extent, &node.ident, kind, |walker| {
            visit::visit_variant(walker, node)
        });
    }

    /// A named field symbol, ending at its type; a tuple field is no symbol (its type is still
    /// walked).
    fn visit_field(&mut self, node: &'ast syn::Field) {
        match &node.ident {
            Some(ident) => {
                let head: [&dyn ToTokens; 2] = [&node.vis, ident];
                let extent =
                    last_span(&node.ty).and_then(|last| Extent::of(&node.attrs, &head, last));
                self.named(extent, ident, lsp::SymbolKind::FIELD, |walker| {
                    visit::visit_field(walker, node)
                })
            }
            None => visit::visit_field(self, node),
        }
    }

    /// A trait symbol, which rust-analyzer reports as an interface.
    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        let last = node.brace_token.span.close();
        let head: [&dyn ToTokens; 4] = [
            &node.vis,
            &node.unsafety,
            &node.auto_token,
            &node.trait_token,
        ];
        let extent = Extent::of(&node.attrs, &head, last);
        let kind = lsp::SymbolKind::INTERFACE;
        self.named(extent, &node.ident, kind, |walker| {
            visit::visit_item_trait(walker, node)
        });
    }

    /// A module symbol, inline (ending at its brace) or `mod name;`.
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        let last = match (&node.content, &node.semi) {
            (Some((brace, _)), _) => Some(brace.span.close()),
            (None, Some(semi)) => Some(semi.span),
            (None, None) => None,
        };
        let head: [&dyn ToTokens; 3] = [&node.vis, &node.unsafety, &node.mod_token];
        let extent = last.and_then(|last| Extent::of(&node.attrs, &head, last));
        let kind = lsp::SymbolKind::MODULE;
        self.named(extent, &node.ident, kind, |walker| {
            visit::visit_item_mod(walker, node)
        });
    }

    /// A type alias symbol, which rust-analyzer reports as a type parameter.
    fn visit_item_type(&mut self, node: &'ast syn::ItemType) {
        let last = node.semi_token.span;
        let extent = Extent::of(&node.attrs, &[&node.vis, &node.type_token], last);
        let kind = lsp::SymbolKind::TYPE_PARAMETER;
        self.named(extent, &node.ident, kind, |walker| {
            visit::visit_item_type(walker, node)
        });
    }

    /// An associated type symbol of an impl.
    fn visit_impl_item_type(&mut self, node: &'ast syn::ImplItemType) {
        let last = node.semi_token.span;
        let head: [&dyn ToTokens; 3] = [&node.vis, &node.defaultness, &node.type_token];
        let extent = Extent::of(&node.attrs, &head, last);
        let kind = lsp::SymbolKind::TYPE_PARAMETER;
        self.named(extent, &node.ident, kind, |walker| {
            visit::visit_impl_item_type(walker, node)
        });
    }

    /// An associated type symbol of a trait.
    fn visit_trait_item_type(&mut self, node: &'ast syn::TraitItemType) {
        let last = node.semi_token.span;
        let extent = Extent::of(&node.attrs, &[&node.type_token], last);
        let kind = lsp::SymbolKind::TYPE_PARAMETER;
        self.named(extent, &node.ident, kind, |walker| {
            visit::visit_trait_item_type(walker, node)
        });
    }

    /// A constant symbol (none for `const _`).
    fn visit_item_const(&mut self, node: &'ast syn::ItemConst) {
        let last = node.semi_token.span;
        let extent = Extent::of(&node.attrs, &[&node.vis, &node.const_token], last);
        self.constant(extent, &node.ident, |walker| {
            visit::visit_item_const(walker, node)
        });
    }

    /// An associated constant symbol of an impl (none for `const _`).
    fn visit_impl_item_const(&mut self, node: &'ast syn::ImplItemConst) {
        let last = node.semi_token.span;
        let head: [&dyn ToTokens; 3] = [&node.vis, &node.defaultness, &node.const_token];
        let extent = Extent::of(&node.attrs, &head, last);
        self.constant(extent, &node.ident, |walker| {
            visit::visit_impl_item_const(walker, node)
        });
    }

    /// An associated constant symbol of a trait.
    fn visit_trait_item_const(&mut self, node: &'ast syn::TraitItemConst) {
        let last = node.semi_token.span;
        let extent = Extent::of(&node.attrs, &[&node.const_token], last);
        self.constant(extent, &node.ident, |walker| {
            visit::visit_trait_item_const(walker, node)
        });
    }

    /// A static symbol, which rust-analyzer reports as a constant.
    fn visit_item_static(&mut self, node: &'ast syn::ItemStatic) {
        let last = node.semi_token.span;
        let extent = Extent::of(&node.attrs, &[&node.vis, &node.static_token], last);
        let kind = lsp::SymbolKind::CONSTANT;
        self.named(extent, &node.ident, kind, |walker| {
            visit::visit_item_static(walker, node)
        });
    }

    /// A `macro_rules!` definition symbol, which rust-analyzer reports as a function, ending at
    /// its closing delimiter or `;`; a macro invocation at item position is no symbol. Any other
    /// item-like call with a name (`foo! name { … }`, `r#macro_rules! name { … }`) refuses:
    /// rust-analyzer makes a definition only of the `macro_rules` keyword.
    fn visit_item_macro(&mut self, node: &'ast syn::ItemMacro) {
        let Some(ident) = &node.ident else {
            visit::visit_item_macro(self, node);
            return;
        };
        if !node.mac.path.is_ident("macro_rules") {
            self.refused = true;
            return;
        }
        let last = node
            .semi_token
            .as_ref()
            .map_or(node.mac.delimiter.span().close(), |semi| semi.span);
        let extent = Extent::of(&node.attrs, &[&node.mac.path], last);
        self.named(extent, ident, lsp::SymbolKind::FUNCTION, |walker| {
            visit::visit_item_macro(walker, node)
        });
    }

    /// An impl symbol labelled as rust-analyzer labels it, `impl Type` or
    /// `impl Trait for Type` (`!` kept before a negative impl's trait) from the verbatim text of
    /// the trait path and the self type, and selected at the self type. Refuses an impl for `!`
    /// and one whose trait path or self type spans lines or runs longer than
    /// [`MAX_LABEL_PART`] bytes — measured between the tokens around them before either is
    /// printed, so a huge one costs nothing.
    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        let bytes = |from: usize, to: Span| to.byte_range().start.saturating_sub(from);
        let self_end = node
            .generics
            .where_clause
            .as_ref()
            .map_or(node.brace_token.span.open(), |clause| {
                clause.where_token.span
            });
        let (trait_fits, self_start) = match &node.trait_ {
            Some((bang, path, for_token)) => {
                let start = bang
                    .as_ref()
                    .map(|bang| bang.span)
                    .or_else(|| path.leading_colon.as_ref().map(|colon| colon.spans[0]))
                    .or_else(|| path.segments.first().map(|segment| segment.ident.span()));
                let fits = start.is_some_and(|start| {
                    bytes(start.byte_range().start, for_token.span) <= MAX_LABEL_PART
                });
                (fits, for_token.span.byte_range().end)
            }
            None => {
                let before = node.generics.gt_token.as_ref().map(|gt| gt.span);
                (
                    true,
                    before.unwrap_or(node.impl_token.span).byte_range().end,
                )
            }
        };
        if !trait_fits
            || bytes(self_start, self_end) > MAX_LABEL_PART
            || matches!(*node.self_ty, syn::Type::Never(_))
        {
            self.refused = true;
            return;
        }
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
        let last = node.brace_token.span.close();
        let head: [&dyn ToTokens; 3] = [&node.defaultness, &node.unsafety, &node.impl_token];
        let extent = Extent::of(&node.attrs, &head, last);
        let kind = lsp::SymbolKind::OBJECT;
        self.symbol(extent, label, kind, selection, |walker| {
            visit::visit_item_impl(walker, node)
        });
    }
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
        assert!(sources.len() >= 32, "corpus: {sources:?}");
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
        assert!(exact >= 13, "{exact} exact corpus files");
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
        assert_eq!(at("State/Off/why").signature, "why: String");
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
    /// macro 2.0 definition), a trait alias, an extern block, whitespace rustc rejects,
    /// cfg-duplicated items with the same name, an impl for `!`, and impls whose self type or
    /// trait path spans lines or exceeds [`MAX_LABEL_PART`] bytes.
    #[test]
    fn unclean_sources_are_refused() {
        let long = format!("impl Tr for Foo<{}> {{}}\n", "u8, ".repeat(70));
        let long_trait = format!("impl Tr<{}> for Foo {{}}\n", "u8, ".repeat(70));
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
            "impl ! {}\n",
            "impl Tr for Foo<\n    u8,\n> {}\n",
            &long,
            &long_trait,
        ];
        for source in refused {
            assert!(
                lexical_outline(Path::new("a.rs"), source).is_none(),
                "must be refused: {source:?}"
            );
        }
        // Just within the bound, the same shape is labelled.
        let fits = format!("impl Tr for Foo<{}> {{}}\n", "u8, ".repeat(60));
        assert!(lexical_outline(Path::new("a.rs"), &fits).is_some());
    }

    /// A plain comment right above an item belongs to the server's node, so the outline refuses;
    /// one an empty line separates does not (recorded from rust-analyzer: `// c`, empty line,
    /// `/// Doc.`, `pub fn f() {}` ranges from the doc line), while a line of spaces still joins.
    #[test]
    fn a_comment_an_empty_line_separates_is_not_the_items() {
        let start = |source: &str| {
            lexical_outline(Path::new("a.rs"), source).map(|outline| outline.symbols[0].range.start)
        };
        assert_eq!(
            start("use std::fmt;\n// tag\n\n/// Doc.\npub fn f() {}\n"),
            Some(4)
        );
        assert_eq!(start("// tag\n\npub fn f() {}\n"), Some(3));
        assert_eq!(start("// tag\n/// Doc.\npub fn f() {}\n"), None);
        assert_eq!(start("// tag\n    \n/// Doc.\npub fn f() {}\n"), None);
        assert_eq!(start("/* tag */\n\npub fn f() {}\n"), Some(3));
    }

    /// Unstable syntax syn parses in quadratic time is refused before the parse, at the full
    /// [`MAX_TOKENS`] and well inside [`PARSE_TIMEOUT`]: `box` patterns, `become` chains,
    /// `dyn*` types, and `const` / `[const]` trait bounds.
    #[test]
    fn quadratic_syntax_is_refused_before_the_parse() {
        // Each text stays within the token bound, so its trigger is what refuses it.
        let levels = |tokens_per_level: usize| MAX_TOKENS / tokens_per_level - 16;
        let chain = |open: &str, close: &str, per_level: usize| {
            let depth = levels(per_level);
            format!(
                "fn f() -> {}u8{} {{}}\n",
                open.repeat(depth),
                close.repeat(depth)
            )
        };
        let sources = [
            format!("fn f() {{ let {}x = 0; }}\n", "box ".repeat(levels(1))),
            format!("fn f() {{ {}x; }}\n", "become ".repeat(levels(1))),
            chain("Box<dyn* Tr<", ">>", 8),
            chain("impl const Tr<", ">", 5),
            chain("impl [const] Tr<", ">", 6),
            chain("impl ?Sized + const Tr<", ">", 8),
        ];
        for source in &sources {
            let started = std::time::Instant::now();
            assert!(lexical_outline(Path::new("a.rs"), source).is_none());
            let elapsed = started.elapsed();
            assert!(
                elapsed < PARSE_TIMEOUT / 4,
                "{elapsed:?} for {}",
                &source[..40]
            );
        }
    }

    /// An impl whose self type holds a long text is refused before its label is printed, well
    /// inside [`PARSE_TIMEOUT`]: 100 impls nested in each other's const generic argument around
    /// a 1 MiB string would otherwise make 100 labels of a megabyte each. The same shape two
    /// levels deep around a short string is labelled.
    #[test]
    fn a_huge_impl_header_is_refused_before_it_is_printed() {
        let nest = |levels: usize, text: usize| {
            let mut source = format!("\"{}\";", "x".repeat(text));
            for _ in 0..levels {
                source = format!("impl T for A<{{ {source} 0 }}> {{}}");
            }
            source
        };
        assert!(lexical_outline(Path::new("a.rs"), &nest(2, 1)).is_some());
        let started = std::time::Instant::now();
        assert!(lexical_outline(Path::new("a.rs"), &nest(100, 1 << 20)).is_none());
        let elapsed = started.elapsed();
        assert!(elapsed < PARSE_TIMEOUT / 2, "{elapsed:?}");
    }

    /// A text is outlined once per file and bytes: a second call answers from the cache, a
    /// refusal included, and a changed text or another file computes again.
    #[test]
    fn an_unchanged_text_is_outlined_once() {
        let outlines: Outlines = Mutex::new(VecDeque::new());
        let computed = std::cell::Cell::new(0);
        let outline = |file: &str, source: &str, answer: Option<Outline>| {
            remembered(&outlines, Path::new(file), source, || {
                computed.set(computed.get() + 1);
                answer
            })
        };
        let some = lexical_outline(Path::new("a.rs"), "fn once() {}\n");
        assert!(some.is_some());
        assert_eq!(outline("a.rs", "fn once() {}\n", some.clone()), some);
        assert_eq!(outline("a.rs", "fn once() {}\n", None), some);
        assert_eq!(outline("a.rs", "fn slow() {}\n", None), None);
        assert_eq!(outline("a.rs", "fn slow() {}\n", some.clone()), None);
        assert_eq!(outline("b.rs", "fn once() {}\n", None), None);
        assert_eq!(computed.get(), 3);
    }

    /// The parse backstop answers `None` once the timeout passes, without waiting for the late
    /// work, and `None` for work that panics.
    #[test]
    fn a_slow_or_failed_parse_is_abandoned() {
        let started = std::time::Instant::now();
        let late = run_bounded(1 << 20, Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_secs(2));
            Some(1)
        });
        assert_eq!(late, None);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            run_bounded(1 << 20, Duration::from_secs(5), || Some(2)),
            Some(2)
        );
        let failed: Option<u8> = run_bounded(1 << 20, Duration::from_secs(5), || panic!("parse"));
        assert_eq!(failed, None);
    }

    /// Pathological nesting is refused before the parser recurses, from a caller whose stack is
    /// only 256 KiB: the reviews' 10 000 nested braces (too deep), 100 000 nested two-parameter
    /// closures and 900 closures each followed by a thousand `-` (both over [`MAX_TOKENS`], which
    /// no bracket or operator heuristic may reset). Ordinary nesting still outlines from that
    /// caller, because the parse runs on its own stack.
    #[test]
    fn pathological_nesting_is_refused_without_overflow() {
        let refused = [
            format!("fn f() {{{}{}}}\n", "{".repeat(10_000), "}".repeat(10_000)),
            format!("fn f() {{ let _ = {}0; }}\n", "|a, b| ".repeat(100_000)),
            format!(
                "fn f() {{ let _ = {}0; }}\n",
                format!("|a, b| {}", "-".repeat(1000)).repeat(900)
            ),
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
        assert_eq!(refused, [true; 3]);
        assert_eq!(inner, Some(LineRange::new(2, 2)));
    }

    /// The token bound is sound at its edge: a text of the costliest shape measured for this
    /// build that can reach [`MAX_TOKENS`] at all — a field typed by a chain of `&`
    /// (unoptimized) or a chain of `break` (optimized), one token per level of recursion — is
    /// parsed and outlined on the parse thread's stack at exactly [`MAX_TOKENS`] tokens, and one
    /// token more is refused. Nested blocks cost more per token in an optimized build, but
    /// brackets stop at [`MAX_DEPTH`], far below the token bound, so they cannot reach it.
    #[test]
    fn the_costliest_shape_at_the_token_bound_outlines_without_overflow() {
        // Besides the chain: `struct S {…}` and `a : … u8`, or `fn f () {…}` and
        // `loop {…}` with `1 ;`.
        let (source, others): (fn(usize) -> String, usize) = if cfg!(debug_assertions) {
            (
                |chain| format!("struct S {{ a: {}u8 }}\n", "&".repeat(chain)),
                6,
            )
        } else {
            (
                |chain| format!("fn f() {{ loop {{ {}1; }} }}\n", "break ".repeat(chain)),
                8,
            )
        };
        let outline = lexical_outline(Path::new("a.rs"), &source(MAX_TOKENS - others));
        assert_eq!(outline.map(|outline| outline.symbols.len()), Some(1));
        assert!(lexical_outline(Path::new("a.rs"), &source(MAX_TOKENS - others + 1)).is_none());
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

#[cfg(test)]
mod syntax_tests {
    use super::*;

    /// A clean text is `Clean`; a truncated item fails at its own line with syn's message; a
    /// text the outline refuses for its own equality rules stays `Unchecked`.
    #[test]
    fn syntax_verdict_cleans_fails_and_refuses() {
        assert_eq!(syntax_verdict("fn a() {}\n"), SyntaxVerdict::Clean);
        let failed = syntax_verdict("fn a() {\n");
        assert!(
            matches!(failed, SyntaxVerdict::Failed { line, .. } if line == 1 || line == 2),
            "{failed:?}"
        );
        if let SyntaxVerdict::Failed { message, .. } = failed {
            assert!(!message.is_empty());
        }
        // A shebang is legal Rust rustc accepts; no verdict is proven, so the edit proceeds.
        assert_eq!(
            syntax_verdict("#!/usr/bin/env run-crate\nfn a() {}\n"),
            SyntaxVerdict::Unchecked
        );
        // Region comments refuse the outline; they are no syntax verdict either way.
        assert_ne!(
            syntax_verdict("// region: fold\nfn a() {}\n"),
            SyntaxVerdict::Clean
        );
    }

    /// The lexer's generic `cannot parse string into token stream` becomes a plain sentence an
    /// edit refusal can quote: the delimiter that is never closed is named with the line it was
    /// opened on, a stray closer and a mismatched closer name their own lines. The verdict's own
    /// line stays the lexer span's (where parsing gave up), not the opener's.
    #[test]
    fn unbalanced_delimiters_name_the_opener_and_its_line() {
        let unclosed = "pub fn target( -> i32 {";
        assert_eq!(
            syntax_verdict(unclosed),
            SyntaxVerdict::Failed {
                line: 1,
                message: "unbalanced delimiters: '(' opened at line 1 is never closed".to_owned(),
            }
        );
        let unclosed_brace = "pub fn a() {\n    1\npub fn b() {\n    2\n}\n";
        match syntax_verdict(unclosed_brace) {
            SyntaxVerdict::Failed { line, message } => {
                assert_eq!(
                    message,
                    "unbalanced delimiters: '{' opened at line 1 is never closed"
                );
                assert!(line >= 1, "the span's line, {line}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        assert_eq!(
            unbalanced_delimiters("fn a() {\n    let s = \"}\";\n    let c = '}';\n    1\n}\n"),
            None,
            "delimiters inside strings and char literals are not counted"
        );
        assert_eq!(
            unbalanced_delimiters("fn a() { // {\n    r#\"{\"\"#\n}\n"),
            None,
            "comments and raw strings are not counted"
        );
        assert_eq!(
            unbalanced_delimiters("fn a<'a>() {\n    let lifetime: &'a str = \"x\";\n}\n"),
            None,
            "lifetimes are not char literals"
        );
        assert_eq!(
            unbalanced_delimiters("fn a() }\n"),
            Some("unbalanced delimiters: '}' closes nothing on line 1".to_owned())
        );
        assert_eq!(
            unbalanced_delimiters("fn a() {\n    (1\n}\n"),
            Some(
                "unbalanced delimiters: '}' on line 3 does not match '(' opened at line 2"
                    .to_owned()
            )
        );
    }
}
