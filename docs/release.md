# Release installation and update

Agent IDE 0.2.0 publishes one `aarch64-apple-darwin` archive. macOS arm64 is the only claimed
platform; Linux remains explicitly `not_tested` and has no release artifact.

## Verified binary installation

Run `./install.sh 0.2.0` from the tag-pinned repository checkout. The installer downloads
`agent-ide-v0.2.0-aarch64-apple-darwin.tar.gz` and `SHA256SUMS` through the authenticated GitHub
CLI, verifies the checksum, and atomically replaces `${AGENT_IDE_INSTALL_DIR:-$HOME/.local/bin}/agent-ide`.
It does not change host configuration. The archive contains that executable plus both plugin
manifests and marketplace catalogs, the Agent IDE skill, Claude hooks, this guide, and the
installer. `scripts/release-smoke.sh` checks that exact file set and executes the extracted
executable before publication.

## Codex plugin

Install the binary first, then download and verify the tag-pinned release archive. Extract it and
add its `.agents/plugins/marketplace.json` as a local Codex marketplace, then install `agent-ide`.
The plugin supplies the coding workflow skill; MCP remains machine-local because its launcher
template contains accepted absolute toolchain paths.

Configure the installed executable and launcher template in `~/.codex/config.toml`:

```toml
[mcp_servers.agent-ide]
command = "/absolute/path/to/agent-ide"
args = ["mcp", "--launcher-template", "/absolute/path/to/launcher.json"]
```

To update, run the installer with the new explicit version, download and verify the matching tagged
archive, replace the local marketplace source with its extracted bundle, and restart Codex. Verify
that the configured `command` still names the installer destination.

## Claude Code plugin

Add the tag-pinned marketplace and install its plugin:

```sh
claude plugin marketplace add DKotsyuba/agent-ide@v0.2.0
claude plugin install agent-ide@agent-ide
```

Export the installed absolute binary path before starting Claude Code, and use that identical path
as the MCP `command`:

```sh
export AGENT_IDE_BIN="$HOME/.local/bin/agent-ide"
```

```json
{
  "mcpServers": {
    "agent-ide": {
      "type": "stdio",
      "command": "/absolute/path/to/agent-ide",
      "args": ["mcp", "--claude-launcher-template", "/absolute/path/to/launcher.json"]
    }
  }
}
```

The plugin hook accepts only an absolute executable `AGENT_IDE_BIN` and never searches `PATH`.
Consequently MCP and hooks execute the same atomically replaced file when the configured command
and environment value match. After installing a newer explicit binary version, move the
marketplace source to the matching tag, run `claude plugin marketplace update agent-ide` and
`claude plugin update agent-ide@agent-ide`, then run `/reload-plugins` or start a new session.
Recheck `claude mcp get agent-ide` and `AGENT_IDE_BIN` before using the updated plugin.

## Local install from source

`scripts/install-local.sh [--prefix DIR] [--dry-run] [--no-build]` builds this checkout and
installs it into a user prefix (default `$HOME/.local`) so agent-run runtimes, crew hooks, and
Claude skills catalogs can depend on a stable installed path instead of the checkout itself. The
checkout remains the development working copy; nothing about it changes.

What is installed where:

- `<prefix>/bin/agent-ide` — the release binary, built with `cargo build --locked --release`
  (skip with `--no-build` when `target/release/agent-ide` is already current) and copied in
  atomically: staged as `agent-ide.tmp-<pid>`, then `mv`'d over the existing name so running
  processes keep their already-open old inode.
- `<prefix>/share/agent-ide/plugin/<version>/` — a plugin bundle containing `.claude-plugin/`,
  `.codex-plugin/`, `hooks/`, and `skills/` copied from the checkout. Its `hooks/claude-hook.sh`
  is regenerated to `exec` the installed binary's absolute path directly, so the installed copy
  no longer needs `AGENT_IDE_BIN`; the checkout's own strict `hooks/claude-hook.sh` is untouched.
  Re-running the installer for the same version replaces that version directory (staged, then
  swapped in).
- `<prefix>/share/agent-ide/plugin/current` — a symlink to the just-installed version directory,
  swapped atomically (staged as `current.tmp-<pid>`, then `mv -f`).

The script validates the installed bundle (`hooks/hooks.json` and `skills/agent-ide/SKILL.md`
present, the generated hook executable) and runs `agent-ide launcher check` against
`$HOME/.config/agent-ide/launcher.json` when that file exists; a launcher check failure is
reported as a warning, not a stop. It never edits `~/.claude/settings.json`,
`~/.agent-run/config.toml`, `crew.toml`, or any skills catalog — it only prints the exact lines an
operator should apply there, pointed at `<prefix>/share/agent-ide/plugin/current`. `--dry-run`
prints every action it would take without writing anything.

To roll back: restore the binary from its `<prefix>/bin/agent-ide.bak-<old-version-or-timestamp>`
backup (written before the new binary replaces the old one, named from the old binary's own
`--version` output when it prints one, else a UTC timestamp), and point `current` back at the
previous `<prefix>/share/agent-ide/plugin/<old-version>/` directory.

`agent-ide -v`, `-V`, `--version`, and `version` all print `agent-ide <version>` and exit 0
without touching the daemon, runtime dir, config, or network.

`agent-ide errors [--repo <path>] [--since <minutes>] [--limit <n>] [--summary]` reads the local
append-only error log at `~/.agent-ide/logs/<repository-key>/events.jsonl[.1]` (`--repo` defaults
to the current directory; the repository key is the same one an `ai-r-<id>` runtime directory
uses). It works with no daemon running, prints one compact `time method outcome reason worktree
detail` line per event (bounded to `--limit`, default 200, most recent last), and `--summary`
prints grouped `(method, outcome, reason)` counts instead. See
[`docs/contracts/error-log-v0.3.md`](contracts/error-log-v0.3.md).

## Publication gate

The release workflow repeats formatting, locked workspace tests, Clippy, rustdoc, the complete
product acceptance route, and the release build on macOS arm64. Publication additionally requires
the checked-in product, direct Codex, direct Claude, and installed agent-run-to-Claude evidence to
name one ancestor candidate revision. Every host scenario must be `real_pass`, the product
scenarios must be `product_pass`, and all rows must carry the accepted toolchain versions and
closed privacy fields. Missing drivers, `failed`, `not_tested`, mixed revisions, partial scenarios,
and Linux evidence all block publication.
