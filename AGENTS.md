# agent-ide — agent instructions

A resident stdio MCP server for Codex and Claude Code on macOS arm64: one
language-server-backed tool surface over Rust, Python, TypeScript/JavaScript,
CSS and HTML, with Go built in but outside the release scope.
`docs/architecture.md` is the behaviour truth source; the machine-readable tool
contract is `docs/contracts/tools-v0.4.md` with the exported snapshot in
`schemas/tools.json`.

## Single workflow

`cargo xtask check` is the full non-mutating gate; run it before claiming
done. It runs fmt `--check`, the locked workspace tests with
`--test-threads=1` and the three named gopls skips, the four ignored
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
| `crates/agent-ide-core/src/assistance/` | the eleven-tool MCP facade, host binding, Codex/Claude adapters, replies and hooks |
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
- Every `ide.edit` carries an `operation_id`: a repeated id never applies the
  edit twice; edits are validated against the exact `source_ref` bytes and
  refused together with nothing written when addresses overlap or do not match.
- Replies are written for agents: bounded pages with `detail_ref` continuation,
  closed vocabularies, `isError` on every business refusal, `pending` polls via
  `ide.inspect`, honest `unavailable`/`outcome_unknown` results — never
  fabricated success. A failed IDE never vetoes native host tools.
- The resident daemon lives in a shared per-repository rendezvous
  `/private/tmp/ai-r-<hash>` keyed by the canonical Git common directory; hook
  keys live in `/private/tmp/ai-k-<hash>`. Ownership and staleness are checked
  before adoption and an MCP exit never stops a healthy daemon.
- Project checks run confined (`sandbox-exec`, no network, read-only worktree,
  allowlisted environment) and are fail-open.

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

## Where the contract snapshot lives

`schemas/tools.json` is the tools array of the real built binary's
`tools/list` result. After any change to the tool surface in
`crates/agent-ide-core/src/assistance/facade.rs`, run `cargo xtask contract
update`, review the diff, and commit it together with the source change; CI
and the gate fail on drift.
