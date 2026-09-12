# Delivery roadmap

This is the current delivery boundary. It supersedes the earlier all-at-once implementation order in the design documents. The repository does not yet provide a working or installable IDE. The older architecture and contract documents remain design input; they are not evidence that an interface is implemented or still agreed.

## v0.1: working coding companion

A coding actor explicitly activates assistance for its own Git worktree. The initial MCP surface is exactly `ide.start`, `ide.context`, `ide.diff`, `ide.inspect`, and `ide.stop`. No check, finish, edit, impact, or status MCP tool is exposed in this version. Administrative CLI commands are separate from MCP tools.

Activation is actor-specific. Native subagents do not inherit it. The host adapter must prove the relation between the actor, MCP invocation, and hook/delivery channel; model arguments, working directory, timing, parent IDs, and peer user identity alone are insufficient proof. A failed or unavailable IDE must leave native tools and turn completion usable. Hooks have independently enforced deadlines and never wait for compiler warmup.

The real provider loop covers shared Go/gopls and a bounded exclusive Rust profile. Intelligence chooses the provider topology; Execution admits and supervises physical processes and forwarders. Unsupported sharing stays explicitly unsupported. A shared daemon must not gain authority beyond the host's verified execution profile. Where enforcement is unproven, affected execution is unavailable.

### Responsibilities

| Module | Owns in the working core |
|---|---|
| Workspace | Git/worktree identity, authority, observations, baseline, ownership and lifecycle; the Git adapter lives here |
| Execution | Admission, fair queues, spawning, output draining, cancellation evidence and resource accounting |
| Intelligence | LSP profiles, broker and views, semantic lookup, diagnostics, semantic cache validity |
| Changes | Diff composition and semantic enrichment using Workspace and Intelligence |
| Assistance | Host adapters, five MCP schemas, context presentation, feedback and deduplication |
| Application | Binary, daemon, private IPC, config, SQLite mechanics, migrations, directories and packaging |

Domain policy stays in its owning module. Shared assembly, manifests and migration allocation have one owner. Runtime call dependencies do not by themselves require serial implementation; agreed small contracts and simple substitutes can enable independent work. Substitutes do not prove real transport, durability, isolation or host behavior.

### Implementation order

1. Verify the actual checkout, toolchain, language tooling and dependency APIs.
2. Build the smallest real host roundtrip and prove actor binding and execution-profile enforcement before expanding abstractions.
3. Add the bounded daemon, IPC, configuration, storage mechanics and ownership path needed by that roundtrip. Restart-only configuration is acceptable initially.
4. Connect safe Git/source reads, observations and the real provider loop. Git reads preserve raw paths and use literal pathspecs; external diff, text conversion, filters and network effects are not allowed.
5. Establish shared gopls, bounded Rust operation and resource isolation, then run fault and productivity checks.

Only types, tables and settings actually consumed by this path are implemented. `async-lsp` is a candidate to verify with a compile test and real provider exchange, not a reason to build a replacement JSON-RPC framework in advance.

### Acceptance

The target platforms are Linux and macOS. Current v0.1 acceptance runs real Codex CLI and Claude CLI scenarios on the available Mac, including native coding subagents. Linux validation is deferred and must remain explicitly unverified; it does not block this delivery. A single passing host cell is still only an intermediate milestone. Record exact versions and explicit `not_tested`, `mocked`, or `real_pass` evidence; a passing mock never grants support status.

| Host/provider cell | Current evidence |
|---|---|
| macOS 26.6.2, Codex CLI 0.154.0, Go/gopls 0.23.0 | `real_pass`: parent plus parallel native children in divergent worktrees, scoped diagnostic, peer survival and fresh sequential handoff |
| macOS 26.6.2, Codex CLI 0.154.0, Rust/rust-analyzer 1.98.1 | `real_pass` for the managed product/provider contract; live model CLI cell `not_tested` |
| macOS 26.6.2, Claude Code 2.1.267, Go/gopls 0.23.0 | `real_pass`: foreground helper, native-edit diagnostic, Diff, Stop and parallel actor isolation |
| macOS 26.6.2, Claude Code 2.1.267, Rust/rust-analyzer 1.98.1 | `real_pass`: foreground helper semantic context, native-edit diagnostic, Diff and Stop |
| Linux, both hosts/providers | `not_tested`; outside the current Mac-only release gate |

Checks cover isolation between divergent worktrees, stale source/provider results, inactive hooks, stop and handoff, daemon/provider crashes, cancellation, bounded queues/output, and preservation of native tool/turn behavior. Productivity evaluation includes correctness, full model-visible context, cold/warm/handoff time, and the complete process tree's resource use. Fewer tool calls alone do not establish improvement.

Caches follow the worktree across stop and sequential handoff. Actor termination does not delete a valid worktree cache; verified worktree closure/reset retires its owned namespace. Hot memory and durable cache are distinct. The scope of this retention guarantee for opaque provider-internal indexes still needs an explicit decision; no provider profile may silently weaken the current strict requirement.

## Later increments

These are roadmap boundaries, not authorization to scaffold their APIs now.

| Version | Added result |
|---|---|
| v0.2 | ChangeSet, Git/source context and history |
| v0.3 | Explicit known checks, evidence and local finish; `ide.check` and `ide.finish` |
| v0.4 | Scope WorkBundle, CodeBinding and computed CriterionAssessment |
| v0.5 | Task Context Compiler, contract-aware impact and evaluator registry |
| v0.6 | Cost-aware verification planner and approved automatic checks |
| v0.7 | Scope adapter, immutable knowledge, publication outbox and baseline/result acceptance |
| v0.8 | Own edits, journal, rename and recovery; `ide.edit` |
| v0.9 | Failure context and approved DAP debugging |
| v1.0 | Cumulative hardening and independent evaluation |

Scope remains the authority for agreed work. A local bundle can support standalone operation, but its shape is not approval, caller identity, or verification evidence. The daemon does not reinterpret the owner's agreed task. Future verified outcomes require current evidence; v0.1 cannot claim them.
