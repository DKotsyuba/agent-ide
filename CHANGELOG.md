# Changelog

## Unreleased

## 0.10.2 — 2026-10-05

### Fixed

- Cache cleanup no longer stops after an upgrade while sessions of the previous release are
  still open: every release after 0.9.1 takes leases, so a sweeper trusts older ones too. It
  used to treat any release older than itself as lease-unaware and keep every check and
  telemetry cache until those sessions ended (29 GiB kept against a 20 GiB budget).
- A Rust file whose workspace failed to load and that the source outline also refuses is
  refused with `use native reads`; it used to say `ide.outline` and `ide.read` still answer
  from source, which was false for that file.

### Changed

- The bundled `agent-ide` skill matches the 0.10 tools: sixteen listed test failures and
  the reproducing `rerun:` line, edit diagnostics split into `new:`/`pre-existing:` with the
  `project errors in other files` and `not_analysed` next steps, rename and batch limits,
  Python environments and its missing call hierarchy, `unsupported_file`, diff paging by
  parts and `paths`, page markers instead of a `continuation` field, and the exact retry
  facts. The bundled reviewer agent stops its activation before reporting.
- The check caches' budget is 15 GiB (was 20 GiB): least recently used worktrees past it are
  removed and their next project check builds again from cold.

## 0.10.1 — 2026-10-05

### Fixed

- A Rust file with a plain comment separated from the next item by an empty line (a
  `// xtask:…` marker, a section rule) gets its outline, symbol reads and edits while
  waiting for rust-analyzer; the source outline used to refuse it and answer `provider_unavailable`.
- A symbol address naming both a field of a type and a method of one of its blocks
  (`PathSnapshot/source`) resolves to the method; it used to pick the field silently, so an
  insert or replace aimed at the method landed inside the type declaration.
- Two `old` text changes in one `ide.edit` batch overlap only when their matched bytes
  intersect: separate substrings of one line (parts of one string literal) apply together, right
  to left, with exact landing lines; they used to be refused as overlapping lines.
- An edit that leaves its file clean while the project check reports errors in other files says
  so (`current_clean for this file; the project check reports N errors in other files`) and points
  at `ide.context` problems instead of `ide.diff`.
- `ide.diff` no longer fails `capacity` on a hunk larger than one reply: the hunk arrives in
  line-bounded parts on consecutive `ide.inspect` pages (`hunk N of T, lines A-B of L`), and a
  single line too long for any reply becomes a notice with the `ide.read` that shows it, after
  which paging continues. Capacity refusals name the resource that is full.
- `ide.diff` accepts `paths` (literal worktree-relative files or directories) and shows bounded
  untracked text as additions, without staging it. Untracked content is a best-effort snapshot:
  a file that changes during capture stays name-only and never breaks the diff. `staged` mode
  lists untracked names only, as before.
- A test run's `rerun:` line reproduces the run: `cd <dir> && env NAME=VALUE … <argv>` when
  `ide.test` was given `cwd` or `env`.
- A failed run lists up to 16 failing tests (was 8) and says how many more are in the full output
  instead of dropping them silently.
- `ide.edit` inserts count the blank lines already beside the insertion point toward the
  spacing, so inserting next to a neighbour that is already separated (two blank lines before a
  comment, say) no longer doubles the separation; the landing line numbers follow.
- Readers whose activation predates the current writer can borrow its live language session:
  `ide.outline`, symbol reads, symbol cards and semantic context retain the reader's source
  authority instead of failing on the writer's different epoch, including nested Python packages
  without a virtual environment.
- Python usages cross roots that import each other by name only: `ide.symbol` on a method of a
  package in one nested root (`libs/contracts`) now counts its calls in another root that
  reaches it through `PYTHONPATH` (`services/agent` with just a `requirements.txt`, no
  environment installing the package). The Pyright session receives every nested Python root
  (its `src` under a src layout) as `python.analysis.extraPaths`; before, the import was
  unresolved there, the module-level instance had no type and its method calls were never
  references. A project config file's own `extraPaths` still wins.
- `ide.edit` on a file the project check cannot analyse (a Python package with no environment,
  a check that could not run) now answers the language server's diagnostics for the exact
  post-edit source as `current_reported`, labelled as language-server diagnostics with the
  check's reason, or `not_analysed (<reason>)` when the server has none — it used to drop them
  and answer `unknown`. Language-server diagnostics in edit replies and `ide.context` carry
  `path:line:col severity [code]`.

## 0.10.0 — 2026-10-05

### Added

- Automatic cache retention (`docs/cache-retention.md`): each daemon sweeps `~/.agent-ide` 60 s
  after start and then hourly (one sweep per hour machine-wide). Check caches go when their
  worktree is gone, idle 7 days, or least recently used over a 20 GiB budget; telemetry stores
  when gone, idle 30 days or over 1 GiB; installed releases other than `current`, the newest
  three, those installed in the last 14 days and those a live process executes. Every
  activation, check and test run holds a shared lease on its worktree, a sweep claims a cache
  only with an exclusive lock and renames it to trash before deleting, and nothing in checks or
  telemetry is removed while an older (lease-unaware) `agent-ide` process is alive. Removals
  are logged as `retention` events with the bytes freed.
- `agent-ide cache status` (dry run) and `agent-ide cache prune` (apply now).

### Changed

- The lock-free start-up sweep of gone worktrees' check caches is replaced by the leased sweep;
  `ide.start`, project checks and `ide.test` refuse to run when the worktree's retention lease
  cannot be taken (`start:cache_lease`).

## 0.9.1 — 2026-10-05

### Fixed

- `ide.outline`, `ide.read`, `ide.symbol` and `ide.graph` on a file no IDE
  language reads (`Cargo.toml`, a shell script) answer
  `unsupported_file: <path>` with the read that works (`ide.read` `path` and
  `lines`) instead of a misleading `provider_unavailable`; a batch read names
  such a file per item.
- An edit of a Rust file reached only behind a gate — an integration test with
  `#![cfg(feature = "…")]`, a `cfg`-gated `mod` — says it is built only under
  that condition instead of advising to declare it. A `#[path = "…"]`
  declaration naming a file beside the declaring file now reaches it, so such
  files get their project check. `not_analysed` reasons carry their own next
  step; Python's name the missing root or environment.
- An edit of a file no language checks (a template, a manifest) answers
  `not_analysed (no IDE language checks this file type)`; `unknown`
  diagnostics point at `ide.context` with `kind: "problems"`.
- A test run stopped with its activation no longer appears on the plate of the
  next activation in the same session; its status still answers by number.
- `ide.stop` accepts an optional `activation_id` instead of refusing it.
- `ide.symbol`/`ide.graph` with a bare Rust function or method name find every
  definition: rust-analyzer's workspace symbol search now covers all symbol
  kinds, not types only.
- `ide.test {"path": …}` for a Rust file runs in that file's package
  (`--manifest-path <package>/Cargo.toml`) instead of building every target in
  the workspace; a file under `src/tests/` is a module, not an integration test.
- A failed Rust test's summary keeps up to four message lines (an assertion's
  `left:`/`right:` values), not just the first.

## 0.9.0 — 2026-10-04

### Added

- Environment selection for Python. Each project root has one current
  environment; the `ide.start` card shows it with its source (selected,
  pinned by `pyrightconfig.json`/`[tool.pyright]`, or discovered), its version
  from `pyvenv.cfg`, and the other candidates with a ready `choose:` hint.
  `ide.start {"environment": {"python": ".venv-py314"}}` switches it
  (`python:<root>` for a nested project root), `auto` resets it. The choice is
  kept per worktree across daemon restarts and is not inherited by a
  recreated worktree. A reader activation cannot change it.
- One resolver answers for the card, the project check, the Pyright session,
  test and format commands and the syntax probe. A change of the environment
  (selection, recreation, disappearance) restarts the Pyright session, reruns
  the check and shows a one-shot line on the plate.
- Test and format commands run from the resolved environment
  (`<venv>/bin/python -m pytest|black|ruff`) without `uv run`; explicit
  commands get the environment's `bin` on `PATH` and `VIRTUAL_ENV`.
  `uv run` stays only when no environment resolves at all.

### Changed

- Suffixed environments (`.venv-py314`) are listed in name order and count as
  a Python project marker on their own. A `pyrightconfig.json` without venv
  keys no longer lets `[tool.pyright]` pin an environment.
- A missing selected or pinned environment is never silently replaced: the
  check, tests, format and probe report the cause and the way out (recreate,
  `auto`, or edit the pin). A broken environment (base interpreter gone) stays
  listed and cannot be selected.

### Known limitations

- Nested project roots with diverging environments run tests and formatting in
  the worktree root's environment; per-root routing comes with Rust toolchain
  selection.

## 0.8.1 — 2026-10-04

### Added

- Reader and writer activations: `ide.start {"read_only": true}` admits any
  number of readers beside the single writer of a worktree; every call that can
  change code (all `ide.edit` forms, `ide.test`) is refused for a reader before
  any work, with one uniform message. A second writer is refused with the
  holder's activation, start time and last activity. `activation_id` is optional.
- `ide.test` explicit commands accept a worktree-relative `cwd` and bounded
  `env`; the 16 KiB limit applies to the whole argv.
- Python projects in nested directories (no root manifest) are discovered and
  listed on the card with checks per root; suffixed environments such as
  `.venv-py314` are found and named; usages cover sibling packages.
- The plugin ships a read-only `ide-reviewer` agent with the IDE read tools.

### Changed

- Edit diagnostics carry `path:line:col` and separate problems the edit
  introduced from those present before (shifted lines stay pre-existing).
- A clean git baseline reads `git <sha> (clean)` on the start card.
- Outlines show declaration lines; the Rust lexical outline names inline
  struct-variant fields.

### Fixed

- Switching a live start between reader and writer roles keeps its durable activation bound to the active host binding; problem checks return the current running state without stalling other queued work; the bundled reviewer starts read-only.
- A fresh read is no longer evicted before the edit that uses it; a stale
  refusal never hints the reference it just refused.
- Formatting an edited Rust module never rewrites its child files (regression
  test).
- The first `ide.start` waits longer for its hook instead of refusing
  `missing_pre`; never-activated sessions are told to start; a stop after lost
  authority answers "nothing active"; worktree resolution failures name their
  stage.
- A missing Python environment is reported once and not re-probed until inputs
  change; import noise collapses to one line. `.tsx` outline and read fall back
  to source when module resolution is unverified.
- Known runner summaries (pytest, cargo test, vitest/jest, go test) are parsed
  whatever launched them; other commands report their exit code and output
  tail; status plates show the starting actor's own run.
- Refusals name the cause and a working next step: reads past the end of a
  file, stageless read failures, `ide.outline` without `path`, old-text edits
  without `source_ref`, duplicate symbol candidates; problems context waits
  briefly for a running check.
- A same-version source reinstall (`scripts/install-local.sh`, `self-install
  --replace`) restages the plugin instead of keeping the previous one.

## 0.8.0 — 2026-10-03

### Changed

- MCP 2026-07-28 ("modern": no `initialize`; every request carries its
  protocol version and client capabilities in `_meta`) is served on
  `rmcp =3.4.0` (from 3.2.0). Modern `tools/list` now returns
  `resultType=complete`, `ttlMs=60000` and `cacheScope=private`, which
  Claude Code requires or it drops every tool; legacy sessions keep their
  exact original wire (no `ttlMs`, `cacheScope` or `resultType`).
- Host identity never depended on the handshake: the Codex-vs-Claude
  envelope, the tool-call correlation and the managed-Claude hook pairing
  are all selected from per-request `_meta`, so modern sessions are
  unchanged. Raw-wire tests pin each decision in both eras (modern
  tools/list, legacy tools/list, modern `server/discover`, a modern
  fail-open `tools/call`, a modern managed-Claude paired call and a modern
  Codex call with `structuredContent`).

## 0.7.0 — 2026-10-02

### Added

- The agent-* family shape: `AGENTS.md`/`CLAUDE.md`, this changelog,
  `SECURITY.md`, `LICENSE` (MIT), `docs/MCP_RESPONSE_STANDARD.md` and
  `docs/FAMILY_CONTRACT.md`, `docs/qualification.md`, `family.toml` with
  `.family/` template provenance, workspace Cargo hygiene (resolver 3,
  `rust-version`, license, shared lints, hoisted dependencies), `deny.toml`
  with a CI supply-chain job and Dependabot, the `xtask` gate
  (`cargo xtask check`, `standard check`, `contract check|update`), the
  exported `schemas/tools.json` tool-contract snapshot with drift tests, and
  SHA-pinned actions with concurrency groups in both workflows.
- The family release flow: `cargo xtask package` (byte-identical to the
  former shell packager) and `package verify` (the release smoke test plus
  the manifest binding), `release prepare` (local version/CHANGELOG edits,
  preview by default), `release manifest`, `release publish` (draft → verify
  → publish, never overwriting) and `release wait` (REL-02 observer, wrapped
  by `scripts/wait-release.sh`). Releases gain the `release-manifest.json`
  and `acceptance.json` assets; the CI product acceptance runs on the
  packaged executable and names the archive's SHA-256.

### Changed

- The release workflow is split into a read-only build job and a tag-only
  publish job (`release` environment); `workflow_dispatch` with `dry_run`
  exercises the build job on any ref. `scripts/package-release.sh` and
  `scripts/release-smoke.sh` are thin wrappers over the xtask commands, and
  `xtask` carries its own version (0.1.0) so a release bump touches only the
  `agent-ide*` packages.
- `initialize` names the product: `serverInfo` is `agent-ide` with its
  version and title `Agent IDE` (it used to report the rmcp SDK).
- Every tool carries truthful MCP annotations: the read tools are read-only,
  `ide.start`, `ide.stop` and `ide.edit` are idempotent by their durable
  receipts, `ide.test` claims nothing beyond the defaults.
- An executed edit, stop or activation whose reply cannot be presented still
  reports its outcome, path, `operation_id` and whether it may be repeated.
- The reply engine registers only the helpers the template uses and runs
  under a fuel budget; paths and messages are escaped so they cannot forge
  reply lines.

## 0.6.9 — 2026-10-01

### Added

- 0.6.7's one-call `ide.read` (several symbols or line ranges in one reply,
  one shared `source_ref`) and the validated `ide.edit` batch form
  (`changes`: 1–32 edits to one file, every address resolved against the
  named source version before anything is written).
- CI-portable tests: the Python gate test finds `python3` on `PATH` instead of
  a developer's local interpreter.

Older versions are listed on
[GitHub Releases](https://github.com/DKotsyuba/agent-ide/releases).
