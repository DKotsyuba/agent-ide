# Agent IDE

An explicitly activated coding companion for Codex and Claude Code, designed for Linux and macOS. One coding agent owns one Git worktree. A local broker coordinates isolated analysis views, compatible shared language-server backends and bounded feedback.

Status: technical specification and implementation planning. There is no implementation or installable release yet.

- [Product requirements](docs/product.md)
- [Architecture](docs/architecture.md)

The proposed implementation uses Rust and Tokio.

Persistent LSP caches follow the worktree, including sequential coder-to-reviewer handoff. MCP or hook failure must leave ordinary agent work usable, with honest degraded results.
