# Agent IDE

An explicitly activated coding companion for Codex and Claude Code on macOS arm64. One coding agent owns one Git worktree. A local broker coordinates isolated analysis views, compatible shared language-server backends and bounded feedback. Linux remains explicitly `not_tested` and no Linux release artifact is published.

Status: the binary assembles the six-tool MCP surface, exact Codex hook-to-MCP binding,
explicit Codex/Claude native ingress, bounded hook context output, durable Workspace activation,
source context, safe Git comparisons and owned provider cleanup. Host-shaped process tests exercise
Claude foreground-helper activation, stale-safe Edit, Go/Rust Context, Diff and emitted additional context.
Live macOS checks cover Claude Go/Rust and parallel native actors with sequential handoff,
plus Codex Go context/edit/diff/stop and parent/native-child isolation across distinct worktree
roots. Complete delivery accounting and the remaining roadmap acceptance checks are still open.

Tagged releases are built on GitHub Actions using an arm64 macOS runner and published
with a SHA-256 checksum in GitHub Releases. Install the latest private release with
`./install.sh`, or pin v0.2.0 with `./install.sh 0.2.0`. The installer uses the
authenticated GitHub CLI, verifies `SHA256SUMS`, and atomically installs to
`~/.local/bin` (override with `AGENT_IDE_INSTALL_DIR`). It does not edit MCP or hook
configuration. The release archive also contains the Codex and Claude plugin manifests,
marketplace catalogs, skill, and hooks. See the [release installation and update guide](docs/release.md).

Runtime-neutral orchestrators can register one command for both hosts:

```toml
[mcp.agent_ide]
transport = "stdio"
command = "/absolute/path/to/agent-ide"
args = ["mcp", "--auto-launcher-template", "/absolute/path/to/launcher.json"]
```

Auto mode selects Claude whenever `CLAUDE_PROJECT_DIR` is present, including invalid values that
must fail open as Claude rather than fall through to Codex. When the variable is absent, it reuses
the existing Codex managed path. The explicit host flags below remain supported.

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
still exposes the same six tools with disconnected native-fallback results. A repository plugin
manifest cannot safely supply the machine-specific template path, so it remains normal host config.
Legacy `agent-ide mcp --runtime-dir PATH` remains connect-only and compatible with separately
started `agent-ide daemon --runtime-dir PATH` instances.

Claude uses the same standard MCP configuration mechanism, with an explicit managed-Claude flag.
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
target to carry the strict Claude operator profile documented below. It exclusively owns a
deterministic private runtime for that project, so a second MCP stays disconnected until the owner
exits and removes it.

With Claude sandboxing enabled on macOS, `sandbox.network.allowUnixSockets` must contain the exact
`/private/tmp/ai-r-<hash>/claude-helper.sock` path. Since v0.3 the runtime directory is keyed by the
repository rather than one project digest: `<hash>` is the first 16 hex digits of the BLAKE3 digest
of the canonical git common directory (`git -C <project> rev-parse --path-format=absolute
--git-common-dir`, symlinks resolved), or of the canonical project directory outside a repository,
so one allowlist entry serves every worktree of one repository. The pending helper command shows the
exact runtime directory in its `--runtime-dir` argument whenever a helper reports `unavailable`.
Current Claude Code does not expand a wildcard for this socket allowlist; add that exact path to the
project's local settings before the next session, and do not enable `allowAllUnixSockets` for
Agent IDE. `agent-ide claude-rendezvous /absolute/path/to/project` prints the exact
`runtime_dir=` and `helper_socket=` paths for a repository without creating any runtime state.

The separate launcher environment variable `AGENT_IDE_HOST_ATTACHMENT` enables
connect-only routing when supported host request metadata is also present. It is an
opaque transport handle, not authentication or workspace authority. The current daemon
matches exact Codex actor/call hook evidence. Set `AGENT_IDE_LAUNCHER_CONFIG` in the daemon
environment to load the restart-only [trusted execution configuration](docs/assistance-launcher.md).
Without that configuration, methods report the missing Workspace boundary.
See [product MCP boundary](docs/assistance-host-binding.md#product-mcp-boundary) for its limits.
Placeholder-only host examples are shipped for
[Codex](docs/examples/codex-hooks.toml) and [Claude Code](docs/examples/claude-settings.json).

The v0.3 project problem feed adds confined background checks for Rust (`cargo check --offline
--locked`) and Python (pyright) on macOS, a compact `<agent-ide>` error/warning block on Claude
`PostToolUse` hooks, and `ide.context` with `{"kind":"problems"}`. Enable it only through the
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
[EYES-r2 contract](docs/contracts/eyes-v0.3.md) and the
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

Version 0.1.3 makes captured managed sandbox profiles portable across equivalent
worktrees without changing raw execution state or the original five v0.1 methods. It also
includes the offline installation helpers `agent-ide evidence executable`,
`agent-ide evidence record`, and `agent-ide launcher check`. The bundled
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

Pyright v0.1.1 supports Codex and Claude Python (`.py` and `.pyi`). Configure its closed `pyright_defaults_v1` provider with accepted absolute `pyright-langserver` and `node` executable objects, and set `toolchain` to the accepted Node identity. The launcher invokes that exact Node executable with the absolute Pyright script and `--stdio`; Claude does so only in its foreground helper, using the same accepted paths, identities, and BLAKE3 digests. The six-tool MCP surface keeps wire v2 for the original five methods and uses wire v3 for `ide.edit`.

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
TERM/KILL. Claude runs the same profile only in its claimed foreground helper.
