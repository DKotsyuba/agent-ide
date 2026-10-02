# Qualification record

Release qualification for `agent-ide` is recorded in `family.toml`
(`qualification`, `qualified_targets`, `qualified_hosts`, `[languages]`).
This file lists the evidence behind the current record and the deliberate
exceptions to the family standard. A passing unit test alone is never
qualification evidence.

## Current record

- Target: `aarch64-apple-darwin` (macOS 27.0, arm64). Linux is explicitly
  `not_tested` and has no release artifact.
- Hosts: Claude Code 2.1.280, Codex CLI 0.156.1 (each also reached through
  agent-run 0.19.4).
- Source: `main` at `e96830b` (0.6.9); evidence under `docs/evidence/`.

## Evidence

| Area | What was run | Result |
|---|---|---|
| Native gate | fmt, locked workspace tests (`--test-threads=1`, three named gopls skips), four ignored real-provider tests, clippy/rustdoc `-D warnings`, release build — on `macos-26` arm64 in CI and locally via `cargo xtask check` | green on `e96830b` |
| Product acceptance | `scripts/macos-acceptance.sh --route product`: Rust/rust-analyzer, Python/Pyright and TypeScript/JS r3 provider gates, the edit/diagnostic/fix/diff/stop loop, stale-edit zero-write, native fallback, compact MCP projection, telemetry restart/query/export, Claude Pyright gate, divergent-worktree isolation | `product_pass`, all scenarios `product_pass` (`macos-v0.2-product.json`) |
| Direct Codex | committed driver against the real Codex CLI 0.156.1 | `real_pass` (`macos-v0.2-direct-codex.json`) |
| Direct Claude | committed driver against real Claude Code 2.1.280 | `real_pass` (`macos-v0.2-direct-claude.json`) |
| agent-run to Claude | installed plugin through agent-run 0.19.4 + Claude Code 2.1.280 | `real_pass` (`macos-v0.2-agent-run-claude.json`) |
| agent-run to Codex | installed plugin through agent-run 0.19.4 + Codex CLI 0.156.1 | `real_pass` (`macos-v0.2-agent-run-codex.json`) |
| Toolchains | rust-analyzer 1.98.1 (pinned toolchain), Node 24.4.0, pyright 1.1.413, typescript-language-server 6.0.0, TypeScript 5.9.3 | pinned in the workflows and in every evidence row |

## Known limits and deliberate exceptions to the family standard

- Go/gopls are built in but **experimental**: no Go toolchain is installed in
  CI or release, the three real-gopls tests are skipped by name, and evidence
  rows record `go`/`gopls` as `not_tested` (TEST-02, MCP-05-adjacent).
- `unavailable` and `outcome_unknown` results carry `isError=false`: the tool
  answered honestly within its contract; this differs from a business refusal
  and is pinned by product tests.
- Codex receives full `structuredContent` in addition to text; Claude gets the
  compact text projection.
- Tests share one cargo-configured `AGENT_IDE_HOME=target/test-home` and run
  with `--test-threads=1` (CFG-03 exception): the resident daemon's shared
  resources are serialized globally instead of per-test-home. Documented
  exception with the per-test-home redesign deliberately deferred.
- The bundle seal inside the release archive is named `COMPLETE` (PKG-04
  deviation): agent-ide uses it as a packaging seal, not as the installer's
  staged-transaction marker; the installer verifies it as part of the bundle
  manifest.
- Rendezvous directories live under `/private/tmp/ai-r-<hash>` instead of
  `<home>/run/` (CFG-01 deviation): they are cross-client, keyed by the Git
  common directory, ownership- and mode-checked (see SECURITY.md).
- Tool names are dotted (`ide.start` … MCP-05): a rename waits for per-host
  normalization measurement.
- `rmcp` is 3.2.0 against the family baseline candidate 3.4.0; the SDK bump is
  its own compatibility wave.
- `release-manifest.json` follows the template's
  `schemas/release-manifest.schema.json` exactly (closed objects, no extra
  fields), so the archive profile's facts the schema has no field for are
  encoded in the closest schema-valid way (PKG-01 encoding exception):
  the four mirrored versions (Cargo, two plugin manifests, the Claude
  marketplace) are one `version`, because the build job's 4-way gate and
  `cargo xtask package` refuse any divergence; the `archive-bundle-v1`
  delivery profile is the single `bundle` artifact with `target`
  `aarch64-apple-darwin` (the sealed tarball, verified by `package verify`);
  and the evidence → payload binding is the `evidence` artifact
  `acceptance.json`, whose own `payload.sha256` must equal the bundle digest.
  `workflow.id` is read from the Actions API in the build job (it has no
  environment variable).
- Release evidence: the five checked-in host rows may name an ancestor
  candidate revision (validated as before); the product route is re-run by
  the release build job on the packaged executable and its evidence names
  the exact archive SHA-256 (RLS-06 for the product route; host routes stay
  manual).
- Delivery is the existing sealed **archive bundle** (binary, plugin tree,
  manifests, hooks, skill, `metadata.json`, `SHA256SUMS`, `COMPLETE`), not
  `single-binary-v1`; `family.toml` records `delivery_profile =
  "archive-bundle-v1"` to name it honestly.
