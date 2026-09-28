# Language bridge contract (cross-language name facts)

Revision: stage 5. Provider: Agent IDE core. Consumers: language crates (providers) and the core's
tool integration (planned).

## Implementation status

| Capability | Status |
|---|---|
| `lang::names` contract: namespaces, `NameKey` with domain, `NameFact`, `FactSink`, `NameFacts` trait, `LanguageDescriptor::names` | implemented |
| `intelligence::names` index: listing, stat sweep, digest confirmation, `verify`, `uncovered`, LRU of worktrees | implemented |
| Worker glue (`assistance/links.rs`): refresh on the blocking pool, park while building | implemented |
| Style-sheet provider (`agent-ide-lang-css`: CSS, SCSS, Sass, LESS), §7 | implemented |
| HTML provider (`agent-ide-lang-html`), §8 | implemented |
| TypeScript/JavaScript provider (`agent-ide-lang-typescript`, JSX included), §9 | implemented |
| Bridge data in `ide.symbol`, `ide.read` of sigil addresses and the `ide.start` card (tools-v0.4 §2.3.1) | implemented |
| Link edges in `ide.graph` (tools-v0.4 §2.3.1) | implemented |

`ide.symbol` shows `defines:`, index-backed usages and `links:` for symbols of languages with name
facts, name cards for sigil addresses, and bridge candidates in ambiguity lists (tools-v0.4
§2.3.1). Replies for languages without name facts are byte-identical, except that a bare name two
languages share is now ambiguous instead of resolving in the first language.

## 1. Fact model

A **fact** is one syntactic statement about one name at one location, emitted by the language that
owns the file:

| Field | Meaning |
|---|---|
| `key.namespace` | Core-owned namespace (§2). |
| `key.domain` | Scope inside the worktree. Empty means worktree-global. A non-empty domain (for example a module path) must be spelled the same way by every provider of the namespace. |
| `key.name` | Normalized name (§3): no sigil, no escapes. |
| `role` | `Define` (introduces the name) or `Use` (refers to it; never implies it resolves at run time). |
| `line`, `column` | 1-based; the column counts bytes. |
| `certainty` | `Exact` (the supported syntax contains exactly this decoded name) or `Heuristic(reason)` (reading it as a name needed a stated assumption, such as `template literal`). |

A fact carries no language and no line text. The language is always the one owning the file, and
the text is read at render time; that read is the freshness proof (§5).

**Join rule.** Two facts meet only when namespace, domain and name are all byte-equal. `.main`
(class) and `#main` (id) never meet. The same class name in two domains never meets, and a global
key never meets a scoped one.

## 2. Namespaces

Namespaces are `lang::names::ns` constants. The id is versioned: a change of normalization rules
bumps the version, so old and new facts never join by accident.

| Constant | id | Label | Define side | Use side | Bare-name sigil |
|---|---|---|---|---|---|
| `ns::CLASS` | `class/v1` | class name | style rule selector (`rule`) | class attribute or expression | `.btn` |
| `ns::ELEMENT_ID` | `id/v1` | element id | markup `id` attribute (`element`) | selector `#x`, fragment link `#x`, DOM query | `##main` |
| `ns::STYLE_VARIABLE` | `style-variable/v1` | style variable | `--x:` declaration (`declaration`) | `var(--x)` | `--brand` |

A new namespace is one more constant plus extractors in the language crates; no core logic changes.

## 3. Normalization per namespace

Each extractor normalizes before it emits:

- `class/v1`: the decoded class token. Selector escapes are removed (`\:` → `:`, `\/` → `/`).
  Case is kept (standards mode). Nested-rule suffixes resolve to the full name (`&-primary` under
  `.btn` → `btn-primary`); when the parent is not a single class the result is `Heuristic`.
  Hashed module classes are never emitted as global class names: they need a non-empty domain.
- `id/v1`: the decoded id, case kept, without `#`.
- `style-variable/v1`: the property name without the leading `--`, case kept.

## 4. Provider contract

A language opts in through `LanguageDescriptor::names: Option<&'static dyn NameFacts>`:

```rust
pub trait NameFacts: Send + Sync {
    fn coverage(&self) -> &'static [NamespaceCoverage];
    fn extract(&self, file: &Path, source: &str, sink: &mut FactSink) -> FileVerdict;
}
```

- `coverage` states, per namespace, whether the language may emit definitions and/or uses.
- `extract` is pure: no filesystem, network, subprocess or language server. `file` is
  worktree-relative; `source` is the complete UTF-8 text (the core skips non-UTF-8 files).
- `FileVerdict::Skipped(reason)` (`minified`, `generated`) discards the file's facts; the core counts
  skipped files by reason.
- `FactSink::push` drops invalid facts and counts them: a name of 0 or more than 256 bytes, a name
  or domain containing a control character or whitespace, or a zero line or column. It returns
  `false` once the sink holds 5 000 facts; the extractor should stop, and the file is marked capped.

## 5. Index, coverage and freshness

The index lives in the worker, one per worktree incarnation, at most 4 worktrees (least recently
used dropped; a recreated worktree never reuses its predecessor's index). Storage is in memory only
in this release: nothing is persisted, and a daemon restart rebuilds the index lazily.

**Prewarm.** `ide.start` starts a build in the background (on the blocking pool, never delaying the
activation reply) when the bounded presence walk finds files of a language that defines names; a
repository without such files builds nothing. One build runs at a time per worktree: a bridge
question that arrives while the build holds the index parks as `names:building` (the ordinary
`pending` path) and is answered from the index once it is done. The `ide.start` card's `links:` line
gains `(indexed N files, M facts)` once a build of the worktree exists.

**Telemetry.** A refresh that re-read files records `name_index_refreshed` with the index state,
bucketed file and fact counts and the duration; it names no language, path or name.

**Candidates.** In a Git worktree: `git ls-files -z --cached --others --exclude-standard`, run like
the check fingerprint (`/usr/bin/git`, fsmonitor off, 5 s budget), so ignored trees never enter.
Without Git, or when Git fails: a bounded breadth-first walk that skips hidden directories and
`.git`, `.hg`, `.venv`, `venv`, `node_modules`, `target`, `dist`. Only files of languages with a
provider are indexed; every registered language seen is recorded as present.

**Sweep.** A refresh lists candidates, stats each one, and re-reads only files whose `(size, mtime)`
changed. A blake3 digest confirms the change before the file's facts are replaced, all at once.
Files no longer listed lose their facts. Every read goes through `read_authorized_source`.

**States.**

| State | Meaning |
|---|---|
| `Building` | A sweep is unfinished and has used less than the 20 s build budget. Queries park 300 ms and retry (`provider_loading`, detail `names:building`). |
| `Ready` | Every listed candidate was swept. |
| `Partial { indexed, listed }` | A bound was hit: the build budget, the listed or indexed file caps, or the worktree fact cap. Counts are lower bounds. An unfinished sweep resumes on the next refresh. |

**Coverage.** `uncovered(namespace)` lists the present languages whose provider covers neither role
of the namespace, or that have no provider. Replies must say "unavailable for" these languages, not
"zero".

**Counts are indexed, not live.** A count comes from the last sweep. A same-size rewrite within the
same mtime nanosecond keeps an undisplayed count stale until the next change. A displayed row is
always proven: its file is read at render time and `verify(file, bytes)` compares the digest; on a
mismatch the file is re-extracted from those bytes and the caller queries again, so a stale row never
reaches a reply.

**Order.** Sites are ordered by file path bytes, then line, column, namespace id and name,
independent of discovery order.

## 6. Bounds

| Bound | Value |
|---|---|
| Listed candidates (provider-backed) | 50 000 |
| Indexed files per sweep | 20 000, in path-byte order |
| File size | 1 MiB (`MAX_SOURCE_BYTES`); larger files are skipped as `large` |
| Facts per file | 5 000 (the file is marked capped) |
| Facts per worktree | 500 000; a file that would pass it is skipped as `facts cap` |
| Worktrees kept | 4, least recently used dropped |
| Build budget | 20 s of sweeping, resumable |
| Warm refresh target | ≤ 200 ms at 10 000 unchanged files |
| Fallback walk | 10 000 directories |

## 7. CSS provider

`agent-ide-lang-css` owns `.css`, `.scss`, `.sass` and `.less`. It has no server and no project
check; the symbol tools outline style sheets from source (rules named by selector text, nested
rules as children, at-rules as containers).

**Tokenizer.** Hand-written. It skips comments (`/* */`, and `//` in SCSS, Sass and LESS),
strings, escapes, `url(…)` and interpolation (`#{…}`, `@{…}`), so braces and semicolons inside
them never count. Indented Sass is first given braces at line ends, so positions stay those of the
original text.

**Coverage.** `class/v1` define and use, `id/v1` use, `style-variable/v1` define and use.

| Syntax | Fact |
|---|---|
| `.x` anywhere in a rule's selector list (`.card .btn` defines both; `:not(.x)` too) | `Define class x`, exact |
| `#x` in a selector | `Use id x`, exact |
| `&-x`, `&__x` (SCSS, LESS) under a parent that is exactly one class `.p` | `Define class p-x`, exact |
| the same under any other parent | `Define class <c>-x` for each class `c` ending a parent selector, `Heuristic("nested suffix")` |
| `--x: …` declaration | `Define style-variable x`, exact |
| `var(--x)` (fallbacks included) | `Use style-variable x`, exact |
| `@extend .x` | `Use class x`, exact |

**Normalization.** Escapes decode (`\:` → `:`, `\/` → `/`, hex escapes); case is kept; sigils are
dropped. Attribute selectors (`[href="#x"]`) and at-rule preludes (`@media …`) are not read.

**Reduced coverage, not skips.** A name touching interpolation (`.btn-#{$size}`, `.x-@{v}`) yields
no fact; the rest of the file is still indexed.

**Domains.** Classes in `*.module.css` / `*.module.scss` (any `*.module.*` style sheet) get the
file's worktree-relative path as their domain, so they never join global class names. Their ids and
style variables stay global.

**Skips.** `*.min.*` files and files whose average line exceeds 2 000 bytes are
`Skipped("minified")`.

## 8. HTML provider

`agent-ide-lang-html` owns `.html` and `.htm`. It has no server and no project check; the outline
holds landmark elements (`header`, `nav`, `main`, `section`, `article`, `aside`, `footer`, `form`,
`table`), elements with an `id` (named `tag#id`, so `index.html#main#content` addresses one) and
`script`/`style` elements, nested by the element tree. Other outlined elements are named by tag and
first class (`nav.site-nav`).

**Tokenizer.** Hand-written. Tag and attribute names are case-insensitive; values may be
double-quoted, single-quoted or unquoted; comments, doctypes and processing instructions are
skipped; void elements and `/>` never open; `<script>` and `<style>` bodies are skipped whole
(embedded regions are a later stage). An end tag closes the nearest open element of its name.

**Coverage.** `class/v1` use, `id/v1` define and use.

| Syntax | Fact |
|---|---|
| `class="a b"` | `Use class a`, `Use class b` |
| `id="x"` | `Define id x` |
| `href="#x"` | `Use id x` |
| `for="x"`, `list="x"`, `form="x"` | `Use id x` |
| `aria-labelledby="a b"`, `aria-describedby="a b"` | `Use id a`, `Use id b` |

**Normalization.** Character references decode (`&amp;`, `&lt;`, `&gt;`, `&quot;`, `&apos;`,
`&nbsp;`, numeric); names are case-sensitive; positions point at the value token (at `#` for
`href`).

**Templates.** A class or id-list value holding a placeholder (`{{ }}`, `{% %}`, `<%= %>`, `${ }`)
makes its other tokens `Heuristic("template")`; a token touching a placeholder, and a single-id value
holding one, yield no fact.

**Skips.** `*.min.*` files and files whose average line exceeds 2 000 bytes are
`Skipped("minified")`.

## 9. TypeScript provider

`agent-ide-lang-typescript` states name facts for its files (`.ts`, `.tsx`, `.js`, `.jsx`, `.mts`,
`.cts`, `.mjs`, `.cjs`). Scripts only use names: coverage is `class/v1` use and `id/v1` use.

**Scanner.** A lexical scan (no parser) produces identifiers, punctuation, string literals and
template literals (nested `${…}` tracked), skipping comments and regular-expression literals.
Regex versus division is decided from the previous token (`</` closes a JSX element, it is not a
regex); a quoted string never spans a line, so an apostrophe in JSX text costs at most the rest of
that line.

| Syntax | Fact |
|---|---|
| `className="a b"`, `class="a b"`, `className={"a b"}` | `Use class a`, `Use class b`, exact |
| a template literal there | static tokens that touch no `${…}`, `Heuristic("template literal")` |
| inside `className={…}`: string literals and object keys within `clsx`, `classnames`, `classNames`, `cx`, `cn`, `twMerge`, `twJoin` | `Heuristic("clsx call")` |
| any other string literal inside `className={…}` | `Heuristic("expression")` |
| `getElementById("x")` | `Use id x`, exact |
| `querySelector("#x")` / `querySelector(".x")` / `querySelectorAll(…)` with one simple selector | `Use id x` / `Use class x`, exact |

Strings with escapes and compound selectors name nothing. `*.min.*` files and files whose average
line exceeds 2 000 bytes are `Skipped("minified")`.

**Cost.** A symbol card consults the index only when the symbol's own file states a fact inside
the symbol (checked from the observed bytes), so cards of ordinary code keep their latency. Bare
names consult the index only when a language that defines names (style sheets, markup) is present.
