# Agent IDE

An explicitly activated coding companion for Codex and Claude Code, designed for Linux and macOS. One coding agent owns one Git worktree. A local broker coordinates isolated analysis views, compatible shared language-server backends and bounded feedback.

Status: the binary assembles the five-tool MCP surface, exact Codex hook-to-MCP binding,
explicit Codex/Claude native ingress, bounded hook context output, durable Workspace activation,
source context, safe Git comparisons and owned provider cleanup. Host-shaped process tests exercise
Claude foreground-helper activation, Go/Rust Context, Diff and emitted additional context.
Live macOS checks cover Claude Go/Rust and parallel native actors with sequential handoff,
plus the Codex Go context/edit/diff/stop loop. Codex native actors with distinct worktree roots,
complete delivery accounting and the remaining roadmap acceptance checks are still open.

Build with `cargo build --locked --bin agent-ide`. Configure an MCP client to launch
`target/debug/agent-ide mcp --runtime-dir PATH`. Discovery works without a daemon;
inactive calls return a bounded error directing the caller to native tools. Start the
daemon separately with `target/debug/agent-ide daemon --runtime-dir PATH`.

The separate launcher environment variable `AGENT_IDE_HOST_ATTACHMENT` enables
connect-only routing when supported host request metadata is also present. It is an
opaque transport handle, not authentication or workspace authority. The current daemon
matches exact Codex actor/call hook evidence. Set `AGENT_IDE_LAUNCHER_CONFIG` in the daemon
environment to load the restart-only [trusted execution configuration](docs/assistance-launcher.md).
Without that configuration, methods report the missing Workspace boundary.
See [product MCP boundary](docs/assistance-host-binding.md#product-mcp-boundary) for its limits.
Placeholder-only host examples are shipped for
[Codex](docs/examples/codex-hooks.toml) and [Claude Code](docs/examples/claude-settings.json).

`agent-ide doctor --runtime-dir PATH` is observational: it reports effective default configuration and local endpoint/lock/protocol state without creating the path or starting services. Workspace scanning, LSP startup, daemon autostart, and cache retirement without a peer-verified closure/reset fact are unsupported.

- [Current delivery roadmap](docs/roadmap.md)

Earlier design documents (subject to the current roadmap and interface renegotiation):

- [Product requirements](docs/product.md)
- [Architecture](docs/architecture.md)

The implementation uses Rust and Tokio.

Persistent LSP caches follow the worktree, including sequential coder-to-reviewer handoff. MCP or hook failure must leave ordinary agent work usable, with honest degraded results.

Stopping a coder releases its analysis leases and retains its worktree's cache directories. A verified Workspace closure or reset is the only fact that retires a cache namespace; v0.1 has no worktree-close surface that produces that fact yet, so namespaces accumulate up to a fixed ceiling for the daemon's lifetime rather than being retired automatically. Shared backends remain available to other open worktrees. Retained directories do not imply retained hot provider indexes; the strict retention requirement in the [roadmap](docs/roadmap.md#acceptance) remains unresolved.

Go worktrees split their cache in two. Each worktree keeps a private namespace holding its own `GOCACHE`/`GOMODCACHE`/`GOTMPDIR`, delivered per LSP view rather than as process environment, so a view without one fails closed instead of reading another worktree's build state. Compatible worktrees additionally share one backend-scoped native namespace holding gopls' own on-disk filecache and the listener's temporary directory: gopls binds that filecache once per process, so sharing it is what lets divergent worktrees run on a single physical listener with one forwarder each. Compatibility is one canonical effective-rights identity — provider, settings, toolchain, and the rights the observed sandbox state actually grants; cwd-relative sandbox roots are resolved to absolute rights first, and any policy whose rights cannot be proven equal is never shared. The shared namespace is reference-counted, so it survives a partial stop while any sharing worktree is still live, and both namespaces persist across stop and handoff. gopls manages the contents of its shared native namespace itself and may evict them at any time; that eviction is a cache miss, never a loss of IDE-owned worktree state.
