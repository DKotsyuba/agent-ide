# Symbol address v1 contract

Revision: symbol-address r2; tracked boundary id `symbol-address-v1`, revision 2 (r2 only unifies the tracked description; the text is that of r1). Status: draft for
agreement. Provider: M-028 (`crates/agent-ide-core/src/lang/address.rs` and
`crates/agent-ide-core/src/assistance/addressing.rs`, both created by this module). Consumers: M-029
(`ide.read`, `ide.edit`: resolution of an address to a span) and M-030 (instructions, skill text and
reply formats). Vocabulary of the surrounding tools: [tools v0.4](tools-v0.4.md); batch forms:
[batch API v1](batch-api-v1.md); symbol cards: [symbol and graph item](symbol-graph-item-v1.md).

Purpose: every code location in a file has exactly one canonical text, an **address**, that every
IDE result prints next to `path:line` and every IDE input accepts. The address does not shift when
code above it changes. It names symbols, and also the file-level code that no symbol covers
(header comments, imports, top-level statements), so every line of a file belongs to exactly one
address. Plain `path`, `path` + `lines` and `path:line` remain as fallbacks.

## 1. Vocabulary

- **Unit**: a symbol (an entry of the language's [`Outline`](../../crates/agent-ide-core/src/lang/mod.rs))
  or a **region** (§5). The units of a file form a tree: symbols nest by their children, regions
  exist only at file level.
- **Span** of a unit: the inclusive 1-based line range `file#unit` denotes. For a symbol it is the
  outline's `range`, which already starts at the header (doc comments, attributes, decorators,
  JSDoc) and, for merged TypeScript overloads, covers the whole group. For a region it is its first
  to its last line.
- **Own lines** of a unit: the lines of its span that no child unit's span covers.
- **Symbol address**: names a unit (`src/a.rs#Store/get`). **Position address**: names one line of a
  unit (`src/a.rs#Store/get:7`). Rows that name a definition print a symbol address; rows that name
  a position print a position address.

## 2. Grammar

```text
address   = file "#" path [ ":" window ]       ; unit or position, file known
          | path [ ":" window ]                ; no file: a name lookup (§7.3)
          | file ":" line [ ":" column ]       ; location form: what grep and compilers print
file      = relative path, no "..", as today
path      = segment { "/" segment }
segment   = name [ "~" ordinal ]               ; a symbol, ordinal >= 2
          | "@" region [ "~" ordinal ]         ; a region, first segment only (§5)
window    = line [ "-" [ line ] ]              ; relative to the unit's first line, 1-based, inclusive; "N-" runs to the unit's last line
line      = digit1 { digit }
```

- `file#` with an empty path names the whole file (unchanged).
- The location form is read when the text has no `#`, ends in `:<digits>` or `:<digits>:<digits>`,
  and the part before the colon contains a `.` or a `/`. Anything else without `#` is a name lookup.
  `src/a.rs:120:15` ignores the column. A location resolves to the position address of its line (§7.2).
- A segment is the unit's name with whitespace runs collapsed to one space and trimmed; the
  language normalizers already produce that form. Names may contain spaces and any other
  character. Segments are written with the escapes `%2F` for `/`, `%25` for `%`, `%7E` for `~`
  and `%3A` for `:`, applied only where the raw character would change the parse: `/` always, `%`
  only when the next two characters are `25`, `2F`, `7E` or `3A` (any case), `~` only when it
  starts a trailing `~<digits>`, `:` only when it starts a trailing `:<digits>[-<digits>]`. A
  decoder maps the four escapes anywhere; every other `%` is literal. Encoding followed by
  decoding is the identity.
- In a one-line row the address ends at the first run of two spaces or at the end of the line.
- Inputs accepted for compatibility: `file#Owner/name`, `#Owner/name`, a bare `name`, and the
  sigil addresses of the name index (`.btn`, `##main`, `--brand`), all with the old meaning.
  The old unescaped `/` inside a name stays unresolvable, as before.

## 3. Line partition (the totality invariant)

For a file with `L` lines (`lang::line_count`), the units cover every line `1..=L`, and **every line
has exactly one innermost unit**: the deepest unit whose span contains it. At equal depth a later
start wins and, at equal start, the shorter span. The line's **position address** is that unit's
address plus `:N`, where `N = line - span.start + 1`. Hence:

- doc comment and attribute lines of a symbol are lines `1..k` of that symbol;
- a line of a container that no member covers (the `impl` line, a `}`, a comment between members, a
  `use` inside `mod tests`) is a line of the container;
- file-level lines no symbol covers belong to regions (§5), which tile the gaps between top-level
  symbols completely, blank and comment-only lines included;
- an empty file has no units; a line beyond `L` has no address.

`N` counts from the first line of the span as `ide.read` of the symbol prints it (M-029 numbers that
read from 1). It is **relative**, so an edit above the symbol or in another symbol never changes
it.

## 4. Canonical form and uniqueness

The raw path of a symbol is its names from the outline root, `Owner/…/name`. Several units can share
a raw path (a type and its `impl` blocks, cfg variants, redefinitions, a field and a method of the
same name, `impl Foo<A>` and `impl Foo<B>` both defining `new`). The canonical address of the
`k`-th unit with a given raw path (pre-order, file order) is the raw path for `k = 1` and the raw
path with `~k` appended to its last segment for `k >= 2`. The ordering inside a group of equal raw
paths is: declarations first, then `impl` blocks, then fields, each in file order. Consequences:

- `Foo` is the type, `Foo~2` the first `impl Foo` block, `Foo/new` the first method named `new`
  in any block, `Foo/new~2` the second. Members are never prefixed with their block's ordinal;
  an address that was unique before stays byte-identical.
- Rust inherent impls are named by the bare type (`impl<T> Foo<T>` is `Foo`); trait and generic
  trait impls keep the normalized label (`impl Display for Foo`, `impl<T> From<T> for Foo`), capped
  with a digest as today (`lang-rust` `impl_segment`). Python methods are `Class/method`,
  functions nested in functions `outer/inner`; a property setter that reuses the getter's name is
  `name~2`. TypeScript overloads are one unit (the group), accessor pairs merge, test callbacks are
  `describe("suite") callback/it("case") callback`. CSS rules are their selector, at-rules are
  containers (`styles.css#@media (min-width: 600px)/.card`), HTML elements are `tag#id` or
  `tag.class`.
- Resolution is the inverse: collect the units whose names match the segments (descending with
  backtracking through same-named siblings, as `Outline::find` does today), order them as above and
  take the `k`-th. A segment that contains `~<digits>` literally is tried by its literal name first.
- Canonical addresses are produced by `AddressMap`; no other code formats an address.

## 5. Regions

A region is a file-level unit with a name starting with `@`. The vocabulary is closed:

| Region | Lines |
|---|---|
| `@header` | The file's leading lines: blank lines, comments and the language's header statements (below), up to the first import, symbol or code block. Empty when the file starts with one of those. |
| `@imports` | A run of import statements, from the first to the last import line (blank and comment lines between two imports belong to the run). A second run is `@imports~2`. |
| `@main` | Python: the `if __name__ == "__main__":` block, from its `if` line to its last body line. |
| `@code/<leader>` | A top-level statement block that is neither a symbol nor an import (Python statements, Rust macro invocations, TypeScript expression statements). `<leader>` names the block (below). |
| `@after/<prev>` | The blank and comment-only lines between two units, or after the last one, where `<prev>` is the segment text of the unit above (`@after/Store`, `@after/@imports`). |

Constants, statics, type aliases, enums, mod declarations and module-level variables are **ordinary
symbols** wherever the outline reports them (Rust `const`, `static`, `type`, `mod x;`; Python
module variables; TypeScript `const`, `let`, `var`, `type`, `enum`), not regions.

`<leader>`: the code of the block's first line (a Rust attribute line or a Python decorator line is
skipped), comments removed, cut before the first `(`, `{`, `[`, `=`, `;`, `,` or a `:` that is not
part of `::`, trimmed, whitespace collapsed, clipped to 32 characters; `stmt` when nothing is left.
`sys.path.insert(0, root)` is `@code/sys.path.insert`, `thread_local! {` is
`@code/thread_local!`, `if TYPE_CHECKING:` is `@code/if TYPE_CHECKING`, `module.exports = {` is
`@code/module.exports`. Equal canonical names get `~k` as in §4.

Block extent (a deterministic line scan, not a parse; strings and comments are skipped when counting
brackets): a block starts at the first line of a gap that is not trivia (blank or comment-only), not
a header statement and not an import, and ends at the first line where bracket depth is 0 and the
line ends in `;` or `}` or the next non-blank line starts in column 0 with something other than a
continuation (`.`, `?`, `:`, `&&`, `||`, `)`, `]`, `}`, `else`, `elif`, `except`, `finally`,
`catch`). In an indentation language (Python) a block also continues while the next non-blank line
is indented deeper than its first line. Attribute lines (`#[…]`) and decorators directly above a
block start it. A symbol span inside a block splits it into fragments; equal names get `~k`.

Per language (`LanguageSupport::region_rules`, static data in each language crate; a registered
language without rules gets the generic rules: `//` and `/* */` comments, no imports):

| | Header statements | Import statements |
|---|---|---|
| Rust | `//!` and `/*! */`, inner attributes `#![…]`, a `#!` shebang, `//` comments | `use`, `pub use`, `pub(…) use`, `extern crate`, with their attributes |
| Python | shebang and coding comments, `#` comments, the module docstring | `import x`, `from x import y` (parenthesised continuations included) |
| TypeScript/JavaScript | shebang, `//`, `/* */`, `/// <reference …>`, directive prologue (`"use strict"`) | `import …`, `import "x"`, `import x = require("y")`, `export … from "y"` |
| CSS, SCSS, Sass, LESS | `/* */`, `//`, `@charset` | `@import`, `@use`, `@forward` |
| HTML | `<!doctype>`, `<!-- -->` comments | none |

HTML has no statement structure outside its outlined elements: its `@code/<tag>` blocks are runs of
non-blank lines between outlined elements, named by their first tag. Files no registered language
reads have no addresses; `path`, `lines` and `path:line` remain their only forms.

## 6. Line windows

- `file#unit:N` is the one line `N`; `file#unit:N-M` is lines `N..=M`; `file#unit:N-` is lines `N..`
  to the unit's last line; all relative to the first line of the unit's span. `1 <= N <= M`.
  `Window.last` is `Option<u32>` (`None` for `N-`), and `Display` prints the form it parsed.
- `M` beyond the span is clamped to its last line and the result says so. `N` beyond the span is
  `LineOutOfRange { length }`; the caller decides to clamp. A window never leaves its unit.
- A position address printed by any result always carries a single `:N`.

## 7. Resolver API (Rust, `agent_ide_core::lang::address`)

Names are fixed when the foundation lands; the semantics below are the contract.

### 7.1 Types

```rust
pub struct Address { /* file: Option<PathBuf>, segments: Vec<String> (decoded), window: Option<Window> */ }
pub struct Window { pub first: u32, pub last: Option<u32> }    // relative, 1-based, inclusive; None = to the unit end
pub enum Parsed { Address(Address), Location { file: PathBuf, line: u32, column: Option<u32> } }
impl Address { pub fn parse(text: &str) -> Result<Parsed, AddressError>; /* Display = canonical text */ }

pub enum UnitKind { Symbol(SymbolKind), Region(RegionKind) }
pub enum RegionKind { Header, Imports, Main, Code, After }
pub struct Unit {
    pub address: Address,          // canonical, no window
    pub kind: UnitKind,
    pub span: LineRange,           // §1
    pub depth: u8,                 // 0 for file level
    pub symbol: Option<usize>,     // pre-order index into the outline's symbols, for insert_site and signatures
    pub signature: String, pub doc: Option<String>,
}
pub struct Resolved { pub unit: usize, pub span: LineRange, pub window: Option<LineRange>,
                      pub bytes: Range<usize>, pub clamped: bool }  // absolute lines and byte offsets
pub struct Located { pub unit: usize, pub address: Address }         // position address, window = {n, Some(n)}
pub enum ResolveError { UnknownUnit { near: Vec<usize> }, LineOutOfRange { length: u32 }, NoFile }
```

### 7.2 `AddressMap`

```rust
impl AddressMap {
    /// Pure function of the outline and the exact source text it was computed from; reads the
    /// language's region rules from `outline.language`. Never fails: a file always gets a total partition.
    pub fn build(outline: &Outline, source: &str) -> AddressMap;
    pub fn units(&self) -> &[Unit];                       // file order, pre-order
    pub fn resolve(&self, address: &Address) -> Result<Resolved, ResolveError>;   // address -> lines and bytes
    pub fn at_line(&self, line: u32) -> Option<Located>;  // path:line -> enclosing position address (§3)
    pub fn named(&self, name: &str) -> Vec<usize>;        // §7.3
    pub fn near(&self, address: &Address) -> Vec<usize>;  // at most 5 units: same owner, or same last segment elsewhere
    pub fn imports_insertion_line(&self) -> u32;          // where a new import belongs: after the last import, else after @header, else 1
}
```

`resolve` accepts every compatibility input of §2 whose file is this map's file and returns
`UnknownUnit` with `near` otherwise. `bytes` is the byte range of `span` or `window` in the source
(line terminators included), so a caller can splice without recomputing lines. `at_line` of a line
inside a symbol's header, a container's own lines or a gap always answers (§3); it answers `None`
only beyond the last line.

### 7.3 Name lookup

`named(name)` returns the units whose last segment equals `name` (any depth), file order, **with the
declaration-and-impl collapse of symbol-graph-item r2 §2**: an `impl` unit is dropped when a
non-`impl` unit shares its raw path. Regions are never named. The tools merge the lists of every
file and answer §8's candidate list when there are two or more; `ambiguous_symbol` is no longer
produced for a bare name.

### 7.4 Worker glue (`assistance/addressing.rs`)

- `address_map(job, observed, bytes) -> Result<(Arc<AddressMap>, Option<Lexical>), FailureCode>`:
  `outline_of` followed by `AddressMap::build`, cached per binding by file, exact source revision
  and outline origin. The failure codes are `outline_of`'s.
- `position_addresses(job, &[(file, line)]) -> Vec<Option<String>>`: best effort for result
  rendering. It outlines each distinct file through the existing `scanned_outline`, never waits for
  a loading server beyond the lexical/source outline, spends at most 1.5 s and 16 files per reply,
  and answers `None` for a location it did not resolve. A result then prints that location without
  an address and appends one line `addresses: omitted for N locations (<reason>)`; it never prints a
  guessed address.

## 8. What results print

The primary content of each row is fixed here; M-030's reply-format contract may change the framing
around it, abbreviations in dense lists only where it states the rule, and layout per host, but a
row that names a location keeps its full address verbatim, as a single copyable text.

| Result | Row |
|---|---|
| `ide.outline` | one row per unit in file order, regions included: `<first>–<last>  <address>  <signature>  // <doc>` (regions have no signature; `@after/…` rows appear only when they hold a non-blank line; collapsed tests keep `[N tests collapsed]`); header `<file>  (<L> lines, <language>)`; footer counts symbols and regions. `kinds` filtering hides regions unless the list contains `region`. |
| Bare-name search (`ide.symbol`, `ide.graph`, batch entries) | `<N> candidates for <name>:` then one row per candidate `<symbol address>  <kind>  <signature>  (lines a–b)`, at most 20 and the rest behind the reply's `detail_ref`. Not an error. A single candidate answers the card as today. |
| `unknown_symbol` | adds `near:` with at most 5 symbol addresses (§7.2 `near`). |
| Compiler and checker diagnostics, post-edit diagnostics, problems | `<path>:<line>:<col> <position address> <severity> [<code>] <message>`; the 256-byte bound of a diagnostic line applies to the whole row and clips the message first. |
| Failing tests | `FAIL <runner name>  <test symbol address>` when the runner name maps to a test unit of a file in the run, and the assertion site `<path>:<line>  <position address>  <message>`. |
| Callers, callees, graph nodes | the node's symbol address in place of the raw outline path (canonical, regions possible for module-level callers) beside `file:line`; a node that is a use site prints a position address. |
| Usages | `<path>:<line>  <position address>  <text>` (M-030 may elide the repeated file in this dense list). |
| Diff | one line before each hunk's patch: `@ <position address of the hunk's first changed line in the changed file>`; a deleted file prints `@ <path> (deleted)`; the patch bytes stay exact. |
| `ide.symbol` / `ide.graph` / `ide.read` / `ide.edit` input | every form of §2; a location resolves to its enclosing unit and the reply echoes the canonical address it used. |

## 9. Stability after edits

1. An address is a function of the file text; nothing is stored. Server and lexical outlines yield
   the same addresses (the `outline_while_loading` equality obligation).
2. A symbol address changes only by renaming or moving the symbol, by a change of its `~k` ordinal
   (an identical raw path appears or disappears above it, or its declaration/impl/field ordering
   changes), or when a region's leader or ordinal changes.
3. `:N` changes only when a line is inserted or removed in the same unit above `N`. An edit that
   keeps a unit's line count keeps all its `:N`. Edits in other units never move it.
4. Replies that apply an edit (M-029) print the post-edit address of every changed unit; a stale
   address is never repaired silently. An address that no longer resolves is `unknown_symbol` with
   `near`, not a guess.
5. A unit's span is the same for read and edit, so `file#unit` replaced by itself is the identity.

## 10. Compatibility and changed behaviour

- `ide.outline` rows change as in §8 (gutter line numbers become `first–last`, indentation is dropped
  because the address carries the nesting).
- A bare name with two or more matches answers a candidate list instead of `ambiguous_symbol`
  ([symbol and graph item](symbol-graph-item-v1.md) r3 §2).
- `Outline`, `Symbol`, `normalize` and the bundled-module wire are unchanged: addresses are derived
  in the core from the outline and the text, so module-hosted languages need no protocol change.
  `SymbolPath` gains the §2 escapes (`Display` encodes, `parse` decodes) and `Outline::find` gains
  the §4 ordinal and ordering rules.

## 11. Conformance

The provider's contract tests (the evidence M-029 and M-030 may reuse):

- grammar: round trip of parse and print over names with `/`, `%`, `~`, `:`, spaces and digits;
- totality: for every first-party `.rs`, `.py`, `.js`, `.ts`, `.css` and `.html` file of this
  repository and the fixture corpora, every line has exactly one innermost unit, region spans do not
  overlap symbol spans, and `resolve(at_line(n).address)` contains `n`;
- uniqueness: no two units of one file share a canonical address; `resolve(unit.address)` returns
  that unit;
- stability: appending a line above a symbol, or editing another symbol, leaves its address and its
  `:N` unchanged; inserting a line inside it shifts only the `:N` below the insertion;
- per-language region fixtures for every row of §5.

A mutation that must be caught: drop the `~k` suffix for duplicates (uniqueness test fails) and
shift `N` to be absolute (stability test fails).

## 12. Stub signatures for consumers (published before the Epic freeze)

Landed first by M-028 task "foundation" as compiling stubs (`todo!()` bodies, `#[ignore]`d contract
tests), in `agent_ide_core::lang::address` unless noted. They map M-029's six needs
(symbol-read-edit-v1 §6) one to one; region names in addresses carry the `@` prefix (§5).

| M-029 need | Signature |
|---|---|
| 1 parse and `Display` round trip, typed window | `Address::parse(&str) -> Result<Parsed, AddressError>`; `impl Display for Address`; `Address::window(&self) -> Option<Window>`; `Window::to_lines(self, span: LineRange) -> Result<(LineRange, bool), ResolveError>` (second value: clamped) |
| 2 unit over symbols and regions | `AddressMap::unit(&self, path: &SymbolPath) -> Option<&Unit>` (`Unit.span` includes doc comments, attributes and decorators; `Unit.address` is the canonical form; a region is the one-segment path `@imports`) |
| 3 total over every line, address string | `AddressMap::at_line(&self, line: u32) -> Option<Located>`; `AddressMap::address_at(&self, file: &Path, line: u32) -> Option<String>` (`file#unit:N`, no absolute line) |
| 4 same regions without a language server | `outline_from_source` is unchanged; `AddressMap::build(&outline, source)` consumes either outline, so the addresses of an edited candidate are `AddressMap::build(&lang.outline_from_source(file, candidate)?, candidate)` |
| 5 region as insert anchor | `AddressMap::insert_site(&self, source: &str, lang: &dyn LanguageSupport, anchor: &Address, where_: InsertWhere) -> Result<InsertSite, LangError>`: a region anchor places `After` at its last line + 1 (`@imports` after the last import; `@header` for a file without imports, via `imports_insertion_line`) and `Before` at its first line; a symbol anchor delegates to `LanguageSupport::insert_site`. The trait is unchanged. |
| 6 duplicate names | canonical `~k` forms of §4; `AddressMap::resolve(&Address)` parses them back to exactly that unit |

Consumers never format or parse an address themselves; they call `Address` and `AddressMap`.
