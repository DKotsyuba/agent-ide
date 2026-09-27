# Release installation and update

Agent IDE 0.2.0 publishes one `aarch64-apple-darwin` archive. macOS arm64 is the only claimed
platform; Linux remains explicitly `not_tested` and has no release artifact.

## Verified binary installation

Bootstrap with `curl -fsSL <install.sh> | sh` semantics — or download `install.sh` from the
release and run it directly:

```sh
sh install.sh --version 0.4.0
```

The script supports macOS arm64 only, refuses anything but https (curl `-q --proto =https`,
wget `--no-config`), downloads the release archive and its release-level `SHA256SUMS`, verifies
the tarball hash, lists the archive and rejects absolute paths, `..`, links, and special files,
extracts with `--no-same-owner --no-same-permissions`, and hands the extracted sealed bundle to
its own `agent-ide self-install`. `--version` (leading `v` allowed) picks a release; without it
the latest release is resolved through the GitHub API. `--home`, `--prefix`, and `--bin-dir`
pass through to self-install. The repository is private today: export `GITHUB_TOKEN` (sent as
`Authorization: Bearer`) or keep an authenticated `gh` on PATH, which is the download fallback.
It does not change host configuration. The archive contains the executable plus both plugin
manifests and marketplace catalogs, the Agent IDE skill, Claude hooks, this guide, the
installer, and the three seal files. `scripts/release-smoke.sh` checks that exact file set,
installs the bundle into disposable dirs, and launches the written executable before
publication.

What `self-install` writes (defaults, all overrideable):

- The standalone prefix holds the immutable releases:
  `<prefix>/releases/<version>/` (default prefix `<home>/standalone`, home default
  `$AGENT_IDE_HOME` or `~/.agent-ide`), each a verified copy of the bundle sealed by
  its own `SHA256SUMS` and `COMPLETE`.
- `<prefix>/current` — a symlink to `releases/<version>`, swapped atomically.
- `<share-dir>/plugin/<version>/` — the plugin parts (default share dir
  `~/.local/share/agent-ide`), with `hooks/claude-hook.sh` regenerated to `exec` the launcher;
  `<share-dir>/plugin/current` selects it, so agent-run and crew keep working unchanged.
- `~/.local/bin/agent-ide` — a managed launcher shim that defaults `AGENT_IDE_HOME` and
  `exec`s `<prefix>/current/agent-ide "$@"`. An existing file there is replaced only when it is
  a managed shim, a symlink into `<prefix>/current`, or a previously installed Mach-O binary,
  which moves aside once as `agent-ide.bak-<old version>`; anything else is refused as an
  `unowned launcher`.

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

`scripts/install-local.sh [--prefix DIR] [--no-build]` builds this checkout, packages it into
the sealed release bundle with `scripts/package-release.sh`, and installs that bundle through
its own `agent-ide self-install` — the single code path that writes the releases, both
`current` symlinks, and the managed launcher. The install lands in the same layout as the
bootstrap (`<prefix>/releases/<version>/`, `<prefix>/current`,
`<prefix>/share/agent-ide/plugin/<version>/` plus its `current`, and the managed shim at
`<prefix>/bin/agent-ide`), so agent-run runtimes, crew hooks, and Claude skills catalogs can
depend on stable installed paths instead of the checkout itself. The checkout remains the
development working copy; nothing about it changes. A previously installed plain binary at
`<prefix>/bin/agent-ide` moves aside once as `agent-ide.bak-<old-version>`; a foreign file
there refuses the install as an `unowned launcher`. The script never edits
`~/.claude/settings.json`, `~/.agent-run/config.toml`, `crew.toml`, or any skills catalog — it
only prints the exact lines an operator should apply there, pointed at
`<prefix>/share/agent-ide/plugin/current`.

agent-run resolves `<prefix>/share/agent-ide/plugin/current` to its versioned directory once, when
its service starts. After an install, restart the agent-run service when no agents are running;
until then its Claude runtimes keep loading the previous plugin version (seen with 0.3.12 → 0.3.13,
whose old hooks no longer ran under Claude Code 2.1.280).

To roll back: point `<prefix>/current` at the previous `<prefix>/releases/<old-version>/`
directory (releases are kept per version), and repoint
`<prefix>/share/agent-ide/plugin/current` the same way; a replaced binary also remains as
`<prefix>/bin/agent-ide.bak-<old-version>`.

`agent-ide -v`, `-V`, `--version`, and `version` all print `agent-ide <version>` and exit 0
without touching the daemon, runtime dir, config, or network.

`agent-ide errors [--repo <path>] [--since <minutes>] [--limit <n>] [--summary] [--all]` reads the
local append-only log at `~/.agent-ide/logs/<repository-key>/events.jsonl[.1]` (`--repo` defaults
to the current directory; the repository key is the same one an `ai-r-<id>` runtime directory
uses). It works with no daemon running. Every tool call and daemon/client lifecycle fact is
logged with a closed `level` (`error`/`warn`/`info`); by default only `warn` and `error` are shown,
`--all` also includes `info` (successes, `pending`, lifecycle). Prints one compact `time level
method outcome reason worktree detail` line per event (bounded to `--limit`, default 200, most
recent last), or with `--summary` grouped `(level, method, outcome, reason)` counts instead. See
[`docs/contracts/error-log-v0.3.md`](contracts/error-log-v0.3.md).

## Publication gate

The release workflow repeats formatting, locked workspace tests, Clippy, rustdoc, the complete
product acceptance route, and the release build on macOS arm64. The accepted release languages are
Rust, Python, and TypeScript/JavaScript; Go and gopls are outside the release scope. No Go
toolchain is installed, the workspace gate skips exactly the three real-gopls toolchain contracts
(`real_gopls_production_context_tracks_exact_observed_bytes`,
`shared_gopls_isolates_divergent_worktrees_and_detaches_one_view`, and
`dropping_live_gopls_owner_closes_its_owned_listener`) by name while running every other
workspace test, and the runner records `go` and `gopls` evidence as `not_tested`. Publication
additionally requires
the checked-in product, direct Codex, direct Claude, and installed agent-run-to-Claude evidence to
name one ancestor candidate revision. Every host scenario must be `real_pass`, the product
scenarios must be `product_pass`, and all rows must carry the accepted language toolchain versions
with `go` and `gopls` pinned to `not_tested` and closed privacy fields. Missing drivers, `failed`,
`not_tested` outside the go/gopls rows, mixed revisions, partial scenarios,
and Linux evidence all block publication.
