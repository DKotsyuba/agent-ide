# Symbol-addressed tools v0.4 contract

Revision: v0.4. Provider: Agent IDE. Consumers: coding agents and IDE hosts.

## Implementation status

| Tool or capability | Status |
|---|---|
| `ide.outline`, `ide.read`, `ide.symbol` (usages, callers/callees, `history: true` for the last 3 commits touching the definition) | implemented |
| `ide.edit` symbol operations (`replace`, `insert`, `delete`, `rename`) and `path` + `lines` range operation | implemented (landing now) |
| Long-lived language server session for Rust | implemented |
| `ide.start` project card | implemented (appended to the activation reply; no `ide.project`, no `not_a_project`; servers always `not started`) |
| `ide.test` (`symbol` / `path` / `pattern` / `command` / `status`, `budget_s`; one job per worktree; status line in the `<agent-ide>` block; full output paged through `ide.inspect`) | implemented (wire version 4, 11 tools) |
| `ide.graph` (`symbol`, `direction`, `depth`, `tests`; live bounded caller/callee tree; only functions and methods are nodes, and tests collapse to one `+N tests` line per parent unless `tests: true` expands them with `[test]` marks) | implemented (wire version 5, 11 tools) |
| `ide.diff` cleanup (compact default, `provenance` flag, plain-`git diff` fallback, untracked symlinks listed) | implemented (v0.6.1) |
| `ide.problems` cleanup | planned |
| Live language server sessions for Python and TypeScript (one per binding and language) | implemented (Go planned) |

## 0. Principles

1. **Address symbols**, not lines or offsets. Lines are a fallback mode and are used in read output.
2. A tool returns information an agent cannot get from `grep` or `cat` in a second: types, documentation, relationships, compiler errors, and project commands.
3. **Keep responses compact.** Responses contain no internal bookkeeping fields (`authority_epoch`, hashes, generations). Lists have ceilings and report “N more”; retrieve the complete list through `detail_ref`.
4. Errors keep the closed reason and stage, then name what happened and the next call. For example, a missing path answers `error: no_such_file: src/assistance/host_bindng.rs (outline:no_such_file); this path does not exist in the worktree. Fix the path, or use ide.symbol with a bare name to find it`. Host-correlation refusals retain a closed cause in parentheses and explain whether to retry, call `ide.start`, or continue with native tools. Causes include `invalid_metadata`, `missing_field`, `invalid_field`, `invalid_attachment`, `unsupported_hook_phase`, `missing_invocation`, `outside_allowed_roots`, `hooks_not_delivered`, `missing_pre`, `replay`, `mismatch`, `inactive_binding`, `capacity_exceeded`, `host_unrecognized`, and `project_moved: bound to <path>, asked <path>`. The same stage tag is recorded in the daemon journal's `detail` field.
5. The language server runs as it would in a human's editor and remains alive for the session.
6. Every host uses one path: return the result directly when it arrives within about 10 seconds; otherwise return `pending` and use `ide.inspect`.

`ide.start` refusals name the actionable cause: an absent requested root includes the nearest existing ancestor under an allowed root; Git discovery failures include a short reason; unresolved worktrees and durable state failures have distinct stages. A worktree conflict says whether another actor owns it, this actor's other channel owns it, or this actor already owns another worktree, and names `ide.stop` or reuse as the remedy. An `ide.start` of another root by the actor that already holds an activation elsewhere hands that activation over without an `ide.stop` when nothing is pending on it (no queued or pending job, no edit that is unsettled or `outcome_unknown`, no running test): the old authority, provider ownership and leases are released as a stop would release them, the same session's grant moves to the new root in one durable step (a stopped session cannot start again), another session of the actor is revoked and stopped first, and the handover is journaled (`handover: released <path>`). Otherwise the `start:actor_owns_another_worktree` refusal stays, names the held worktree path and what keeps it (`; worktree <path>; kept: <reason>`), and releases nothing; another actor's activation is never touched. Provider cache namespace conflicts have their own stage and recovery. A successful repeat by the same actor and binding reports the existing activation idempotently. The start card explains which tools can answer from source while a language server starts. Its baseline says why coverage is partial: Git metadata and source bytes are captured separately, so an atomic window is not proven.

## 1. Common conventions

### Symbol path

```text
<file>#<Owner>/<name>
src/assistance/host_binding.rs#HostBindingGuard/establish_start
src/index.ts#ClassImpl/method
src/agent_tasks/persistence.py#run_mutation
```

- `#` separates the file from the symbol. A filename may contain any characters; the file boundary never has to be guessed by checking whether a path exists.
- Within a symbol path, `/` expresses nesting: type/impl/class/namespace to member. Rust modules and nested Python functions use the same levels.
- To search by name without a file, use `#BindingStatus` or `BindingStatus`. The IDE searches the project (every language) and returns candidates when there is more than one; it does not guess:

```text
ambiguous_symbol: new matches 3 symbols; repeat ide.symbol with one exact path:
  src/assistance/host_binding.rs#HostBindingGuard/new
  src/execution/mod.rs#AdmissionController/new
  src/workspace/git.rs#GitIdentity/new
```

- Symbol boundaries include the header: docstrings, attributes, decorators, and JSDoc are part of the range for reads, replacements, and deletions. The language crate normalizes differences between servers.

### Lines and coordinates in output

- Lines are 1-based, as in editors and compilers; locations use `file:line[:column]`.
- Code fragments include line numbers on the left, followed by one tab before the code: `906\t/// Establishes one exact start binding …`. A tab is the separator agents already strip from their own read tool, so a copied line never keeps separator spaces.

### Readiness and waiting

- Return a result directly if the server responds within about 10 seconds. Otherwise return `pending: use ide.inspect with detail_ref X`. `ide.inspect` waits up to 10 seconds before returning `pending` again.
- While a language server is starting, symbol tools return `error: provider_loading; the language server is still loading the workspace; repeat the call in a few seconds` instead of waiting silently for minutes.
- A language that opts in to outlining from its text while loading (today only Rust) answers `ide.outline`, `ide.read {symbol}` and the symbol form of `ide.edit` (`replace`/`insert`/`delete`) from that lexical outline while its registered server is still loading, when that server is unavailable (its workspace failed to load, or no launch configures it), and when its ready session's `documentSymbols` exchange itself fails, and the reply says so in one compact line — `outline: from source, exact (rust-analyzer still indexing; no need to repeat)` while it loads, `outline: from source, exact (rust-analyzer unavailable; no need to repeat)` when it failed, `outline: from source, exact (rust-analyzer request failed: <short cause>; no need to repeat)` when the exchange failed — the outline is already exact, so the call needs no repeat, but semantic facts (usages, callers, rename) are not included. A lexical outline is either exactly the outline the server path would give for the same text (addresses, kinds, ranges, children) or it is not used: a file it cannot reproduce exactly (for Rust, whose lexical outline comes from a full parse: a syntax error, from which rust-analyzer recovers with a tree of its own; comment text directly above an item, which rust-analyzer may attach to the item's range; an `extern` block; a `// region:` comment; more tokens or deeper brackets than the parse's bounded stack is sized for — 240 000 tokens, 128 brackets) keeps `provider_loading` rather than a guessed range while the server loads, and `provider_unavailable` when it failed or its exchange did. An address the lexical outline does not contain is not proven absent: while the server loads the call waits for it (`provider_loading`, parked like any loading call; an edit answers it at once) instead of `unknown_symbol`, and when the server is unavailable the call answers `provider_unavailable` at once rather than park for a server that will not answer. `ide.symbol` waits for the server while it loads; when the server is unavailable it answers the definition-only card — signature, doc and definition come from the lexical outline — with each live-session section noting the failed workspace (`usages: unavailable (rust-analyzer workspace failed to load)`). `ide.graph` and `rename` always wait for the server. A refusal on these paths names its stage (`<server>: workspace load failed`, `<server>: transport gone`, `<server>: documentSymbols/references request failed`) and its reply states what still works (`ide.outline`/`ide.read` from source), what does not (usages, callers) and how to recover (retry later; if it keeps failing, fix what stops the project from loading — `ide.context {"kind":"problems"}` shows the project check). The moment the live session answers, the server path is used again.

### `<agent-ide>` status block

A hook or the top of a response may include check and background-test status, in no more than 6 lines:

```text
<agent-ide>
rust: 0 errors, 0 warnings
tests #3: running 42 s; poll: call ide.test with {"status": 3}
</agent-ide>
```

When the worktree's checked-out branch or detached commit changed outside the IDE (another process ran `git checkout`/`switch`), the plate of the first call that probes HEAD afterwards leads with one line, delivered once. HEAD is probed when the session starts and then by a completed call at most once per 30 s, so the line can trail the switch by up to 30 s of calls: `git: HEAD moved 4e2e2e2 → 9daac64 (claude/a → claude/b) outside Agent IDE; earlier indexed answers may be stale`. A commit on the same branch is not reported. It is a notice only: nothing is invalidated or restarted.

### Ceilings and pages

- A response is at most 64 KiB (the serialized reply is held to 63 KiB, leaving a 1 KiB reserve for the MCP envelope). Lists are capped at 30 usage lines, 20 caller lines, 8 diagnostic lines in an edit reply, and 20 problems per problems page. Any remainder is reported as “N more” with a `detail_ref`.
- This ceiling bounds the *reply*, not the model-facing *argument*: `ide.edit`'s `content` accepts up to 128 KiB (v0.6.1), enough for a whole module in one call.
- `ide.inspect {detail_ref}` retrieves pages; `ide.inspect {detail_ref, page}` retrieves a specific page.
- A symbol card with more than 30 usages, or an ambiguity list with more than 20 candidates, keeps the cut rows in its detail. The first page is the card or list as before, ending with `… N more (ide.inspect <detail_ref>)`; each later `ide.inspect` returns the next page of the remaining rows (`usages 31–N of N:` or `candidates 21–N of N:`, same row format). Cards and lists within the ceilings are unchanged. The first `ide.inspect` after an inline reply always returns page two, never page one again, for every paged tool.

### Permissions

The IDE's configured `allowed_roots` is the sole path rule and remains unchanged.

## 2. Tools

### 2.1 `ide.start` — activation and project card (implemented, see status table for gaps)

Input: `{activation_id?, root?, read_only?, environment?}`. A start that names no `activation_id` keeps a stable
default derived from its session binding, so repeating it returns the same activation. Omit
`read_only` or set it to `false` for the one writer allowed per worktree; set it to `true` for a
reader. Readers coexist with writers and other readers. A reader can upgrade by starting without
`read_only` when the writer slot is free, and a writer can downgrade by starting with
`read_only: true`. A reader's start schedules no project check (neither the first check nor later triggers or environment restarts); it still sees the problems a writer's check produced.

`environment` selects the current environment per project root: for example,
`{"environment":{"python":".venv-py314","python:packages/alpha":".venv"}}`.
The object has at most eight entries, with nonempty string selectors of at most
1,024 characters. Root suffixes are relative to the worktree. Absolute selectors
must pass the launcher's `allowed_roots` admission; the language validates candidates
and reports pin conflicts as `invalid_detail` with its reason. `"auto"` clears that
key. Readers cannot pass this property. Choices persist in the daemon store for
the worktree incarnation, survive restart, and are shared by its sessions. Repeating
the same activation ID applies a new choice and returns a refreshed card without
changing activation identity. Resolved environment lines replace that language's
generic facts, show at most four candidates, and offer a choice when alternatives
exist. Changes invalidate checks and emit a one-shot environment-change plate line.

Output is a deterministic project card, without model-generated content:

```text
project: agent-ide  root: /Users/pluto/projects/agent-ide  (git: claude/e005-allowed-roots, clean, last 45d3ad0 test(product): …)
languages: rust 47k lines · typescript 2k (scripts) · python 1k (scripts)
commands (from CI + manifests):
  build: cargo build --release      check: cargo check --workspace --all-targets
  test:  cargo test --workspace     lint: cargo clippy --workspace --all-targets -- -D warnings
  fmt:   cargo fmt --all            docs: cargo doc --no-deps
environment: rust 1.98.1 (rustup) · node 24.4.0 · python: no .venv
layout: src/ (assistance 14 files, execution 4, intelligence 9, workspace 7, checks 5) · tests/ 31 · docs/ 24 · scripts/ 9
entry points: src/main.rs (binary agent-ide) · src/lib.rs
docs: README.md, CLAUDE.md, AGENTS.md, docs/contracts/*.md (12)
servers: rust-analyzer loading (~10 s) · pyright ready · tsserver ready
problems: rust checking (first check)
```

Commands come from CI (`.github/workflows`), `Makefile`/`justfile`, or manifests. If a command comes from README, mark it `(README)`. A project may also declare commands itself: a fenced block at the root of `AGENTS.md`, or of `CLAUDE.md` when `AGENTS.md` declares none (each read only as the worktree's own regular file of at most 64 KiB — a symlink, a non-regular file or a larger file declares nothing, so nothing outside the worktree reaches the card), tagged `agent-ide`, one `<kind>: <command>` line per kind (the closed set `build`/`check`/`test`/`lint`/`fmt`/`typecheck`):

```` ```agent-ide
check: cargo xtask check
``` ````

A kind this block names wins outright over CI/manifest/README for that kind (provenance `agents`/`claude` in the heading); a block with an unknown key, a repeated key, a line with no command, a command over 200 bytes or containing a control character, or no closing fence is ignored entirely rather than partially trusted, and the kind falls back to CI/manifest as before. Layout lists only first- and second-level directories with file counts. The card never exceeds 1,500 bytes: layout children, then docs collapse first, and a card that still overflows is cut on a character boundary and ends with `… (card truncated)`. Cache the card for the session; `ide.project {}` returns it again.

Implemented wire form: the activation reply keeps its first line (`activated: epoch N; baseline: …`) and appends the card after a blank line. The card is computed under a 5 s budget off the runtime; when it does not fit the budget the reply is the plain activation text. Commands print one line per kind with the provenance in the heading (`commands (agents, ci, manifest):`), `environment:` names the toolchain and edition, `servers:` prints `not started` for each detected language (the daemon does not probe servers at start), and there is no `ide.project` yet — call `ide.start` again to see the card. Symbol requests on a Rust worktree where the root is not a Cargo workspace also load nested crates (two directories deep, excluding test material), so references across such a crate's own tests resolve.

A directory with no `.git` in it or any ancestor activates as a plain directory; its baseline says `not a git repository: no git data`, and requested symbol history reports that sentence.

Errors: `outside_allowed_roots` (as today). There is no `not_a_project` failure (see the status table).

### 2.2 `ide.outline` — file skeleton (implemented)

Input: `{path, kinds?: "fn,method,…"}`. A `path` naming a directory (trailing slash optional) answers a directory outline instead: subdirectories with file counts, then files with line counts and the first documentation line (`//!` for Rust, the module docstring for Python, the leading comment for TypeScript/JavaScript), one level deep, at most 200 files.

`kinds` is an optional comma list over the closed `SymbolKind` vocabulary (`module`, `namespace`, `struct`, `enum`, `class`, `interface`, `trait`, `impl`, `type`, `fn`, `method`, `constructor`, `field`, `variant`, `const`, `var`, `test`, `symbol`), for a file too big to read whole (T163): a symbol shows when its own kind is selected or a descendant's is, so a container of a selected member still shows for context, and test modules always collapse exactly as they do unfiltered. Byte paging already bounds the reply; the filter is the agent-controlled cut for a large outline:

```text
src/assistance/worker.rs  (4700 lines, rust)
 906  pub fn establish_start(&mut self, candidate: Candidate, channel: ChannelRef) -> BindingStatus
 …
  (262 symbols; showing 31 by kinds fn,method; tests collapsed: 31)
```

Replies of every tool are delivered inline when the job completes within 8 s (the daemon waits; the MCP bridge allows 10 s for the reply after a fast 1 s connect); a longer job answers `pending` with a `detail_ref` as before.

Output contains signatures, docstrings, and line numbers, without bodies:

```text
src/assistance/host_binding.rs  (2292 lines, rust)
  36  const CLAUDE_TOOL_USE_ID: &str
 250  pub struct HookEvent            // One bounded native hook observation …
 296    pub fn tool_name(&self) -> Option<&str>
 302    pub fn agent_type(&self) -> Option<&str>
 333  pub enum BindingStatus          // Reports the lifecycle strength available …
 401  pub struct HostBindingGuard
 700    pub fn observe_hook(&mut self, event: HookEvent, channel: ChannelRef) -> BindingStatus
 911    pub fn establish_start(&mut self, candidate: Candidate, channel: ChannelRef) -> BindingStatus
 …
  (48 symbols; tests module at 1700–2292 collapsed: 31 tests)
```

The docstring is the first displayed line. Collapse test modules to a count. For Python, show decorators; for TypeScript, show `export` and overloads on one line.

Errors: `no_such_file` (the requested path does not exist in the worktree; fix the path or use `ide.symbol` with a bare name), `unsupported_file` (the file exists but no IDE language reads its type, such as `Cargo.toml` or a shell script; read its text with `ide.read` `path` and `lines` or `ranges`, or native tools — the same refusal answers `ide.read`, `ide.symbol` and `ide.graph` addressed into such a file), `outside_allowed_roots`.

### 2.3 `ide.symbol` — symbol card and relationships (implemented)

Input: `{symbol, usages?: true, callers?: 0..3, callees?: 0..3, history?: true}`. The descriptions spell out `0–3` because some hosts omit the JSON schema maximum. Defaults: usages, callers 1, callees 0; `history` is opt-in (boolean) and shows the last 3 commits touching the definition.

Output:

```text
symbol: BindingStatus — enum, src/assistance/host_binding.rs#BindingStatus (lines 330–346)
signature: pub enum BindingStatus { PreObserved, Validated(ValidatedInvocation), Settled(ValidatedInvocation), NativeObserved(BindingRef), Unavailable(BindingUnavailable) }
doc: Reports the lifecycle strength available to a Workspace or Execution consumer.
     PreObserved is not an authority claim. Validated proves a trusted pre-hook matched …
usages: 63 in 9 files (src 19, tests 44)
  src/assistance/worker.rs:3366        let BindingStatus::Validated(invocation) = status else {
  src/assistance/assembly.rs:412       BindingStatus::PreObserved => Some(PeerReply::HookObserved {}),
  src/assistance/host_binding.rs:429   BindingStatus::Unavailable(BindingUnavailable::CapacityExceeded)
  … 16 more (ide.inspect sym-12)
history: 3 last commits touching the definition
  ffb25c2 2026-09-25 refactor(assistance)!: serve Claude through the daemon route …
  bbd58f6 2026-09-25 feat(assistance)!: allowed_roots gate …
```

For functions, include `callers` (`callers: N` with one `name  file:line` row each) and `callees` to the requested depth, capped at 20 lines per level. Read function bodies with `ide.read`.

Errors: `ambiguous_symbol` (candidates), `unknown_symbol` (no similar-name suggestion; check the file with `ide.outline` or search a bare name with `ide.symbol`), `provider_loading`.

### 2.3.1 Cross-language links (implemented for CSS and HTML)

Languages with name facts (see [the language bridge contract](language-bridge.md)) add index-backed
lines to `ide.symbol`. Counts are indexed, not live; every row shown was read and verified against
the file's current bytes first, and a row whose file can no longer be read is dropped and counted
(`; N stale dropped` in the usages line). Cards of languages without name facts are unchanged.

A symbol that defines names (a style rule) gains `defines:` lines and tagged usages; one that uses
names gains a `links:` section (at most 20 names, 2 definitions each):

```text
symbol: .btn — symbol, styles.css#.btn (lines 2–4)
signature: .btn
definition styles.css#.btn  (lines 2–4)
defines: class name btn — also styles.css:6 .layout .btn, theme.scss:2 .btn
usages: 2 indexed in 1 files (src 2, tests 0)
  index.html:5  [html] <button class="btn btn-primary">Save</button>
  index.html:6  [html ~template] <a href="#main" class="btn {{ extra }}">Top</a>
  (~ = heuristic match, not proven)
links: 1 style variable used here
  --brand  → theme.scss:1 :root
unavailable for: python
callers: unavailable (css has no call hierarchy)
```

A name with no indexed definition reads `→ no indexed <rule|element|declaration>`. `unavailable
for:` lists languages present in the worktree that cannot state the name's namespace. While the
index is bounded, `links: partial (indexed A of B files); counts are indexed, not live` follows.

A sigil address (`.btn`, `##main`, `--brand`) goes straight to the index and answers a name card:

```text
symbol: ##main — element id, 1 element, 2 usages in 2 files (css, html)
definitions:
  index.html:4  [html] <main id="main" class="layout">  (index.html#main#main, lines 4–8)
usages: 2 indexed in 2 files (src 2, tests 0)
  index.html:6  [html] <a href="#main" class="btn {{ extra }}">Top</a>
  styles.css:7  [css] #main { display: block; }
unavailable for: python
```

A bare name is looked up in the index and through every language's workspace symbols (the index
is skipped while the worktree has no index yet and a bounded walk finds no file of a language with
name facts, so other projects keep their latency). Index-only
hits answer the name card; hits on both sides answer the ambiguity list, language-server
candidates first:

```text
ambiguous_symbol: btn matches 2 symbols; repeat ide.symbol with one exact path:
  app.py#btn
  .btn  (class name: 3 rules, 2 usages)
```

`ide.read {symbol: ".btn"}` reads the first indexed definition (its enclosing outline symbol). The
`ide.start` card gains `links: class, id, style-variable facts from css, html` when such a language
is present, followed by `(indexed N files, M facts)` once the worktree's index exists; activation
prewarms that index in the background. While the index is first built, a query waits up to 1 s and then parks like a loading
language server (`provider_loading`, detail `names:building`).

**Graph.** `ide.graph` adds cross-language link edges, drawn `⇢`, for symbols of languages with
name facts. Callers of a symbol that defines names (a style rule) are the enclosing outline symbols of
their use sites (the file itself when no symbol encloses the site), tagged with their language; a
callable one continues through the ordinary call hierarchy at the next level. When the server
cannot outline the file (a TypeScript file whose `tsconfig.json` lives below the worktree root),
the enclosing symbol comes from the language's top-level declarations read from the text, and the
node is not expanded. Callees of a symbol
that uses names end in one leaf per name (at most 10, never expanded), located at its first indexed
definition. Depth, the 60-node and 120-edge ceilings, cycles and test collapsing are unchanged, and
graphs without link edges are byte-identical:

```text
graph: callers of styles.css#.btn (depth 2, 5 nodes, 4 edges)
  ⇢ index.html#main#main  index.html:4 [html]
  ⇢ src/Button.tsx#Button  src/Button.tsx:1 [typescript]
    ← src/App.tsx#App  src/App.tsx:2
  ⇢ src/Menu.tsx#Menu  src/Menu.tsx:2 [typescript]
graph: callees of src/Button.tsx#Button (depth 2, 2 nodes, 1 edges)
  ⇢ .btn  styles.css:2 [class name]
```

### 2.4 `ide.read` — symbol body or line range (implemented)

Input: `{symbol}`, `{path, lines: "120-180"}` or `{path}` (the whole file).

Output is code with line numbers and the header included:

```text
src/assistance/host_binding.rs#HostBindingGuard/establish_start  (lines 906–931)
906\t/// Establishes one exact start binding after a trusted pre-observation …
907\tpub fn establish_start(&mut self, candidate: Candidate, channel: ChannelRef) -> BindingStatus {
908\t    let invocation = (candidate.host, candidate.actor_id.clone(), channel.clone());
…
931\t}
source_ref: sym-14
```

`source_ref` is the reference currently called `detail_ref` by edit: it binds to the read content. A stale or incomplete read is refused as `stale_source` with no write; retry with the newest `source_ref` in the reply, or re-read every page before retrying.

Batch reads fetch everything one edit needs in one call:

```text
ide.read {"symbols": ["src/a.rs#Foo/bar", "src/b.rs#qux"]}   // up to 16, any files
ide.read {"path": "src/x.rs", "ranges": ["10-20", "44-60"]}  // up to 16 ranges
```

The reply has one block per item in request order, headed by its address and line range, with the same numbered gutter as every read. Unknown symbols are reported per item (`no such symbol: … — check ide.outline {"path":"…"}`) without failing the rest. One `source_ref` is minted per call and is valid for every file it included: a batch `ide.edit` on any of them needs no re-read, and its line numbers are that version's. Blocks are never cut mid-body: what the reply budget could not hold is listed explicitly (`not included (over the reply budget): src/c.rs#C — call ide.read {"symbols":["src/c.rs#C"]}`); a single block larger than the whole budget pages by bytes like any read. An exactly duplicated address renders once; a nested symbol (a method inside a requested type) renders inside its parent and as its own block.

A file no IDE language analyzes (a log, `README.md`, YAML, a script, `Cargo.toml`) reads as text in every form that needs no outline — `{path}` alone (the whole file, `(empty file)` when it has no lines, paged by bytes like any read), `{path, lines}`, `{path, ranges}` and a bare file path as an item of `{symbols}` — and the reply carries one line before `source_ref`: `note: no code analysis for this format; showing its text only (ide.outline and ide.symbol do not read it)`. A symbol address into such a file (`ide.read {symbol}`, an item of `{symbols}`) has no outline to resolve: the single form answers `unsupported_file`, a batch item says `<file> has no code symbols; read it with ide.read {path, lines|ranges}` — in either order with a bare-path item of the same file, and without failing the rest. A binary file (not UTF-8, or holding a NUL byte), a directory or a file whose bytes could not be read is refused softly, never as `internal`: the single forms answer `source_unavailable` (`read:not_text` or `read:source_unavailable`) naming the path and a native tool, a batch reports a binary item as `not a readable text file (binary, not UTF-8 or unreadable; inspect it with a native tool such as `file` or `xxd`): <path>` and still answers the rest.

Errors: `no_such_file` (the requested path does not exist in the worktree; fix the path or use `ide.symbol` with a bare name), `unknown_symbol`, `unsupported_file` (symbol address into a file no IDE language reads), `source_unavailable` (binary or unreadable file, or a line range past the end).

### 2.5 `ide.edit` — edit a symbol or range (implemented)

Input operations:

```text
{op: "replace", symbol: "src/index.ts#ClassImpl/method", content: "…", source_ref?: "sym-14"}
{op: "insert",  symbol: "src/index.ts#ClassImpl/A", where: "before"|"after", content: "…"}
{op: "insert",  symbol: "src/index.ts#ClassImpl",   where: "first"|"last",   content: "…"}
{op: "delete",  symbol: "src/index.ts#ClassImpl/old"}
{op: "rename",  symbol: "src/index.ts#ClassImpl/method", new_name: "run"}
{op: "replace", path: "src/index.ts", lines: "1-12", source_ref: "sym-14", content: "…"}   // fallback mode
{path: "src/index.ts", source_ref: "sym-14", content: "…"}                                // whole-file replace
{path: "src/new.ts", content: "…"}                                                        // create a missing file
```

The whole-file form without `source_ref` only creates a file that does not exist yet: `ide.read` and `ide.outline` answer `no_such_file` on a missing path, so there is no read to name. The worker observes the path itself; an absence is the base, the candidate is formatted and written through the same confined, stale-safe path as every edit (reply `edit: created` with the project check), and a file that appears between that observation and the write is refused `stale_source`, never overwritten. On an existing file the call is refused with no write: `invalid bounded parameters: "source_ref" is required to replace an existing file: read it first (ide.read)`.

For a symbol, `content` is the complete symbol including its header. The IDE derives indentation and blank lines from neighboring code. After writing, the project's formatter runs over the candidate (before the write), then the project check (cargo check / pyright / tsc) is scheduled at once and the reply carries the edited file's problems from it. `rename` is performed by the language server across the project.

The line-range form **requires** `source_ref`, and it must name a retained read of the same file (an `ide.read` reply's `source_ref`, a completed paged read, or a prior edit's `source_ref`). The edit applies only while that observation's bytes are still the file's current bytes; a stale or incomplete reference is refused as `stale_source` with no write. The reply names the newest `source_ref` for that path known to this session, from its last successful edit or read, when available; retry with that reference. If none is given, re-read with `ide.read` and inspect every page before retrying. The symbol form resolves the symbol again, so its `source_ref` is optional — but when one is given it is validated the same way. When an edit changes the file's line count, its successful reply states the net movement and first changed line (for example `lines after 12 moved +3`). A later line-addressed edit using that edit result is refused as `stale_source (edit:lines_moved)`; re-read current lines with `ide.read {path, lines}`. `old` and symbol edits can continue from the edit reply. A line-count-preserving edit and reads from `ide.read` or `ide.context` remain valid bases for line ranges.

```text
lines after 12 moved +3
```

Output (implemented wire form):

```text
edit: replaced; path src/lang/path.rs; source_ref …-3; diagnostics: current_reported (project check 1.8s: 1 errors, 0 warnings in this file)
src/lang/path.rs:132:9 error [E0308] mismatched types
Next: use ide.edit with source_ref …-3
lines after 12 moved +3
```

```text
edit: replaced; path src/lang/path.rs; source_ref …-4; diagnostics: current_clean. Next: use ide.diff
```

`current_clean` is only ever derived from a completed project check that named no problem in the file; a language server's empty publish is not taken as proof. A clean file in a project whose completed check reports errors in other files answers `diagnostics: current_clean for this file; the project check reports N errors in other files. Next: use ide.context with kind problems` (structured `project_errors: N`), so a clean edit never reads as a passing build. When the check could not have analysed the file, the reply says `diagnostics: not_analysed (<reason>)`, and the reason carries its own next step. For Rust, a file no unconditional `mod` declaration reaches from a build target answers ``rust check may not have compiled this file — no unconditional `mod` declaration reaches it; declare it, then edit again``; a file reached only behind a gate — an inner `#![cfg]` (an integration test behind `feature = "…"`), a `cfg`/`cfg_attr`-gated `mod` on its chain, or a `#[path]` declaration the walk cannot follow — answers that it is built only under that condition and should be checked by a build or `ide.test` command that enables it. A `#[path = "…"]` declaration whose literal names a file beside the declaring file reaches it. A file no IDE language checks (a template, a manifest) answers `not_analysed (no IDE language checks this file type)`. The closed diagnostics vocabulary is `current_reported`, `current_clean`, `not_analysed`, `unknown`, `pending`, and `not_analysed` is never clean. Python files outside every project root, or under a root without an environment, are `not_analysed` with that reason (`the Python project root beside this file has no environment; create one or pick it with ide.start environment`); TypeScript files are always treated as analysed (an unimported file can still read `current_clean`). A provider report for the exact post-edit version (an error rust-analyzer or tsserver found on its own) is kept as it arrives instantly. When the project check cannot analyse the file (it could not run — for example no Python environment — or it skipped the file), a language server's report for the exact post-edit version still answers `current_reported`, one `path:line:col severity [code] message` line per diagnostic, with the delta `language server reported N diagnostics for the exact post-edit source generation; project check did not analyse this file: <reason>` (or `; project check unavailable`): imports may be unchecked, and with no completed check there is no new/pre-existing split. Without such a report the reply is `not_analysed (<reason>)`, never `unknown`. `ide.context` lists its `diagnostic_messages` in the same located form. The reply waits at most 90 s for the check, then says `diagnostics: unknown` and the result reaches the next `<agent-ide>` block or `ide.context`. The `rerun tests:` hint arrives with `ide.test` (§2.6).

For `rename`:

```text
edit: renamed; path src/index.ts; source_ref …-5; diagnostics: current_clean. Next: use ide.diff
renamed method → run; 7 sites in 4 files: src/index.ts (3), src/api.ts (2), test/index.test.ts (2), … (+1 more)
```

The first line's operation word names what happened: `edit: inserted`, `edit: deleted`, `edit: renamed` (`replace` and every plain edit keep `edit: replaced`, `edit: created`, `edit: unchanged`). The rename's second line lists every touched file with its site count, bounded like every other list (five files inline, then `+N more`); its diagnostics are the last written file's, and a rename never waits for the project check — a still-running check reports `diagnostics: unknown` and reaches the next `<agent-ide>` block or `ide.context` like any other edit.

Errors: `stale_source` (including `edit:lines_moved`, which requires re-reading current lines), `unknown_symbol`, `edit_refused` (address resolution, overlap, or a candidate that does not parse — nothing written, retry with the same `operation_id`).

A batch edit changes many places in ONE file in one call:

```text
{operation_id, path, source_ref, changes: [
  {lines: "12-20", content: "…"},
  {symbol: "src/index.ts#ClassImpl/method", op: "replace", content: "…"},
  {symbol: "src/index.ts#ClassImpl", op: "insert", where: "after", content: "…"},
  {symbol: "src/index.ts#ClassImpl/old", op: "delete"},
  {old: "exact text", new: "…", within?: "src/index.ts#ClassImpl/method"}]}
```

`changes` holds 1–32 entries, each addressed by exactly one of `lines` (numbers of the version `source_ref` read — never shifted by other changes in the same call; empty `content` deletes the range), `symbol` (with the same `op`/`where` vocabulary as the single form), or `old` (exact text that must match once, optionally scoped to `within`'s symbol; exactly the matched bytes are replaced, so `old` may start or end mid-line and may include line terminators — `new: ""` deletes the match). Every address resolves against the base bytes before anything is applied; two `old` changes overlap only when their matched bytes intersect (separate substrings of one line, such as two parts of one string literal, apply together), other addresses overlap by line; overlapping ranges, zero-or-multi matches, unknown symbols and ranges past the end are refused together in one reply (`error: edit_refused (edit:refused); change N: …`) naming each failed change and the exact fix — nothing is written and the `operation_id` is unconsumed, so the same id retries. Application is all-or-nothing: one atomic write, one project check.

Before the write the formatted candidate must parse: a structural check runs per language (Rust: syn; Python: its interpreter's `ast.parse`; TypeScript/JavaScript: the accepted typescript package the launcher configured, else the project's own). A candidate that does not parse is NOT written; the reply names the change that produced the error — the change whose range contains the error line, else the nearest change before it, since a parser reports an unterminated construct a line or two after the edit that opened it — its line and a few lines around it. A file that already failed the same check before the edit is still edited (the reply notes the pre-existing error). Languages and files without a checker proceed and report through the project check as before.

The success reply lists where every change landed in the final file, so no re-read is needed to learn the new numbers:

```text
edit: replaced; path src/x.rs; source_ref …-7; diagnostics: current_clean. Next: use ide.diff
3 changes applied: change 1: lines 12–20 replaced (now 12–24); change 2: src/x.rs#Foo/bar inserted after #Foo (now 26–33); change 3: old text at line 88 replaced (now 90–91)
lines after 12 moved +3
```

### 2.6 `ide.test` — run tests on request, in the background (implemented)

Input: `{symbol}` | `{path}` | `{pattern}` | `{command}`, with optional `budget_s?: 120`.

- `symbol` selects tests that reference the symbol (using the language crate); `path` selects tests for a file/module (for Rust, scoped to that file's package with `--manifest-path`, so one file's tests do not build every target in the workspace); `pattern` filters through the test runner; `command` is an exact project command.

Immediate output:

```text
tests #3: started — cargo test --workspace worker::  (4 tests selected, budget 120 s); poll: ide.test {"status": 3}
```

The `symbol` form first asks the live language server for the symbol's references, which takes
seconds on a cold session, so it answers `pending` at once and the started line (or
`tests: no tests reference …`) arrives through `ide.inspect`; `path`, `pattern` and `command`
answer inline. Start and running lines say `poll: call ide.test with {"status": N}` so the hint
cannot be mistaken for an `ide.inspect` detail reference. If that hint is passed to
`ide.inspect` anyway, the reply directs the caller to `ide.test {"status": N}`. Explicit
`command` runs with no parsed test counts keep the call inline briefly: a completed run answers
in the starting call with its exit code, up to 4 KiB of output, and `rerun:`; only a truncated
output includes `full output: ide.inspect <detail_ref>`. A longer run returns the ordinary
started line. Parsed test-runner replies are unchanged. Every start and running line ends with
the poll hint, and
`ide.inspect` with a test-run handle (`tests #N`, `tests-N`, `#N`, `N`) answers with that run's
status line, so a handle mistaken for a `detail_ref` still reaches the result.

Status appears in the status block and through `ide.test {status: 3}`:

```text
tests #3: 3 passed, 1 failed, 12 s
  FAIL assistance::worker::tests::stop_cancels_queue
       src/assistance/worker.rs:4012  assertion failed: left 5, right 6
  rerun: cargo test stop_cancels_queue      full output: ide.inspect test-3
```

Up to 16 failing tests are listed, each with its location and up to four message lines; more add one `(+N more failed tests in the full output)` line. The `rerun:` line reproduces the run exactly: an explicit command given `cwd` or `env` reads `rerun: cd <dir> && env NAME=VALUE … <argv>`.

A run that counted no test is never shown as `0 passed, 0 failed`: a non-zero exit reads `tests #3: no test results (exit 2), 1 s — runner said: <bounded runner line>; full output: ide.inspect <detail_ref>` (the runner could not run, e.g. `uv run pytest` without a usable environment), and a zero exit without a parsed summary reads `no summary parsed`. The `<agent-ide>` status line for the same run keeps the runner line but omits the reference, which the bounded block could otherwise cut.

Run at most one test job at a time per worktree. Stop a run when its budget expires, return its partial result and the command for manual execution, and page full output through `detail_ref`. `ide.stop` lists up to eight runs started in this binding whose results were never collected, including each run number and command (and whether it is still running). The IDE never starts tests on its own.

### 2.7 `ide.diff` — changed files (implemented)

Input: `{mode: head|staged|unstaged|task, paths?: [relative file or directory, …], detail_ref?, provenance?: false}`. `paths` accepts 1–16 literal worktree-relative paths, each at most 1024 UTF-8 bytes; absolute paths, empty/dot segments, `..`, and NUL are refused. Directories select descendants by path component, not a string prefix or Git glob. Selection happens before source reads and changed-path/source/patch budgets; comparison metadata remains bounded independently. Omitted `paths` includes the worktree. `task` compares the worktree with the commit recorded at activation, so it includes committed and uncommitted changes and current untracked paths passing Git's standard ignore rules. If activation could not record that commit, it directs the caller to `ide.diff {"mode": "head"}`.

Output is compact and contains no hashes:

```text
diff (head): 2 files, +14 −3
inventory: 2 tracked, 0 untracked, 0 conflicted; hunks delivered: 2
paths: "src/assistance/mod.rs", "src/lang/mod.rs"
file: "src/assistance/mod.rs"
@@ -12,6 +12,7 @@
 …
hunks: 3 more (ide.inspect diff-4)
```

The captured path inventory and its tracked/untracked/conflicted counts are separate from this page's delivered hunks. Names are bounded to five inline, then `(+N more)`; use `paths` to review the remaining inventory. The summary file count includes tracked changes and captured untracked text additions. Untracked regular UTF-8 text is captured without following symlinks and reviewed as additions against empty, without staging it. Its content is best-effort: a rewrite between reads becomes name-only (`changed during capture`) without invalidating tracked capture. Captured untracked contents remain a frozen snapshot on later pages. Binary, oversized, unreadable, symlink and special entries stay name-only with a reason. In `staged` mode, untracked names remain listed but have no content, hunks, file-count contribution or added-line totals. The existing caps apply: 256 selected paths, 64 KiB of names, 1 MiB per file, 8 MiB of total source/blob bytes and 1 MiB of patch bytes. Tracked bytes consume the budget first; remaining untracked content becomes name-only at a total cap.

Normal pages select fewer whole hunks until the serialized MCP envelope fits. A lone oversized hunk is split only at complete line boundaries; every part says `hunk N of TOTAL, lines A-B of LINES`, with a continuation or last-part marker. These line numbers count the original patch header and body. Concatenating the patch bytes of consecutive parts reconstructs the complete hunk. `ide.inspect` continues the same frozen, bounded snapshot; test runs do not consume a new detail slot for each diff page. Completed test-run metadata already retains at most four runs per worktree, so no unbounded output cache is added.

A single diff line that cannot fit is delivered as a bounded notice: `hunk N of T, line L of P: one line of B bytes is too long for one reply; read it with ide.read {path, lines}`. Its cursor advances past the line, so later changes remain reachable; this notice explicitly replaces those raw bytes. A path inventory or recovery notice that cannot fit says `capacity` and directs the caller to narrower `paths`. When aggregate continuation retention is full, the fitter reserves the actual recapture trailer before accepting the page. The reply has no continuation, its part label says remaining lines require recapture, and its already selected bytes are never shrunk by final rendering. Queue saturation says to wait for pending work. A full result store first releases the requester’s settled results, then other actors’ results above their eight-result floor; settled results of actors idle for over 15 minutes are reclaimable. Pending and pinned results stay protected. If the store remains full, `worker:actor_share_full` says the remaining capacity is held by other active actors.

`provenance: true` returns today's exact hash-bearing header instead (worktree/comparison identity, capture generation, per-path lists) — kept for debugging, never the default.

If the exact two-pass capture cannot prove a consistent read (for example a concurrent checkout, or a file that changed since a still-recorded observation of it), a single-pass plain `git diff` answers instead, marked on every page: `diff (head): 2 files, +14 −3 (plain git diff; exact capture unavailable: snapshot unstable or a file changed since it was observed)`. This degraded capture keeps unknown freshness and uses the same bounded plain/task pager, including untracked text and line parts. It runs without rename detection (a rename shows as deletion plus addition), attributes each hunk by its decoded `diff --git` path, and refuses output that cannot be attributed exactly. Every captured content path passes no-follow in-root confinement; name-only untracked symlinks disclose no bytes and do not veto continuation. Its combined path, source and patch budgets are the same as the ordinary capture, and later pages check captured source fingerprints without claiming an atomic Git window.

Provenance reports `current_tree: captured just now` on the first page. Later non-staged pages recheck tracked worktree bytes only. Untracked text remains the captured snapshot; staged pages, including the degraded path, never recheck worktree contents. They do not recheck commits, staging, or the untracked name inventory; after those change, call `ide.diff` again. Delivery does not recheck Git state. If `ide.inspect` finds a tracked source changed after capture, it refuses the stale page.

An untracked symlink or other special entry (for example `node_modules ->` a sibling checkout) is listed by name only — its bytes and, for a symlink, its target are never read — instead of refusing the whole diff.

If a requested capture exceeds its budget, narrow it with `paths`, select `head`, `staged`, or `unstaged`, or review it with native Git.

In a plain directory, `ide.diff` answers `source_unavailable (diff:not_a_git_repository)` with `not a git repository: no git data`.

### 2.8 `ide.problems` — project or file diagnostics (planned cleanup)

Input: `{scope: "project"|path, language?}`.

Output uses the current `file:line:column code message` form, grouped by file and capped at 20 entries followed by “N more”.

### 2.9 Unchanged tools

`ide.inspect {detail_ref, page?}` and `ide.stop {}`: an unknown `detail_ref` says which it is — `this detail_ref was never issued` for a reference this daemon could not have minted, `this detail_ref has expired` for one it minted and no longer retains — and a test-run handle (`tests #N`, `tests-N`, `#N`, `N`) answers with that run's status line instead of failing the lookup. A poll-hint string passed as `detail_ref` answers with the specific `ide.test {"status": N}` call to make. A test-run handle keeps answering read-only for the run's retained lifetime (up to 10 minutes) even after `ide.stop`, without the `full output` line; every other `detail_ref` ends with the session, and a retained run's output detail is never evicted while the session lasts. `ide.stop` names a durable failure by its typed cause instead of `workspace_authority`: `capacity (stop:busy)` (the state store stayed busy or locked; the daemon retried the idempotent revoke by its operation id twice inline and keeps retrying it in the background, one second doubling to sixteen), `capacity (stop:store_full)` (the receipt store is full), `deadline (stop:store_deadline)` (the wait expired after the work was accepted), `workspace_authority (stop:store_unavailable)` (the store could not record it) and `workspace_authority (stop:authority)` (a conflict with the recorded activation); the grant stays held and the daemon retries it itself with the same operation id — between jobs and when idle, one second doubling to sixteen, and again with the next start of the same worktree or actor — so the agent never has to repeat the stop. `ide.stop` also lists uncollected runs as described above, and accepts an optional `activation_id` for symmetry with `ide.start` (it always ends this session's activation). A run its stop retired is never announced on a later activation's plates; its status still answers by handle. `ide.context` in its current form is retired; its role is divided among `outline`, `symbol`, `read`, and `problems`.

`ide.context {path}` with no `byte_offset` (T163, W5) still pages the whole file exactly as before, for compatibility with existing hosts and acceptance scripts that mint an edit `source_ref` from it; its first line now leads with a one-line hint toward the bounded alternative: `hint: ide.outline {"path"} gives the skeleton and ide.read {"path","lines"} a bounded region; this whole-file view stays for compatibility`. `ide.context {path, byte_offset}` (the semantic query used by the edit-diagnostics and staleness flow) and `ide.context {kind:"problems"}` are unaffected and carry no hint; prefer `ide.outline`/`ide.read` for new work.

## 3. `LanguageSupport` contract

| Method | Returns |
|---|---|
| `detect(root) -> Option<Project>` | Manifests, environment (interpreter/`.venv`, node, toolchain), and build/test/lint/fmt/typecheck commands with their source (CI, manifest, README). |
| `server_command(project, cache) -> Command` | Binary, arguments, and environment as used by the editor. |
| `readiness(event) -> Ready|Loading(eta)|Error(msg)` | Readiness from `serverStatus`, progress, or the first diagnostic. |
| `normalize_symbol(doc_symbol, source) -> Symbol` | Range including the header, `Owner/name` path, and kind (fn/struct/class/method/…). |
| `skeleton(file, symbols) -> Outline` | Signatures and the first docstring line; test modules are collapsed. |
| `insert_site(file, anchor, where) -> Site` | Insertion point, indentation, and blank lines. |
| `tests_for(symbol|path) -> TestSelection` | Test list and filtered test command. |
| `parse_test_output(bytes) -> TestReport` | Passed/failed status and first error with location. |
| `format(file) -> Option<Command>` | Project formatter, if available. |
| `problems_command(project) -> Command` | As today: `cargo check`, `pyright`, `tsc`, or `go vet`. |

Languages: `rust`, `python`, `typescript`, `go`. Start with modules from a single crate; support crates after one week.

## 4. Non-goals

- Do not run tests automatically in the background or run the full suite without an agent request.
- Do not add a project summarization model.
- Do not embed codegraph; remove it after `ide.symbol`/`ide.outline` are available.
- Do not commit automatically or perform Git actions beyond reading data for `diff` and `history`.

## 5. Implementation order (estimate)

1. Long-lived language server session per worktree, symbol paths, and Rust `ide.outline`, `ide.read`, `ide.symbol` — 3 days.
2. Symbol-based `ide.edit` (replace/insert/delete/rename) and diagnostics afterward — 2 days.
3. `ide.start` project card, `ide.problems`, and `ide.diff` cleanup — 1.5 days.
4. `ide.test` — 1.5 days.
5. Python, TypeScript, and Go support under `LanguageSupport` — 1–1.5 days each.

Total: about two weeks; Rust is fully implemented by the end of the first week.

## 6. Approval questions in the source draft

1. Is the symbol path using `#` for the file and `/` for nesting accepted?
2. Is the project card in the `ide.start` response (with `ide.project` to retrieve it again) accepted?
3. Are the ceilings accepted: 16 KB per response, 30 usage lines, and a 120-second test budget?
