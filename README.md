# Agent IDE

An explicitly activated coding companion for Codex and Claude Code on macOS arm64. One coding agent owns one Git worktree. A local broker coordinates isolated analysis views, compatible shared language-server backends and bounded feedback. Linux remains explicitly `not_tested` and no Linux release artifact is published.

Status: the binary assembles the eleven-tool MCP surface, exact Codex hook-to-MCP binding,
explicit Codex/Claude native ingress, bounded hook context output, durable Workspace activation,
source context, safe Git comparisons and owned provider cleanup. Host-shaped process tests exercise
Claude daemon activation, stale-safe Edit, Go/Rust Context, Diff and emitted additional context.
Live macOS checks cover Claude Go/Rust and parallel native actors with sequential handoff,
plus Codex Go context/edit/diff/stop and parent/native-child isolation across distinct worktree
roots. Complete delivery accounting and the remaining roadmap acceptance checks are still open.

Tagged releases are built on GitHub Actions using an arm64 macOS runner and published
with a SHA-256 checksum in GitHub Releases. See [docs/release.md](docs/release.md).

## Install

Use the same command for a fresh installation or an update:

```bash
curl -fsSL https://github.com/DKotsyuba/agent-ide/releases/latest/download/install.sh | sh
```

Or use wget:

```bash
wget -qO- https://github.com/DKotsyuba/agent-ide/releases/latest/download/install.sh | sh -s -- --downloader wget
```

**Availability:** the installer ships starting with 0.4.3. Earlier releases installed only
the bare binary with the old `gh`-based `./install.sh` from the repository checkout.

The repository is private today: the installer accepts `GITHUB_TOKEN` (sent as a bearer
token) or falls back to `gh release download` when `gh` is authenticated.

The installer verifies the tarball against the release `SHA256SUMS`, checks the bundle's own
manifest (`metadata.json`, `SHA256SUMS`, `COMPLETE`), keeps immutable versions under
`~/.agent-ide/standalone/releases/<version>` with a `current` symlink, installs the host
plugin under `~/.local/share/agent-ide/plugin/<version>` with a `plugin/current` symlink
(what agent-run and crew reference), and places a managed launcher shim at
`~/.local/bin/agent-ide`. Add that directory to `PATH`. Repeating the command updates; the
same version is a no-op; an existing version is never overwritten with different bytes. No
Cargo, Node or sudo is needed for the binary; the language servers are separate (accepted
versions are listed under [Configure](#configure)). The installer does not edit host MCP or
hook configuration.

To pin a version or choose directories, download `install.sh` and run:

```bash
sh install.sh --version X.Y.Z --home "$HOME/.agent-ide" \
  --prefix "$HOME/.agent-ide/standalone" --bin-dir "$HOME/.local/bin" \
  --share-dir "$HOME/.local/share/agent-ide"
```

State lives in `~/.agent-ide` (`--home`); the host plugin lives under `--share-dir`
(default `~/.local/share/agent-ide`). `AGENT_IDE_HOME` is a user-home override for tests and relocation: it moves the whole per-user tree (`.agent-ide`, `.config/agent-ide`, `.local`) together.

Build from source (also usable before publication):

```bash
release_root="$(mktemp -d)"
version="$(awk -F '"' '/^version = / { print $2; exit }' Cargo.toml)"
cargo build --locked --release --bin agent-ide
scripts/package-release.sh target/release/agent-ide "v$version" "$release_root"
"$release_root/agent-ide-v$version/agent-ide" self-install --release "$release_root/agent-ide-v$version" --version "$version"
```

Use a new version for changed source: the installer never overwrites an existing version
with different bytes. For a development install without a release bundle, use
`scripts/install-local.sh`.

Restart agent-run after an install or update: it resolves
`~/.local/share/agent-ide/plugin/current` once, when its service starts, so its runtimes
keep the previous plugin until the restart. Register Codex hooks once with
`agent-ide codex-hooks print`. Host wiring for the Codex and Claude Code plugins is in
[docs/release.md](docs/release.md) (§Codex plugin, §Claude Code plugin).

## Configure

`agent-ide init` writes a minimal `~/.config/agent-ide/launcher.json` — the
[trusted launcher configuration](docs/assistance-launcher.md) naming allowed roots and
accepted executables — without overwriting an existing file. Validate it and inspect the
installation:

```bash
agent-ide init
agent-ide launcher check ~/.config/agent-ide/launcher.json
agent-ide doctor
```

`agent-ide doctor` with no arguments prints a JSON health report of the installation —
configuration, toolchains, install layout, plugin link, host hooks, daemons, recent errors —
and exits 2 when it reports errors. `agent-ide doctor --runtime-dir PATH` keeps querying a
running repository daemon.

The accepted language-server versions for 0.4 are rust-analyzer 1.98.1 (from the pinned
Rust toolchain), pyright 1.1.413, typescript-language-server 6.0.0, TypeScript 5.9.3, and
Node 24.4.0; the release workflow installs exactly these, and [docs/release.md](docs/release.md)
lists them for the publication gate.

Runtime-neutral orchestrators can register one command for both hosts:

```toml
[mcp.agent_ide]
transport = "stdio"
command = "/absolute/path/to/agent-ide"
args = ["mcp", "--auto-launcher-template", "/absolute/path/to/launcher.json"]
```

Auto mode selects Claude whenever `CLAUDE_PROJECT_DIR` is present, including invalid values that
must fail open as Claude rather than fall through. An absent variable needs positive non-Codex
evidence before it may leave the Codex contract: a `ZCODE_*` startup variable selects the
Claude-compatible contract (the ZCode desktop host's hooks are `claude-hook`), with the current
directory as the captured candidate — hooks resolve through the Claude rendezvous, a call carrying
no supported host metadata answers `unavailable: host_binding (host_unrecognized)`, and the first
`ide.start {root}` inside `allowed_roots` re-roots the session like a moved Claude session.
Everything else — Codex startup markers, and no evidence at all — keeps the previous Codex
default, so agent-run's Codex children and existing Codex hosts are unchanged; the explicit host
flags below remain supported.

Build locally with `cargo build --locked --bin agent-ide`. The standard Codex setup is one
machine-local entry in `~/.codex/config.toml` (replace both absolute paths):

```toml
[mcp_servers.agent-ide]
command = "/absolute/path/to/agent-ide"
args = ["mcp", "--launcher-template", "/absolute/path/to/launcher.json"]
```

The launcher file uses the existing schema and must contain exactly one target. Managed MCP captures
the host-selected current directory, replaces only that target's candidate and a fresh internal
attachment, validates the result, and owns its private daemon until stdio closes. Startup failure
still exposes the same eleven tools with disconnected native-fallback results. A repository plugin
manifest cannot safely supply the machine-specific template path, so it remains normal host config.
Legacy `agent-ide mcp --runtime-dir PATH` remains connect-only and compatible with separately
started `agent-ide daemon --runtime-dir PATH` instances.

Claude uses the same standard MCP configuration mechanism as Codex.
Keep both absolute, machine-specific paths in the host's normal MCP configuration rather than the
plugin manifest:

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

The Claude plugin contributes the four correlated lifecycle hooks from `hooks/hooks.json`; its
plugin-local command requires `AGENT_IDE_BIN` to be the same absolute executable configured for
MCP and delegates to that exact installed binary's `claude-hook`. It never searches `PATH`, so a
binary update cannot split MCP and hook versions. That argument-free mode finds the active project rendezvous
solely from Claude's absolute `CLAUDE_PROJECT_DIR`. The managed MCP requires the template's one
target. It exclusively owns a
deterministic private runtime for that project, so a second MCP stays disconnected until the owner
exits and removes it.

`agent-ide claude-rendezvous /absolute/path/to/project` prints the repository runtime directory without creating runtime state.

The separate launcher environment variable `AGENT_IDE_HOST_ATTACHMENT` enables
connect-only routing when supported host request metadata is also present. It is an
opaque transport handle, not authentication or workspace authority. The current daemon
matches exact Codex actor/call hook evidence. Set `AGENT_IDE_LAUNCHER_CONFIG` in the daemon
environment to load the restart-only [trusted execution configuration](docs/assistance-launcher.md).
Without that configuration, methods report the missing Workspace boundary.
See [product MCP boundary](docs/assistance-host-binding.md#product-mcp-boundary) for its limits.
Placeholder-only host examples are shipped for
[Codex](docs/examples/codex-hooks.json) (managed; `agent-ide codex-hooks print` emits it, the
[TOML](docs/examples/codex-hooks.toml) form is legacy) and
[Claude Code](docs/examples/claude-settings.json).

The v0.3 project problem feed adds confined background checks for Rust (`cargo check --offline
--locked`) and Python (pyright) on macOS, a compact `<agent-ide>` error/warning block on Claude
`PostToolUse` hooks (on managed Codex it rides native hooks and terminal replies at once,
T29B), and `ide.context` with `{"kind":"problems"}`. Enable it only through the
trusted launcher configuration (`AGENT_IDE_LAUNCHER_CONFIG`): declare normalized absolute
`allowed_roots` plus a `project_checks` section naming the Rust toolchain and the Node/Pyright
pair, as in the shipped fragment
[docs/examples/launcher-eyes.json](docs/examples/launcher-eyes.json). Missing fields, empty
roots, or an undeclared language keep the feed disabled and v0.2 behaviour unchanged. Each check
runs under `sandbox-exec` with no network, a read-only worktree, reads limited to the declared
toolchains, and one private cache under
`$HOME/.agent-ide/checks/<repository>/<worktree>/<language>`; the environment is rebuilt from an
allowlist (`CARGO_NET_OFFLINE=true`, private `CARGO_TARGET_DIR` and temp). Runs are debounced,
bounded by `check_timeout_s`, and their process groups are killed on cancel or timeout. See the
[EYES-r2 contract](docs/contracts/eyes-v0.3.md), the
[symbol-addressed tools v0.4 contract](docs/contracts/tools-v0.4.md), and the
[launcher configuration](docs/assistance-launcher.md).

Checks run in one shared per-repository daemon under `/private/tmp/ai-r-<hash>`, keyed by the
canonical git common directory, so every worktree of one repository shares one daemon; later MCP
servers adopt it and MCP exit never stops it. The MCP leaves the repository key for its hook in
`/private/tmp/ai-k-<hash>`, so hooks never run `git`. With zero open sessions and no running
check the daemon stops after `idle_timeout_s` (default 300 seconds) and removes only its runtime
directory; caches survive. Everything is fail-open: a missing service, toolchain, or feed never
blocks native tools or turn completion.

`agent-ide doctor --runtime-dir PATH` is observational: it reports effective default configuration and local endpoint/lock/protocol state without creating the path or starting services. Workspace scanning, LSP startup, daemon autostart, and cache retirement without a peer-verified closure/reset fact are unsupported.

`agent-ide -v`, `-V`, `--version`, and `version` print `agent-ide <version>` and exit 0 without starting or contacting a daemon.

The IDE's only path policy is the launcher `allowed_roots` list: `ide.start` (optionally with
an absolute `root`) is admitted when the working directory, its Git worktree and its Git
directory lie inside a configured root, and refused with `outside_allowed_roots` otherwise. No
host sandbox is replayed and no sandbox profile is ever accepted or captured. The offline
installation helpers are `agent-ide evidence executable` and `agent-ide launcher check`. The bundled
`skills/agent-ide` workflow directs coding agents through start, context, the
supported `ide.edit` path when offered, refreshed context, diff, and stop.
Native host editing remains available when `ide.edit` is inactive, unavailable,
unsupported, declined, or uncertain.

- [Current delivery roadmap](docs/roadmap.md)

Earlier design documents (subject to the current roadmap and interface renegotiation):

- [Product requirements](docs/product.md)
- [Architecture](docs/architecture.md)

The implementation uses Rust and Tokio.

Persistent LSP caches follow the worktree, including sequential coder-to-reviewer handoff. MCP or hook failure must leave ordinary agent work usable, with honest degraded results.

Stopping a coder releases its analysis leases and retains its worktree's cache directories. A verified Workspace closure or reset is the only fact that retires a cache namespace; v0.1 has no worktree-close surface that produces that fact yet, so namespaces accumulate up to a fixed ceiling for the daemon's lifetime rather than being retired automatically. Shared backends remain available to other open worktrees. Retained directories do not imply retained hot provider indexes; the strict retention requirement in the [roadmap](docs/roadmap.md#acceptance) remains unresolved.

Go worktrees split their cache in two. Each worktree keeps a private namespace holding its own `GOCACHE`/`GOMODCACHE`/`GOTMPDIR`, delivered per LSP view rather than as process environment, so a view without one fails closed instead of reading another worktree's build state. Compatible worktrees additionally share one backend-scoped native namespace holding gopls' own on-disk filecache and the listener's temporary directory: gopls binds that filecache once per process, so sharing it is what lets divergent worktrees run on a single physical listener with one forwarder each. Compatibility is one canonical effective-rights identity — provider, settings, toolchain, and the rights the observed sandbox state actually grants; cwd-relative sandbox roots are resolved to absolute rights first, and any policy whose rights cannot be proven equal is never shared. The shared namespace is reference-counted, so it survives a partial stop while any sharing worktree is still live, and both namespaces persist across stop and handoff. gopls manages the contents of its shared native namespace itself and may evict them at any time; that eviction is a cache miss, never a loss of IDE-owned worktree state.

Pyright v0.1.1 supports Codex and Claude Python (`.py` and `.pyi`). Configure its closed `pyright_defaults_v1` provider with accepted absolute `pyright-langserver` and `node` executable objects, and set `toolchain` to the accepted Node identity. The launcher invokes that exact Node executable with the absolute Pyright script and `--stdio`; The daemon uses the same accepted paths, identities, and BLAKE3 digests. The MCP surface keeps wire v2 for the original five methods and uses wire v3 for `ide.edit`.

The v0.2 TypeScript increment supports Codex and Claude semantic Context for `.js`, `.jsx`, `.ts`,
and `.tsx` on their separate accepted macOS release cells. Configure `typescript_defaults_v1` with
the exact Node 24.4.0, TypeScript Language Server 6.0.0 bridge, TypeScript 5.9.3 `tsserver.js`,
complete closure identities, the bundle-bound compiled Codex evidence ID, the separate Claude
evidence ID when that host is enabled, and an ancestor `tsconfig.json` or `jsconfig.json` as
documented in the [launcher contract](docs/assistance-launcher.md). That config must set
`compilerOptions.types` to `[]` and `moduleResolution` to `node10`; JS/JSX also require
`allowJs: true`. The top-level `files` array must contain the current document exactly once as a
bounded normalized relative path; `include`, `exclude`, `outDir`, `declarationDir`, and dependency
graphs are unsupported. Each operation is exclusive and one-shot; normal
success requires graceful shutdown, protocol EOF, zero bridge exit, and direct-child reap without
TERM/KILL. Claude runs the same profile through the daemon.
