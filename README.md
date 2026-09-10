# Agent IDE

An explicitly activated coding companion for Codex and Claude Code, designed for Linux and macOS. One coding agent owns one Git worktree. A local broker coordinates isolated analysis views, compatible shared language-server backends and bounded feedback.

Status: the binary assembles the five-tool MCP surface, exact Codex hook-to-MCP binding,
explicit Codex/Claude native ingress, bounded hook context output, durable Workspace activation,
source context, safe Git comparisons and owned provider cleanup. Host-shaped process tests exercise
this assembly; fresh live Codex/Claude acceptance and model-visible feedback remain unverified.

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

The proposed implementation uses Rust and Tokio.

Persistent LSP caches follow the worktree, including sequential coder-to-reviewer handoff. MCP or hook failure must leave ordinary agent work usable, with honest degraded results.

Finishing a coder session retains its worktree's cache and analysis leases for reuse; it does not retire them. A verified Workspace closure or reset is the only fact that retires a cache namespace; v0.1 has no worktree-close surface that produces that fact yet, so namespaces accumulate up to a fixed ceiling for the daemon's lifetime rather than being retired automatically. Shared backends remain available to other open worktrees.
