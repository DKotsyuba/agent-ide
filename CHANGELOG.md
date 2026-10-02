# Changelog

## Unreleased

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
