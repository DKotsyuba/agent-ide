# Agent IDE

An explicitly activated coding companion for Codex and Claude Code, designed for Linux and macOS. One coding agent owns one Git worktree. A local broker coordinates isolated analysis views, compatible shared language-server backends and bounded feedback.

Status: the binary exposes the five-tool MCP surface, finite daemon routing, and exact Codex hook-to-MCP binding. The v0.1 coding companion is not yet usable: durable Workspace activation and source/context/diff delivery are not assembled end to end.

Build with `cargo build --locked --bin agent-ide`. Configure an MCP client to launch
`target/debug/agent-ide mcp --runtime-dir PATH`. Discovery works without a daemon;
inactive calls return a bounded error directing the caller to native tools. Start the
daemon separately with `target/debug/agent-ide daemon --runtime-dir PATH`.

The separate launcher environment variable `AGENT_IDE_HOST_ATTACHMENT` enables
connect-only routing when supported host request metadata is also present. It is an
opaque transport handle, not authentication or workspace authority. The current daemon
matches exact Codex actor/call hook evidence, then reports `workspace_activation` as the next
missing boundary; no method claims source or semantic work before that peer is connected.
See [product MCP boundary](docs/assistance-host-binding.md#product-mcp-boundary) for its limits.

`agent-ide doctor --runtime-dir PATH` is observational: it reports effective default configuration and local endpoint/lock/protocol state without creating the path or starting services. Workspace scanning, LSP startup, daemon autostart, and cache retirement without a peer-verified closure/reset fact are unsupported.

- [Current delivery roadmap](docs/roadmap.md)

Earlier design documents (subject to the current roadmap and interface renegotiation):

- [Product requirements](docs/product.md)
- [Architecture](docs/architecture.md)

The proposed implementation uses Rust and Tokio.

Persistent LSP caches follow the worktree, including sequential coder-to-reviewer handoff. MCP or hook failure must leave ordinary agent work usable, with honest degraded results.

Closing a worktree retires its cache and analysis leases; finishing a coder session does not. Shared backends remain available to other open worktrees.
