# agent-ide — agent instructions

A resident stdio MCP server for Codex and Claude Code on macOS arm64: one
language-server-backed tool surface over Rust, Python, TypeScript/JavaScript,
CSS and HTML (Go support was removed in 0.10.8).
`docs/architecture.md` is the behaviour truth source; the machine-readable tool
contract is `docs/contracts/tools-v0.4.md` with the exported snapshot in
`schemas/tools.json`.

## Single workflow

`cargo xtask check` is the full non-mutating gate; run it before claiming
done. It runs fmt `--check`, the locked workspace tests with
`--test-threads=1` (built with the `test-seams`
cargo feature that the seam-driven product tests need, plus one default-build
run proving a release build ignores every `AGENT_IDE_TEST_*` seam), the four ignored
real-provider product tests, clippy and rustdoc with `-D warnings`, the
release build, the family standard checks and the contract snapshot check —
with `--no-fail-fast` on the workspace tests so one failing test binary does
not hide the rest. The real-provider tests read their toolchains from the
`AGENT_IDE_*` environment exactly as CI prepares it; nothing defaults to a
developer's local paths.

`cargo xtask standard check` verifies the declarative family invariants
(Cargo identity, edition/resolver, shared lints, `family.toml` fields, required
files, prohibited non-Rust tooling outside the declared exceptions).
`cargo xtask contract check|update` compares (or explicitly regenerates)
`schemas/tools.json` against the `tools` array of the real built binary's
`tools/list` result: the snapshot is exported, never hand-edited, and `check`
fails with a contract drift message when the two diverge.
`tests/contract_snapshot_contract.rs` asserts the same equality under plain
`cargo test`.

Release commands (the full flow is `docs/release.md`):

| Command | Effect |
|---|---|
| `cargo xtask release prepare X.Y.Z [--apply]` | preview (default) or write the version edits: workspace version, three plugin manifests, `agent-ide*` lock entries, CHANGELOG heading; never commits, tags or pushes |
| `cargo xtask package BINARY vX.Y.Z [OUT]` | seal the archive bundle and write the tarball + `SHA256SUMS` (`scripts/package-release.sh` wraps it) |
| `cargo xtask package verify ARCHIVE` | the release smoke test, plus manifest ↔ files ↔ `SHA256SUMS` when a manifest sits next to the archive (`scripts/release-smoke.sh` wraps it) |
| `cargo xtask release manifest DIR` | CI only: write `release-manifest.json` and the aggregate `SHA256SUMS` |
| `cargo xtask release publish DIR` | CI only: draft with all assets → download-back verify → publish; refuses an existing release or draft |
| `cargo xtask release wait --repo … --tag … --commit … [--run-id N]` | observe one exact tag/commit/run and verify its assets; never installs (`scripts/wait-release.sh` wraps it) |

## Module map

| Files | Scope |
|---|---|
| `src/main.rs`, `src/lib.rs` | the `agent-ide` binary: command dispatch (`daemon`, `mcp`, `managed-mcp`, hooks, `doctor`, `init`, `self-install`, …), language registration, re-exports |
| `crates/agent-ide-core/src/workspace/` | workspace authority: actor/worktree exclusivity, generations, Git snapshots, durable state |
| `crates/agent-ide-core/src/intelligence/` | LSP client and server seam, view/backend reuse, semantic context, the cross-language name index |
| `crates/agent-ide-core/src/changes/` | versioned patch/rename application, recovery journal, diff/finish |
| `crates/agent-ide-core/src/execution/` | bounded admission, jobs, cancellation/reaping, seatbelt confinement |
| `crates/agent-ide-core/src/assistance/` | the host-specific MCP facade, host binding, Codex/Claude adapters, replies and hooks |
| `crates/agent-ide-core/src/checks/`, `feed/` | confined background project checks and the problem feed |
| `crates/agent-ide-core/src/app/` | daemon lifecycle/IPC, launcher configuration, SQLite store, transport |
| `crates/agent-ide-core/src/lang/` | the language contract: registry, outlines, edits, rendering, name facts |
| `crates/agent-ide-lang-*` | one crate per language; each depends only on the core |
| `xtask/` | the development gate, standard checks, contract export and the release flow (std + `serde_json`; own version) |
| `scripts/`, `install.sh`, `.github/workflows/` | acceptance drivers, packaging, the installer, CI and release |

The core depends on no language crate and names no language; a language crate
depends only on the core; only the root package wires the languages in. Core
unit tests enforce both rules.

## Safety invariants

- The only path policy is the launcher `allowed_roots` list: `ide.start`
  admits a session whose working directory (and Git worktree) lies inside a
  configured root, and refuses `outside_allowed_roots` otherwise. No host
  sandbox is replayed or captured.
- stdout of `mcp` is protocol-only; secrets and host metadata never enter
  replies, diagnostics or logs (the TrustedTransport Debug impl is redaction-
  tested).
- `ide.edit` accepts an optional `operation_id`: a repeated supplied id never
  applies the edit twice, and an omitted id is generated. Read and edit use
  symbol/region addresses; current text and delivered-window guards protect
  freshness without a required file-version reference. Overlapping or stale
  changes are refused atomically with nothing written.
- Replies are written for agents: bounded pages with `detail_ref` continuation,
  closed vocabularies, `isError` on every business refusal, results of long work
  pushed in `<agent-ide>` blocks (never a poll prompt; `ide.inspect` reads them
  early), honest `unavailable`/`outcome_unknown` results — never fabricated
  success. A failed IDE never vetoes native host tools.
- Claude runtime and hint children use the common operator-selected
  `AGENT_IDE_CLAUDE_RENDEZVOUS_ROOT` parent, keyed by the canonical Git common
  directory; an invalid parent fails without falling back. The unset legacy
  default remains `/private/tmp`; project work selects its approved temporary
  parent explicitly. Ownership and staleness are checked
  before adoption and an MCP exit never stops a healthy daemon.
- Project checks run confined (`sandbox-exec`, no network, read-only worktree,
  allowlisted environment) and are fail-open. When the host already confines the
  daemon and refuses a nested profile, the check still runs, under the host's
  confinement only (`docs/assistance-launcher.md`).

## Invariants and delivery

Rust 2024, resolver 3, pinned toolchain 1.98.1, committed application lock,
`publish = false`, shared workspace lints, no Python/Node scripting stack (the
embedded TypeScript adapter and test fixtures are declared product/test
assets, not tooling). The profile is resident + local state over stdio with
host adapters. Delivery is the existing sealed archive bundle
(`cargo xtask package`: binary, plugin manifests, hooks, skill,
`metadata.json`, `SHA256SUMS`, `COMPLETE`), built once by the tag workflow's
build job after the full gate, verified, accepted on the packaged executable
and described by `release-manifest.json`; the publish job stages a draft,
verifies it and publishes. Keep the bundle format, `install.sh`,
`scripts/macos-acceptance.sh` and `scripts/validate-release-evidence.sh`
unchanged unless a change is the point of the task.

## Agent batch workflow

Collect related targets before making tool calls. Use bounded batches: outline paths/glob,
symbol/graph symbols, read items, one multi-file edit (Codex patch / Claude files), test
targets, aggregate diff paths and known inspect detail_refs. Do not issue one call per
file, symbol or tiny hunk when the related set fits a batch. Split only for advertised
limits or newly discovered work. Start/stop are once-only lifecycle calls per worktree.

Batch related production and test edits after reading the affected units. In Bridge mode
graph/test acceptance is asynchronous even when cached. Keep the activation alive, end an
idle turn and continue from automatic completion; never poll or finish/stop while waiting.
This is workflow guidance, not extra permission or a claim of unsupported native wake.
Preserve the safety invariants, required full gate and staged cutover qualification.

## Where the contract snapshot lives

`schemas/tools.json` exports the real binary's `tools/list` array for Claude;
`schemas/tools-codex.json` exports Codex (`mcp --runtime-dir <dir> --host codex`).
The current production source defines eleven public tools per host, including `ide.graph` and
`ide.tree`; `ide.prepare` is retired from the catalogs but remains routable for internal tests.
Both snapshots are generated from the rebuilt binary and contain the eleven public names.
The complete project gate remains required before claiming this candidate is done.
Both source catalogs have the same eleven public names. Public `ide.context` is
removed, including `tools/call`; only its private engine/transport remains. Source
reads move to `ide.read {"items":[{"path":"src/a.rs"}]}` and skeletons to `ide.outline`.
`ide.symbol {"path":"src/a.rs","byte_offset":42}` resolves an exact UTF-8 use-site
(definitions/references or explicit lexical/unavailable evidence, no whole-file dump).
Offsets are semantic positions, never page cursors; split/out-of-file offsets refuse.
`ide.inspect {"kind":"diagnostics","path":"src/a.rs"}` reads file provider reports;
`ide.inspect {"kind":"problems"}` reads project check problems. Neither runs a check;
unknown diagnostics never mean zero, and unversioned counts remain provisional lower
bounds. Continue long results with `detail_ref`. `ide.symbol` also gives a card with immediate
callers/callees (all positive values mean direct neighbors). Use `ide.graph`
`{"symbols":["src/a.rs#f","src/b.rs#g"],"direction":"callers","depth":2}` for call chains when changing
signatures/public behavior, refactoring shared functions, tracing indirect faults or
investigating architecture/cross-language connections. Start depth 2, expand within 1–3
only as needed: callers shows impact, callees shows execution/dependencies, both shows
both sides; `tests:true` expands tests. Tiny edits need only the symbol card. Managed
Codex selects it with `mcp --launcher-template <file>`; the export flag does not combine with
the managed launcher form. After any change to the tool surface in
`crates/agent-ide-core/src/assistance/facade.rs`, run `cargo xtask contract
update`, review the diff, and commit it together with the source change; CI
and the gate fail on drift.

`ide.tree {"path":"…"}` is the on-demand project map (directories, files, sizes and the
authors' one-line descriptions; no source bodies). The retired `ide.prepare {"task":"…"}` was the
opt-in task reconnaissance call after `ide.start`. It returns one text reply carrying every selected unit whole, without
pagination or size cuts (`token_budget` is accepted only for compatibility and ignored); a
preparation outliving the call's 45 s wait answers `pending`, and one `ide.inspect` of its
`detail_ref` waits for and returns that packet. `partial` and `needs_evidence` are not successful
reconnaissance. `complete` describes delivered selected evidence, not a proof that
every edit location for an arbitrary task was discovered. Provider/key/root settings
remain in the trusted launcher; the caller supplies only the task.
