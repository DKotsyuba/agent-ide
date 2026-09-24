# macOS acceptance runner

`scripts/macos-acceptance.sh` is the reusable v0.2 acceptance entry point. It creates two detached
worktrees at the exact tested revision, commits different Python and TypeScript diagnostic fixtures
in each, runs the selected cell, and removes only those runner-owned worktrees at exit.

The default `product` route runs the locked real-provider product gates for Go/gopls, Rust,
Python/Pyright, Node/TypeScript r3, the edit/diagnostic/fix/diff/stop loop, stale-edit zero-write,
native fallback, compact MCP projection, telemetry restart/query/export, the Claude foreground
helper for Pyright and TypeScript, and divergent-worktree isolation. Every toolchain path and
version check returns explicitly inside the route guard, so a later successful check cannot mask
an earlier failure. Supply all of these absolute environment paths:

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

The `codex`, `claude`, `agent-run-claude`, and `agent-run-codex` routes accept an optional absolute `--driver`
executable. Without one, the runner creates bounded `not_tested` evidence and makes no host claim.
With one, `AGENT_IDE_ACCEPTANCE_HOST_VERSION` must be a public version token — for the committed
drivers that is a token identifying the actual host version, such as `agent-run-0.14.0+codex-cli-0.156.1`
for agent-run to Codex. The driver is invoked with no arguments and receives only these environment
variables:

- `AGENT_IDE_ACCEPTANCE_ROUTE`;
- `AGENT_IDE_ACCEPTANCE_LEFT_WORKTREE` and `AGENT_IDE_ACCEPTANCE_RIGHT_WORKTREE`;
- `AGENT_IDE_ACCEPTANCE_RESULT`.

Setting `AGENT_IDE_ACCEPTANCE_DRY=1` makes a committed driver print the exact session command
lines and exit 0 without running anything.

The direct `claude` driver runs `claude -p` sessions in each fixture worktree against the candidate
binary's managed MCP and plugin, capturing stream-json transcripts:

```sh
claude -p "$(cat <prompt>)" --model haiku --output-format stream-json --verbose \
  --dangerously-skip-permissions --strict-mcp-config --mcp-config <generated> \
  --plugin-dir <worktree>
```

Claude Code 2.1.274 documents no flag that bounds agentic turns (`--max-turns` is gone from
`claude --help`), so each session is bounded only by the driver's outer `perl alarm` wall clock.
The strict launcher template defaults to `/Users/pluto/.config/agent-ide/launcher.json`, which
carries the merged Codex profiles, Pyright, and the accepted TypeScript r3 provider
`claude-r3-2026-09-14`.

The shared agent-run driver starts real agents through the installed agent-run resident broker
and reads their final answers. Use `scripts/acceptance-drivers/agent-run-claude.sh` for
`agent-run-claude`, or `scripts/acceptance-drivers/agent-run-codex.sh` for `agent-run-codex`:

```sh
agent-run start --provider claude --model sonnet --profile implement \
  --task "$(cat <task>)" --workdir <worktree> --write --wait
agent-run answer <agent_id>
```

The route requires the matching schema-2 provider: `claude` with default model `sonnet`, or
`codex` with default model `gpt-6-luna`. Either route may override its model, but a provider
override must still match the route. The old `AGENT_IDE_ACCEPTANCE_AGENT_RUN_RUNTIME` setting
is rejected; use `AGENT_IDE_ACCEPTANCE_AGENT_RUN_PROVIDER`.
Codex uses `codex-prompts` and no Claude helper socket or project-local Claude
settings. The resident broker spawns the agent process, so environment set on the start command line (`AGENT_IDE_BIN`, `HOME`) does not
reach the agent: the agent's MCP comes from the operator's `~/.agent-run/config.toml`
`[mcp.agent_ide]` entry. The outer `perl alarm` bounds each start call independently of agent-run's
own timeout setting. Because the driver cannot redirect the agent
to the candidate, it fails closed with `installed_binary_differs` unless the installed
`/Users/pluto/.local/bin/agent-ide` is byte-identical to the candidate binary. The Codex route
also requires a matching accepted v3 visualization profile in the launcher before sessions start.
Set `TMPDIR` to an existing writable directory covered by the launcher's `allowed_roots` before
invoking the runner; for the current operator launcher, use `TMPDIR=/private/tmp/agent-ide-stability`.
The Codex driver checks both fixture worktrees against those roots before starting an agent.
For both agent-run routes, every captured Agent IDE tool reply must be a complete string result
of at most 16 KiB; divergent-worktree transcripts must contain no left fixture marker.

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
field. The agent-run Codex route requires a complete live cell before it can emit `real_pass`;
no Codex evidence is claimed by this implementation alone.

### Direct Codex driver

`scripts/acceptance-drivers/codex.sh` is the committed driver for `--route codex`. Status:
work in progress — first live run pending, and it has produced no `real_pass` claim. It runs real
bounded `codex exec --json -C <worktree> -s workspace-write --skip-git-repo-check -m <model>
-o <last-message> "<prompt>"` sessions (with `--dangerously-bypass-hook-trust` only when the
installed CLI documents that flag; approvals and sandbox are never bypassed) and writes the closed
nine-line document only when every scenario passes on captured transcripts plus real filesystem and
telemetry effects. Managed Codex binds `ide.start` directly from the MCP `_meta` attachment, so the
prompts have no foreground helper step; a `pending` tool answer must be followed by `ide.inspect`
with the returned `detail_ref`.

Each run builds a private `CODEX_HOME` in a per-run temporary directory with only the candidate
binary as the `agent-ide` MCP server (`mcp --launcher-template`), `default_tools_approval_mode =
"never"` for that server, the candidate's own `codex-hooks print` fragment as `hooks.json`, and the
operator's auth linked in place (the `file_link` pattern: a hard link to `~/.codex/auth.json`,
symlink fallback; never copied, read, or printed). Every scenario session is attempted at most three
times, and each retry first restores the exact fixture precondition.

Optional knobs: `AGENT_IDE_ACCEPTANCE_BINARY`, `AGENT_IDE_ACCEPTANCE_CODEX`,
`AGENT_IDE_ACCEPTANCE_LAUNCHER`, `AGENT_IDE_ACCEPTANCE_OPERATOR_HOME`,
`AGENT_IDE_ACCEPTANCE_MODEL` (default `gpt-5.6-luna`),
`AGENT_IDE_ACCEPTANCE_SESSION_SECONDS` (default 900), `AGENT_IDE_ACCEPTANCE_DIAG_LOG`, and
`AGENT_IDE_ACCEPTANCE_DRY=1`, which prints the exact commands and writes nothing.

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

## Host-cell results

The product candidate at revision `d88af079ac1e0c06d411208f513a1bfefeddf4b1` was exercised on
macOS 26.6.2 arm64. Its locked [product gate](evidence/macos-v0.2-product.json) passed with the
documented toolchain versions. Direct Claude was rerun at revision
`0806f50bcde8d68b5404576c5d9e69879bcd14f1`; the intervening revision changed only this acceptance
documentation and evidence, not the tested product. The real-host matrix remains closed: a
partially successful route is not a `real_pass`, and the runner marks every scenario `failed` when
its strict driver withholds the exact complete result document.

All four routes passed on one revision for release 0.3.15 (`edaf281`): Claude Code 2.1.280,
Codex CLI 0.155.1 and agent-run 0.12.6 to Claude Code 2.1.280.
Three environment facts were required and are now encoded in the drivers or release notes: the
Claude plugin hook entry carries no `args` (Claude Code 2.1.280 runs such entries without a shell),
Claude route sessions exclude user-level settings so an operator-registered hook cannot double the
candidate hook, and the agent-run service is restarted after an install so it loads the new plugin.

| Route | Public evidence | Result |
|---|---|---|
| Product contract | [JSON](evidence/macos-v0.2-product.json) | `product_pass` |
| Direct Codex CLI 0.155.1 | [JSON](evidence/macos-v0.2-direct-codex.json) | `real_pass` |
| Direct Claude Code 2.1.280 | [JSON](evidence/macos-v0.2-direct-claude.json) | `real_pass` |
| Installed agent-run 0.12.6 to Claude Code 2.1.280 | [JSON](evidence/macos-v0.2-agent-run-claude.json) | `real_pass` |
| Installed agent-run to Codex | Pending live evidence | `not_tested` |

The release evidence gate now requires `macos-v0.2-agent-run-codex.json` with route
`agent_run_codex` and a complete `real_pass` cell at the same candidate revision. Until that
live run passes, release publication is blocked.

The installed agent-run route is separate evidence and is not relabeled as direct Claude. Private
driver prompts, local paths, credentials, transcripts, and host/run identifiers were retained only
in disposable operator state and do not appear in these artifacts.

The earlier direct-Claude login result was false: the host inherited a per-run synthetic `HOME`, so
Claude resolved an empty isolated profile instead of the operator's normal authorized profile.
Changing outer sandbox permissions did not change that result; restoring the normal login home did,
with the existing OAuth read in place and no credential copied or emitted. Direct Claude then reached
the candidate MCP and foreground-helper route. This environment correction changes the row from
`not_tested` to a real `failed` result; it does not turn the incomplete cell into `real_pass`.

The remaining failures do not justify a product change in this task. The locked Claude helper gate
passes real Pyright Context and Diff, while the live direct-Claude Diff failure is
`workspace_authority`. The agent-run Diff failure is the distinct closed `capacity` result. The
accepted TypeScript r3 provider `claude-r3-2026-09-14` has since been compiled and merged into the
default launcher, so the refreshed drivers exercise it; whether it closes the TypeScript scenarios
on the current hosts is exactly what the pending first passing run must show.
