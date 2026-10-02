# Delivery roadmap

This is the current delivery boundary. It supersedes the earlier all-at-once implementation order in the design documents. The repository ships an installable IDE: release 0.6.9 installs through the [one-line installer](../README.md#install) on macOS arm64 (eleven MCP tools, host plugins, managed launcher), and all five macOS acceptance routes pass at the release revision recorded in [docs/evidence](evidence/). Linux remains explicitly `not_tested`. The older architecture and contract documents remain design input; they are not evidence that an interface is implemented or still agreed.

## v0.1: working coding companion

A coding actor explicitly activates assistance for its own Git worktree. The initial MCP surface is exactly `ide.start`, `ide.context`, `ide.diff`, `ide.inspect`, and `ide.stop`. No check, finish, edit, impact, or status MCP tool is exposed in this version. Administrative CLI commands are separate from MCP tools.

Activation is actor-specific. Native subagents do not inherit it. The host adapter must prove the relation between the actor, MCP invocation, and hook/delivery channel; model arguments, working directory, timing, parent IDs, and peer user identity alone are insufficient proof. A failed or unavailable IDE must leave native tools and turn completion usable. Hooks have independently enforced deadlines and never wait for compiler warmup.

The real provider loop covers shared Go/gopls, a bounded exclusive Rust profile, and a bounded exclusive Pyright profile for Codex and Claude Python (`.py` and `.pyi`) on macOS. Pyright is always one worktree-isolated stdio child with fixed configuration; Claude runs it through the shared daemon route. Intelligence chooses the provider topology; Execution admits and supervises physical processes and forwarders. Unsupported sharing stays explicitly unsupported. A shared daemon must not gain authority beyond the host's verified execution profile. Where enforcement is unproven, affected execution is unavailable.

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

The evidence table below is the historical v0.1 record (recorded 2026-09-12 against Codex CLI 0.154.0 and Claude Code 2.1.267). Go and gopls left the release scope in 0.6: the publication gate pins their evidence rows to `not_tested` ([release gate](release.md#publication-gate)), so those two `real_pass` cells record past capability, not a current claim.

| Host/provider cell | v0.1 evidence (historical) |
|---|---|
| macOS 26.6.2, Codex CLI 0.154.0, Go/gopls 0.23.0 | `real_pass`: parent plus parallel native children in divergent worktrees, scoped diagnostic, peer survival and fresh sequential handoff |
| macOS 26.6.2, Codex CLI 0.154.0, Rust/rust-analyzer 1.98.1 | `real_pass` for the managed product/provider contract; live model CLI cell `not_tested` |
| macOS, Codex CLI, Python/Pyright 1.1.413 | `real_pass` for the bounded product/provider contract |
| macOS 26.6.2, Codex, Node 24.4.0 / TypeScript Language Server 6.0.0 / TypeScript 5.9.3 | `real_pass`: real JS, JSX, TS and TSX semantic Context plus release-pinned normal shutdown |
| macOS 26.6.2, Claude Code 2.1.267, Node 24.4.0 / TypeScript Language Server 6.0.0 / TypeScript 5.9.3 | `real_pass`: historical semantic/diagnostic spike plus release-pinned normal shutdown and product Context |
| macOS 26.6.2, Claude Code 2.1.267, Go/gopls 0.23.0 | `real_pass`: daemon route, native-edit diagnostic, Diff, Stop and parallel actor isolation |
| macOS 26.6.2, Claude Code 2.1.267, Rust/rust-analyzer 1.98.1 | `real_pass`: daemon semantic context, native-edit diagnostic, Diff and Stop |
| macOS, Claude Code, Python/Pyright | `product_covered`; real host containment remains separately verified |
| Linux, both hosts/providers | `not_tested`; outside the current Mac-only release gate |

Checks cover isolation between divergent worktrees, stale source/provider results, inactive hooks, stop and handoff, daemon/provider crashes, cancellation, bounded queues/output, and preservation of native tool/turn behavior. Productivity evaluation includes correctness, full model-visible context, cold/warm/handoff time, and the complete process tree's resource use. Fewer tool calls alone do not establish improvement.

Caches follow the worktree across stop and sequential handoff. Actor termination does not delete a valid worktree cache; verified worktree closure/reset retires its owned namespace. Hot memory and durable cache are distinct. The scope of this retention guarantee for opaque provider-internal indexes still needs an explicit decision; no provider profile may silently weaken the current strict requirement.

## v0.3 MVP: project problem feed

The current increment is the [EYES-r2](contracts/eyes-v0.3.md) MVP: a shared per-repository daemon
runs confined background `cargo check` and pyright checks for admitted Rust and Python worktrees on
macOS and feeds the active agent a compact `<agent-ide>` problem-count block plus an
`ide.context` `{"kind":"problems"}` reader. Done in the MVP: restart-only launcher admission
(`allowed_roots`, `project_checks`), lease-based shared daemon lifecycle with idle stop, sandboxed
read-only check execution with private per-repository caches under `$HOME/.agent-ide/checks`, the
Claude rendezvous (`/private/tmp/ai-r-…`) and its hook key cache (`/private/tmp/ai-k-…`),
delta-only block emission, problems retrieval, and one bucketed `ProjectCheckCompleted` telemetry
event. Every supported host receives the block: Claude in hook context, Codex through its native
hooks and terminal replies at once (T29B) — and any future host without a hook carrier at the top
of its terminal `ide.*` replies (T28B); Linux stays `not_tested`.

Deferred to phase 2, in this order: warm LSP checks, launchd registration, a per-user service,
subagent attach, TypeScript checks, and Go checks last.

## Later increments

These are roadmap boundaries, not authorization to scaffold their APIs now.

The immediate product priority was one measured integrated coding loop rather than
the previously ordered history-first increment. All four items below shipped: v0.2 delivered
items 1–3 (see the version table), and item 4's acceptance was met — every macOS route in the
matrix below now records `real_pass`/`product_pass` at one release revision (see
[docs/evidence](evidence/)).

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
   refusal, native fallback, restart-safe telemetry and two-worktree isolation —
   met at `2db699f` by all five recorded routes.
   Linux remains explicit `not_tested` until a real host cell passes.

### v0.2 acceptance evidence matrix (ACCEPTANCE-r1)

This matrix consumes the public [TELEMETRY-r1](contracts/telemetry-v0.2.md),
[EDIT-r1](contracts/changes-v0.2.md), and [TYPESCRIPT-r3](contracts/intelligence-v0.2.md)
contracts. The implementation and the real runs happened: every macOS route below now records
`real_pass` (product `product_pass`) at one release revision, in
[docs/evidence](evidence/). The table below is the historical first-run record (recorded
2026-09-14), kept as the context for those routes' earlier failures. Public artifacts contain
versions, route, scenario outcomes, bounded metrics, and explicit truncation only; private
supervisor identifiers and transcripts do not enter them.

| macOS route | Required real evidence | Status (2026-09-14, historical) |
|---|---|---|
| Direct Codex | `start -> context -> edit -> diagnostic -> fix -> diff -> stop`; stale edit has zero writes; native fallback; restart-safe telemetry query/export; divergent worktrees | `failed` on macOS 26.6.2 with Codex CLI 0.154.0: Pyright edit/stale/native behavior and TypeScript semantic Context ran; one Diff inspection completed before a further driver inspection failed without preserving its closed code |
| Direct Claude | The same scenario through the daemon route, including TypeScript only after its spike; stale edit has zero writes; native fallback; restart-safe telemetry; divergent worktrees | `failed` on Claude Code 2.1.267: normal OAuth was available after removing the synthetic-home isolation; daemon Pyright ran in both worktrees, but Diff returned `workspace_authority` and TypeScript remained lexical pending its separate host record |
| Installed external agent-run-to-Claude | The same end-to-end route, with its external attachment treated as an adapter rather than identity authority; stale edit has zero writes; native fallback; restart-safe telemetry; divergent worktrees | `failed` on agent-run 0.11.8 to Claude Code 2.1.267: both divergent Pyright paths ran, Diff inspection returned `capacity`, and the tested Claude launcher had no accepted TypeScript provider |
| Linux, all routes | No v0.2 host/provider acceptance claim | `not_tested` |

This increment does not add a remote analytics service, dashboard, arbitrary shell
execution, multi-file atomicity, semantic rename, test/check orchestration or DAP.
Local statistics need a bounded query/export surface; visualization can be added
after the collected events show that it is useful.

| Version | Added result |
|---|---|
| v0.2 | Local usage telemetry; preferred single-file `ide.edit` with native fallback; JavaScript/TypeScript/Node.js profiles |
| v0.3 | Project problem feed MVP (EYES-r2); the previously listed explicit checks/finish scope (`ide.check`, `ide.finish`) moves beyond the MVP |
| v0.4 | Symbol tools (`ide.outline`, `ide.read`, `ide.symbol`, `ide.graph`, `ide.test`, symbol-addressed `ide.edit`); one `allowed_roots` rule instead of copied host sandbox rights; one-command installer (0.4.3) |
| v0.5 | Language-free core with one crate per language |
| v0.6 | Cross-language name bridge (CSS selectors ↔ HTML/TSX class names); symbol edits in Rust while rust-analyzer is still loading (0.6.2); plain directories without Git, `ide.diff {mode: "task"}` for everything changed since `ide.start`, refusals that name the cause and the next step (0.6.5); edit replies report only diagnostics computed from the post-edit bytes, an inserted then deleted symbol restores the file (0.6.6); one `ide.edit` changes many places in a file and refuses a result that does not parse before writing, one `ide.read` returns many symbols or ranges with one source_ref for the edit, outline and symbol answer from source when the language server fails, project checks name their cause and still run in a confined daemon, Python projects with nested requirements are recognised, a newer front replaces an outdated running daemon (0.6.7, first published as 0.6.9); doctor lists the shared repository daemons in /private/tmp (0.6.9) |
| v0.7 | The agent-* family shape: `AGENTS.md`, `family.toml`, one `cargo xtask check` gate locally and in CI, the exported `schemas/tools.json` contract snapshot, SHA-pinned workflows with cargo-deny, `serverInfo` naming the product, truthful tool annotations, and releases built once then published as a verified draft with `release-manifest.json` |
| v0.8 | MCP 2026-07-28 on rmcp 3.4.0 (family template 0.2.0): modern `tools/list` with private cache hints, legacy sessions unchanged, host choice and Claude hook pairing proven without `initialize` |
| Not scheduled | Items of the original plan not taken up yet: computed criterion assessment and task context compiler, evaluator registry and verification planner, knowledge and publication adapters, multi-file edit journal and recovery, failure context and DAP debugging, independent evaluation |

Scope remains the authority for agreed work. A local bundle can support standalone operation, but its shape is not approval, caller identity, or verification evidence. The daemon does not reinterpret the owner's agreed task. Future verified outcomes require current evidence; v0.1 cannot claim them.
