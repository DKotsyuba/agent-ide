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
| `ide.diff` / `ide.problems` cleanup | planned |
| Live language server sessions for Python and TypeScript (one per binding and language) | implemented (Go planned) |

## 0. Principles

1. **Address symbols**, not lines or offsets. Lines are a fallback mode and are used in read output.
2. A tool returns information an agent cannot get from `grep` or `cat` in a second: types, documentation, relationships, compiler errors, and project commands.
3. **Keep responses compact.** Responses contain no internal bookkeeping fields (`authority_epoch`, hashes, generations). Lists have ceilings and report “N more”; retrieve the complete list through `detail_ref`.
4. Errors use one line with the reason and next step, for example `error: ambiguous_symbol; candidates: …`. Every failure also names its closed stage — `<tool>:<stage>` (`diff:too_large`, `symbol:anchor_missing`) — in parentheses after the reason when one is set, and the same tag lands in the daemon journal's `detail` field; a failure with no specific tag derives `<tool>:<reason>`, so no journal line is ever detail-less. Stage tags carry no payloads, paths, or user text.
5. The language server runs as it would in a human's editor and remains alive for the session.
6. Every host uses one path: return the result directly when it arrives within about 10 seconds; otherwise return `pending` and use `ide.inspect`.

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
- To search by name without a file, use `#BindingStatus` or `BindingStatus`. The IDE searches the project and returns candidates when there is more than one; it does not guess:

```text
error: ambiguous_symbol; 3 candidates:
  src/assistance/host_binding.rs#HostBindingGuard/new
  src/execution/mod.rs#AdmissionController/new
  src/workspace/git.rs#GitIdentity/new
```

- Symbol boundaries include the header: docstrings, attributes, decorators, and JSDoc are part of the range for reads, replacements, and deletions. The language crate normalizes differences between servers.

### Lines and coordinates in output

- Lines are 1-based, as in editors and compilers; locations use `file:line[:column]`.
- Code fragments include line numbers on the left.

### Readiness and waiting

- Return a result directly if the server responds within about 10 seconds. Otherwise return `pending: use ide.inspect with detail_ref X`. `ide.inspect` waits up to 10 seconds before returning `pending` again.
- While a language server is starting, symbol tools return `error: provider_loading; retry in ~N s` with an estimate instead of waiting silently for minutes.

### `<agent-ide>` status block

A hook or the top of a response may include check and background-test status, in no more than 6 lines:

```text
<agent-ide>
rust: 0 errors, 0 warnings
tests #3: running 42 s — cargo test worker::
</agent-ide>
```

### Ceilings and pages

- A response is at most 16 KB. Lists are capped at 30 usage lines, 20 caller lines, and 20 diagnostic lines. Any remainder is reported as “N more” with a `detail_ref`.
- `ide.inspect {detail_ref}` retrieves pages; `ide.inspect {detail_ref, page}` retrieves a specific page.
- A symbol card with more than 30 usages, or an ambiguity list with more than 20 candidates, keeps the cut rows in its detail. The first page is the card or list as before, ending with `… N more (ide.inspect <detail_ref>)`; each later `ide.inspect` returns the next page of the remaining rows (`usages 31–N of N:` or `candidates 21–N of N:`, same row format). Cards and lists within the ceilings are unchanged. The first `ide.inspect` after an inline reply always returns page two, never page one again, for every paged tool.

### Permissions

The IDE's configured `allowed_roots` is the sole path rule and remains unchanged.

## 2. Tools

### 2.1 `ide.start` — activation and project card (implemented, see status table for gaps)

Input: `{activation_id, root?}`.

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

Commands come from CI (`.github/workflows`), `Makefile`/`justfile`, or manifests. If a command comes from README, mark it `(README)`. Layout lists only first- and second-level directories with file counts. Cache the card for the session; `ide.project {}` returns it again.

Implemented wire form: the activation reply keeps its first line (`Workspace activated; authority_epoch: …`) and appends the card after a blank line. The card is computed under a 5 s budget off the runtime; when it does not fit the budget the reply is the plain activation text. Commands print one line per kind with the provenance in the heading (`commands (ci, manifest):`), `environment:` names the toolchain and edition, `servers:` prints `not started` for each detected language (the daemon does not probe servers at start), and there is no `ide.project` yet — call `ide.start` again to see the card. Symbol requests on a Rust worktree where the root is not a Cargo workspace also load nested crates (two directories deep, excluding test material), so references across such a crate's own tests resolve.

Errors: `outside_allowed_roots` (as today), `not_a_project` (no manifest is present; continue in files-only mode).

### 2.2 `ide.outline` — file skeleton (implemented)

Input: `{path, depth?: 1|2|all (default all), bodies: false}`. A `path` naming a directory (trailing slash optional) answers a directory outline instead: subdirectories with file counts, then files with line counts and the first documentation line (`//!` for Rust, the module docstring for Python, the leading comment for TypeScript/JavaScript), one level deep, at most 200 files.

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

### 2.3 `ide.symbol` — symbol card and relationships (implemented)

Input: `{symbol, usages?: true, callers?: 0..3, callees?: 0..3, tests?: true, history?: 0..10}`. Defaults: usages, callers 1, tests, and history 3. `history` is planned.

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
  … 16 more in src (ide.inspect sym-12 page 2)
tests: 44 usages in 6 test files — tests/assistance_binding_contract.rs (21), tests/execution_contract.rs (7), …
callers (1 level, for functions): —
history: 3 last commits touching lines 330–346
  ffb25c2 2026-09-25 refactor(assistance)!: serve Claude through the daemon route …
  bbd58f6 2026-09-25 feat(assistance)!: allowed_roots gate …
```

For functions, include `callers` (with locations) and `callees` to the requested depth, capped at 20 lines per level. Read function bodies with `ide.read`.

Errors: `ambiguous_symbol` (candidates), `unknown_symbol` (similar names: “did you mean”), `provider_loading`.

### 2.4 `ide.read` — symbol body or line range (implemented)

Input: `{symbol}` or `{path, lines: "120-180"}`.

Output is code with line numbers and the header included:

```text
src/assistance/host_binding.rs#HostBindingGuard/establish_start  (lines 906–931)
906  /// Establishes one exact start binding after a trusted pre-observation …
907  pub fn establish_start(&mut self, candidate: Candidate, channel: ChannelRef) -> BindingStatus {
908      let invocation = (candidate.host, candidate.actor_id.clone(), channel.clone());
…
931  }
source_ref: sym-14
```

`source_ref` is the reference currently called `detail_ref` by edit: it binds to the read content. A write using a stale reference is rejected as `stale_source` if the file has changed.

### 2.5 `ide.edit` — edit a symbol or range (implemented)

Input operations:

```text
{op: "replace", symbol: "src/index.ts#ClassImpl/method", content: "…", source_ref?: "sym-14"}
{op: "insert",  symbol: "src/index.ts#ClassImpl/A", where: "before"|"after", content: "…"}
{op: "insert",  symbol: "src/index.ts#ClassImpl",   where: "first"|"last",   content: "…"}
{op: "delete",  symbol: "src/index.ts#ClassImpl/old"}
{op: "rename",  symbol: "src/index.ts#ClassImpl/method", new_name: "run"}
{op: "replace", path: "src/index.ts", lines: "1-12", content: "…"}          // fallback mode
```

For a symbol, `content` is the complete symbol including its header. The IDE derives indentation and blank lines from neighboring code. After writing, the project's formatter runs over the candidate (before the write), then the project check (cargo check / pyright / tsc) is scheduled at once and the reply carries the edited file's problems from it. `rename` is performed by the language server across the project.

Output (implemented wire form):

```text
edit: replaced; path src/lang/path.rs; source_ref …-3; diagnostics: current_reported (project check 1.8s: 1 errors, 0 warnings in this file)
src/lang/path.rs:132:9 error [E0308] mismatched types
Next: use ide.edit with source_ref …-3
```

```text
edit: replaced; path src/lang/path.rs; source_ref …-4; diagnostics: current_clean. Next: use ide.diff
```

`current_clean` is only ever derived from a completed project check that named no problem in the file; a language server's empty publish is not taken as proof. A provider report for the exact post-edit version (an error rust-analyzer or tsserver found on its own) is kept as it arrives instantly. The reply waits at most 90 s for the check, then says `diagnostics: unknown` and the result reaches the next `<agent-ide>` block or `ide.context`. The `rerun tests:` hint arrives with `ide.test` (§2.6).

For `rename`:

```text
rename: method → run; 7 edits in 4 files: src/index.ts (3), src/api.ts (2), test/index.test.ts (2)
diagnostics: clean
```

Errors: `stale_source`, `ambiguous_symbol`, `unknown_symbol`, `syntax_error` (the edit is applied, but the file does not parse; report the error and do not roll back).

### 2.6 `ide.test` — run tests on request, in the background (implemented)

Input: `{symbol}` | `{path}` | `{pattern}` | `{command}`, with optional `budget_s?: 120`.

- `symbol` selects tests that reference the symbol (using the language crate); `path` selects tests for a file/module; `pattern` filters through the test runner; `command` is an exact project command.

Immediate output:

```text
tests #3: started — cargo test --workspace worker::  (4 tests selected, budget 120 s)
```

The `symbol` form first asks the live language server for the symbol's references, which takes
seconds on a cold session, so it answers `pending` at once and the started line (or
`tests: no tests reference …`) arrives through `ide.inspect`; `path`, `pattern` and `command`
answer inline.

Status appears in the status block and through `ide.test {status: 3}`:

```text
tests #3: 3 passed, 1 failed, 12 s
  FAIL assistance::worker::tests::stop_cancels_queue
       src/assistance/worker.rs:4012  assertion failed: left 5, right 6
  rerun: cargo test stop_cancels_queue      full output: ide.inspect test-3
```

Run at most one test job at a time per worktree. Stop a run when its budget expires, return its partial result and the command for manual execution, and page full output through `detail_ref`. The IDE never starts tests on its own.

### 2.7 `ide.diff` — changed files (planned cleanup)

Input: `{mode: head|staged|unstaged, path?}`.

Output is compact and contains no hashes:

```text
diff (head): 2 files, +14 −3
src/assistance/mod.rs  +1
@@ -12,6 +12,7 @@
 …
tests/support/git_snapshot.rs  +13 −3   diagnostics: clean
```

For files above the response ceiling, return the header and `hunks: N, ide.inspect diff-4`. For large binary or generated files, return only the filename.

### 2.8 `ide.problems` — project or file diagnostics (planned cleanup)

Input: `{scope: "project"|path, language?}`.

Output uses the current `file:line:column code message` form, grouped by file and capped at 20 entries followed by “N more”.

### 2.9 Unchanged tools

`ide.inspect {detail_ref, page?}` and `ide.stop {}` remain unchanged. `ide.context` in its current form is retired; its role is divided among `outline`, `symbol`, `read`, and `problems`.

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
