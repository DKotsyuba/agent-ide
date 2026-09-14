# macOS acceptance runner

`scripts/macos-acceptance.sh` is the reusable v0.2 acceptance entry point. It creates two detached
worktrees at the exact tested revision, commits different Python and TypeScript diagnostic fixtures
in each, runs the selected cell, and removes only those runner-owned worktrees at exit.

The default `product` route runs the locked real-provider product gates for Go/gopls, Rust,
Python/Pyright, Node/TypeScript r3, the edit/diagnostic/fix/diff/stop loop, stale-edit zero-write,
native fallback, compact MCP projection, telemetry restart/query/export, the Claude foreground
helper, and divergent-worktree isolation. Supply all of these absolute environment paths:

- `AGENT_IDE_GO` and `AGENT_IDE_GOPLS` for Go 1.25.x and gopls 0.23.0;
- `AGENT_IDE_RUST_ANALYZER`, `AGENT_IDE_RUST_TOOLCHAIN`, and
  `AGENT_IDE_RUST_TOOLCHAIN_DIR` for the accepted Rust 1.98.1 toolchain;
- `AGENT_IDE_NODE` and `AGENT_IDE_PYRIGHT` for Node 24.4.0 and Pyright 1.1.413;
- `AGENT_IDE_TYPESCRIPT_LANGUAGE_SERVER` and `AGENT_IDE_TSSERVER` for the real 6.0.0 bridge and
  TypeScript 5.9.3 `tsserver.js` files, not their shell-command symlinks.

Run it with a new evidence path outside the repository, for example:

```sh
scripts/macos-acceptance.sh --route product --evidence /absolute/output/evidence.json
```

## Real host drivers

The `codex`, `claude`, and `agent-run-claude` routes accept an optional absolute `--driver`
executable. Without one, the runner creates bounded `not_tested` evidence and makes no host claim.
With one, `AGENT_IDE_ACCEPTANCE_HOST_VERSION` must be a public version token. The driver is invoked
with no arguments and receives only these environment variables:

- `AGENT_IDE_ACCEPTANCE_ROUTE`;
- `AGENT_IDE_ACCEPTANCE_LEFT_WORKTREE` and `AGENT_IDE_ACCEPTANCE_RIGHT_WORKTREE`;
- `AGENT_IDE_ACCEPTANCE_RESULT`.

The driver must exercise the real host route and write exactly this ordered result to the result
path:

```text
schema=agent-ide.host-cell.v1
edit_diagnostic_loop=real_pass
stale_edit_zero_write=real_pass
native_fallback=real_pass
telemetry_restart_query_export=real_pass
compact_content=real_pass
python_provider=real_pass
typescript_r3=real_pass
divergent_worktrees=real_pass
```

This interface deliberately carries no session, run, transcript, prompt, credential, or command
field. A driver can invoke direct Codex, direct Claude, or an externally installed agent-run to
Claude without teaching this repository its private identifiers or changing agent-run source.

## Evidence boundary

The output must satisfy
[`macos-acceptance-evidence-v0.2.schema.json`](contracts/macos-acceptance-evidence-v0.2.schema.json)
and is capped at 16 KiB. It contains only the project revision, closed route/status values, public
platform and tool versions, eight closed scenario outcomes, and explicit false privacy fields. It
contains no filesystem path, source, prompt, credential, command, diagnostic message, transcript,
or private run identifier. `product_pass` records a shipping product/provider gate, while
`real_pass` is reserved for a supplied real-host driver.

Non-macOS execution emits `not_tested` evidence and does not create worktrees or claim support.
Linux therefore remains explicitly `not_tested` until a separate real Linux acceptance contract is
implemented.

## v0.2 candidate results

The candidate at revision `d88af079ac1e0c06d411208f513a1bfefeddf4b1` was exercised on macOS
26.6.2 arm64. Its locked [product gate](evidence/macos-v0.2-product.json) passed with the documented
toolchain versions. The real-host matrix remains closed: a partially successful route is not a
`real_pass`, and the runner marks every scenario `failed` when its strict driver withholds the exact
complete result document.

| Route | Public evidence | Result |
|---|---|---|
| Direct Codex CLI 0.154.0 | [JSON](evidence/macos-v0.2-direct-codex.json) | `failed`: real Pyright edit/stale/native behavior and TypeScript semantic Context ran, but a required final inspection failed; no complete host claim |
| Direct Claude Code 2.1.267 | [JSON](evidence/macos-v0.2-direct-claude.json) | `not_tested`: external authentication blocker was exactly `Not logged in · Please run /login` |
| Installed agent-run 0.11.8 to Claude Code 2.1.267 | [JSON](evidence/macos-v0.2-agent-run-claude.json) | `failed`: both divergent worktrees completed the real Pyright/helper path, including stale zero-write and native fallback; Diff inspection exhausted detail capacity and TypeScript remained lexical-only, so the complete cell did not pass |

The installed agent-run route is separate evidence and is not relabeled as direct Claude. Private
driver prompts, local paths, credentials, transcripts, and host/run identifiers were retained only
in disposable operator state and do not appear in these artifacts.
