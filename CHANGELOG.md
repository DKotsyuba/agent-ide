# Changelog

## Unreleased

### Added

- The agent-* family shape: `AGENTS.md`/`CLAUDE.md`, this changelog,
  `SECURITY.md`, `LICENSE` (MIT), `docs/MCP_RESPONSE_STANDARD.md` and
  `docs/FAMILY_CONTRACT.md`, `docs/qualification.md`, `family.toml` with
  `.family/` template provenance, workspace Cargo hygiene (resolver 3,
  `rust-version`, license, shared lints, hoisted dependencies), `deny.toml`
  with a CI supply-chain job and Dependabot, the `xtask` gate
  (`cargo xtask check`, `standard check`, `contract check|update`), the
  exported `schemas/tools.json` tool-contract snapshot with drift tests, and
  SHA-pinned actions with concurrency groups in both workflows. No shipped
  binary behavior changed.

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
