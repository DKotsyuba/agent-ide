# Language bridge contract (cross-language name facts)

Revision: stage 1. Provider: Agent IDE core. Consumers: language crates (providers) and the core's
tool integration (planned).

## Implementation status

| Capability | Status |
|---|---|
| `lang::names` contract: namespaces, `NameKey` with domain, `NameFact`, `FactSink`, `NameFacts` trait, `LanguageDescriptor::names` | implemented |
| `intelligence::names` index: listing, stat sweep, digest confirmation, `verify`, `uncovered`, LRU of worktrees | implemented |
| Worker glue (`assistance/links.rs`): refresh on the blocking pool, park while building | implemented, not yet called by any tool |
| Providers for real languages (style sheets, markup, JSX) | planned (stages 2–4) |
| Bridge data in `ide.symbol` / `ide.graph` / the project card | planned (stages 3–5) |

No tool reply changes because of this stage. Tool behaviour for languages without a provider stays
byte-identical.

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
used dropped; a recreated worktree never reuses its predecessor's index). Nothing is persisted.

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
