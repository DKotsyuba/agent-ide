# Delivery roadmap

This is the current delivery boundary. It supersedes the earlier all-at-once implementation order in the design documents. The repository does not yet provide a working or installable IDE. The older architecture and contract documents remain design input; they are not evidence that an interface is implemented or still agreed.

## v0.1: working coding companion

A coding actor explicitly activates assistance for its own Git worktree. The initial MCP surface is exactly `ide.start`, `ide.context`, `ide.diff`, `ide.inspect`, and `ide.stop`. No check, finish, edit, impact, or status MCP tool is exposed in this version. Administrative CLI commands are separate from MCP tools.

Activation is actor-specific. Native subagents do not inherit it. The host adapter must prove the relation between the actor, MCP invocation, and hook/delivery channel; model arguments, working directory, timing, parent IDs, and peer user identity alone are insufficient proof. A failed or unavailable IDE must leave native tools and turn completion usable. Hooks have independently enforced deadlines and never wait for compiler warmup.

The real provider loop covers shared Go/gopls, a bounded exclusive Rust profile, and a bounded exclusive Pyright profile for Codex and Claude Python (`.py` and `.pyi`) on macOS. Pyright is always one worktree-isolated stdio child with fixed configuration; Claude runs it only in a foreground helper under the helper's existing strict sandbox contract. Intelligence chooses the provider topology; Execution admits and supervises physical processes and forwarders. Unsupported sharing stays explicitly unsupported. A shared daemon must not gain authority beyond the host's verified execution profile. Where enforcement is unproven, affected execution is unavailable.

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
| macOS, Codex CLI, Python/Pyright 1.1.413 | `real_pass` for the bounded product/provider contract |
| macOS 26.6.2, Codex, Node 24.4.0 / TypeScript Language Server 6.0.0 / TypeScript 5.9.3 | `real_pass`: real JS, JSX, TS and TSX semantic Context plus release-pinned normal shutdown |
| macOS 26.6.2, Claude Code 2.1.267, Go/gopls 0.23.0 | `real_pass`: foreground helper, native-edit diagnostic, Diff, Stop and parallel actor isolation |
| macOS 26.6.2, Claude Code 2.1.267, Rust/rust-analyzer 1.98.1 | `real_pass`: foreground helper semantic context, native-edit diagnostic, Diff and Stop |
| macOS, Claude Code, Python/Pyright | `product_covered`; real host containment remains separately verified |
| Linux, both hosts/providers | `not_tested`; outside the current Mac-only release gate |

Checks cover isolation between divergent worktrees, stale source/provider results, inactive hooks, stop and handoff, daemon/provider crashes, cancellation, bounded queues/output, and preservation of native tool/turn behavior. Productivity evaluation includes correctness, full model-visible context, cold/warm/handoff time, and the complete process tree's resource use. Fewer tool calls alone do not establish improvement.

Caches follow the worktree across stop and sequential handoff. Actor termination does not delete a valid worktree cache; verified worktree closure/reset retires its owned namespace. Hot memory and durable cache are distinct. The scope of this retention guarantee for opaque provider-internal indexes still needs an explicit decision; no provider profile may silently weaken the current strict requirement.

## Later increments

These are roadmap boundaries, not authorization to scaffold their APIs now.

The immediate product priority is one measured integrated coding loop rather than
the previously ordered history-first increment. Work proceeds in this order:

1. Add bounded local usage telemetry to the existing five tools and native-change
   hooks. It records structured metadata for adoption, fallback reason, outcome,
   latency, provider/language, cache reuse, diagnostics, output size and resource
   cost. Raw source, prompts, credentials, arbitrary arguments and command bodies
   are excluded by default. Telemetry failure remains fail-open and cannot delay a
   coding operation.
2. Add a language-neutral `ide.edit` MVP as the preferred active-session source
   mutation path. The first contract is a single-file create-or-replace-if-current operation
   with an exact effects receipt, refreshed source observation, diagnostic feedback
   and the existing diff flow. Stale input performs no write. Unknown outcome names
   the affected path and requires inspection before retry. Native editing remains
   available when the IDE is inactive, unavailable, unsupported or explicitly
   bypassed; host guidance prefers `ide.edit` and telemetry measures every fallback
   without denying it.
3. Add JavaScript/TypeScript/Node.js provider profiles through the same bounded
   context/diagnostic/edit/diff/stop scenario. Node/TypeScript project resolution
   is part of the supported profile; an installed binary alone does not establish
   support. Python is delivered for Codex and Claude/macOS through their existing bounded
   provider paths.
4. Accept the increment only after real Codex and Claude macOS scenarios cover
   `start -> context -> edit -> diagnostic -> fix -> diff -> stop`, stale-edit
   refusal, native fallback, restart-safe telemetry and two-worktree isolation.
   Linux remains explicit `not_tested` until a real host cell passes.

### v0.2 acceptance evidence matrix (ACCEPTANCE-r1)

This matrix consumes the public [TELEMETRY-r1](contracts/telemetry-v0.2.md),
[EDIT-r1](contracts/changes-v0.2.md), and [TYPESCRIPT-r3](contracts/intelligence-v0.2.md)
contracts. It is preparation only: every cell below is `not_tested` until an implementation and
real run produce public evidence. Public artifacts contain versions, route, scenario outcomes,
bounded metrics, and explicit truncation only; private supervisor identifiers and transcripts do
not enter them.

| macOS route | Required real evidence | Status |
|---|---|---|
| Direct Codex | `start -> context -> edit -> diagnostic -> fix -> diff -> stop`; stale edit has zero writes; native fallback; restart-safe telemetry query/export; divergent worktrees | `failed` on macOS 26.6.2 with Codex CLI 0.154.0: Pyright edit/stale/native behavior and TypeScript semantic Context ran; one Diff inspection completed before a further driver inspection failed without preserving its closed code |
| Direct Claude | The same scenario through the claimed foreground helper, including TypeScript only after its spike; stale edit has zero writes; native fallback; restart-safe telemetry; divergent worktrees | `failed` on Claude Code 2.1.267: normal OAuth was available after removing the synthetic-home isolation; helper-backed Pyright ran in both worktrees, but Diff returned `workspace_authority` and TypeScript remained lexical pending its separate host record |
| Installed external agent-run-to-Claude | The same end-to-end route, with its external attachment treated as an adapter rather than identity authority; stale edit has zero writes; native fallback; restart-safe telemetry; divergent worktrees | `failed` on agent-run 0.11.8 to Claude Code 2.1.267: both divergent Pyright/helper paths ran, Diff inspection returned `capacity`, and the tested Claude launcher had no accepted TypeScript provider |
| Linux, all routes | No v0.2 host/provider acceptance claim | `not_tested` |

This increment does not add a remote analytics service, dashboard, arbitrary shell
execution, multi-file atomicity, semantic rename, test/check orchestration or DAP.
Local statistics need a bounded query/export surface; visualization can be added
after the collected events show that it is useful.

| Version | Added result |
|---|---|
| v0.2 | Local usage telemetry; preferred single-file `ide.edit` with native fallback; JavaScript/TypeScript/Node.js profiles |
| v0.3 | Explicit known checks, evidence and local finish; `ide.check` and `ide.finish` |
| v0.4 | ChangeSet, Git/source context and history; Scope WorkBundle and CodeBinding |
| v0.5 | Computed CriterionAssessment, Task Context Compiler and contract-aware impact |
| v0.6 | Evaluator registry, cost-aware verification planner and approved automatic checks |
| v0.7 | Scope adapter, immutable knowledge, publication outbox and baseline/result acceptance |
| v0.8 | Multi-file edit journal, semantic rename and recovery |
| v0.9 | Failure context and approved DAP debugging |
| v1.0 | Cumulative hardening and independent evaluation |

Scope remains the authority for agreed work. A local bundle can support standalone operation, but its shape is not approval, caller identity, or verification evidence. The daemon does not reinterpret the owner's agreed task. Future verified outcomes require current evidence; v0.1 cannot claim them.
