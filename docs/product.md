# Product requirements

This specifies intended behavior for a new product; it does not describe an implemented release.

## Explicit assistance

Only an explicit `ide.start` activates a coding session. Before activation, no workspace scans, language servers, checks or feedback are started for that actor. An installed inactive hook shim is silent and cheap. A daemon supporting other active actors may exist. `ide.stop` revokes this actor's assistance and releases its leases without rolling back files or stopping peers. Disconnect and explicit stop are different events.

## One coding agent, one worktree

Each actor has one assigned Git worktree and each mutable view has one actor. Multiple frontends belonging to that same actor may attach. A second actor cannot attach even read-only to another actor's mutable view. A parent thread, PID, cwd or a model-supplied ID is not sufficient actor proof. Trusted host binding must distinguish native subagents or activation reports integration unavailable. Inherited environment does not automatically activate child agents.

Support standalone Codex and Claude Code on Linux and macOS. An optional agent-run integration can use a verified run ID as an alias; agent-run is not a mandatory service or identity authority for all agents.

## Useful code operations

The v0.1 product exposes exactly five scenario tools: `ide.start`, `ide.context`, `ide.diff`, `ide.inspect`, and `ide.stop`. Context returns bounded exact source plus generation-matched diagnostics and provisional feedback when available. Diff distinguishes pre-existing, staged, unstaged and untracked changes. Inspect resolves bounded asynchronous results. Missing evidence is not success, and unavailable assistance remains fail-open for ordinary native work.

Unsupported language capabilities and incomplete semantic results must be explicit. Do not expose a raw catalog of LSP methods. Unicode coordinates and source bytes must be exact. The initial vertical slices should exercise Rust and Go; language coverage expands through verified provider profiles, not claims based on installed binaries alone.

## Active feedback

After activation, relevant native edit/shell activity invalidates previous observations and schedules bounded analysis. The IDE delivers actionable facts through tool replies, host hooks and supported subsequent-context delivery. Mere MCP notification acceptance does not prove model visibility. Activation requires a usable active host integration; silently operating as a passive MCP is not the product.

Give only new, relevant problems, evidence-based advice and clear next actions. Separate facts from suggestions. Bound repeated notices by issue/revision/context generation, re-present current critical issues after compaction, report overflow and incomplete coverage. No additional LLM call is required per edit. Stop nudges are off by default and bounded when explicitly configured. Feedback cannot grant user authorization.

## Shared analysis without mixed data

Separate frontend instance, coding session, workspace view and physical backend. Multiple IDE instances reuse a compatible registered backend; compatible views may share a physical LSP only under a verified provider profile. Ordinary LSP multi-root support is not isolation proof. Support native multi-session, validated multi-workspace, and exclusive-context fallback with explicit queue/unavailability. `require_existing` never silently starts a backend. There must be at least one real cross-worktree shared profile in the first usable release.

Single-flight cold starts, versioned request routing, per-view readiness and ownership fencing prevent cross-agent responses. Reject server-initiated writes without proven authorized causality. Stopping one view must not stop peers. Borrowed backends are never killed. Retained owned backends must not keep analyzing disabled workspaces.

## Trustworthy edits and verification

Detect stale results against actual relevant file contents, configuration, toolchain and Git state. Re-read relevant refs/HEAD at edit, diff and finish; watcher events are hints. Stage/commit without changed check inputs need not invalidate a valid check. Worktree moves, recreation, resets, shared-ref changes and daemon restart cannot silently restore stale authority.

API-owned multi-file edits have strict preconditions, per-session serialization and a recovery journal. A partial failure reports actual effects. Never recover by overwriting subsequently changed external bytes. Arbitrary shell commands are not fully controlled without a managed launcher/sandbox.

## Predictable local cost

Every operational tuning value is named typed configuration with units, validation, provenance and controlled reload. Host ceilings cannot be raised by project/session settings. Invalid reload preserves the previous valid configuration. Decreases drain applicable work instead of blindly killing peers.

Bound physical backends, view warmups, process starts, checks, worker threads, queues, memory reservations, watchers, scans, retries, output, caches, logs and events. Account one physical PID once plus marginal view costs and descendants. Separate short interactive work from heavy jobs and prevent starvation. Linux hard controls depend on available OS facilities; macOS RSS monitoring is not a hard-memory guarantee.

## Context efficiency

Use typed internal records and ordinary bounded text/Markdown for model-facing output. Do not dump full LSP/JSON payloads or duplicate structured and text responses by default. Preserve exact selected code plus outcome, freshness, scope and overflow. Expanded details refer to the same typed result. Business logic never parses rendered text. Evaluate actual model tokens, useful-answer accuracy, tool-call count and latency on controlled tasks.

## Acceptance

Prove a real loop: understand code, edit, receive one useful problem report, fix and finish on the latest revision. Exercise both hosts and both OS families; real host delivery and real-provider tests are required in addition to substitutes. Exercise simultaneous divergent worktrees, compatible backend sharing, cancellation, crashes, restarts, stale generations, dropped watcher events and overload. No benchmark result or dependency compatibility is claimed before it is measured.

## Warm review handoff

After the coder releases a worktree, an explicitly activated orchestrator/reviewer can take ownership of that same worktree and reuse compatible analysis state. The old owner is revoked/drained before the successor is admitted; there are never two actors owning one mutable view. Separate reusable workspace/provider cache identity from actor/session identity. Do not reset the entire index merely because a new actor performs the review. Preserve provider-native persistent caches and retain compatible warm backends/views under bounded policy where they can remain quiescent without analyzing an inactive workspace.

Capabilities are provider-specific: live warm state, native persistent cache and incremental rebuild are different guarantees. No generic LSP index export exists by assumption. Record what was reused and which inputs required refresh; cache corruption/config/toolchain/root changes cannot yield stale results. Test coder-to-reviewer handoff and restart restore against a cold baseline, including actual indexing work, latency and bounded memory/disk cost.

## Fail-open assistance

MCP, daemon, language server, storage or hook failure must not block the agent's ordinary tools or prevent its turn/task from ending. Lightweight frontends enforce deadlines independent of a stuck daemon, bound retries and isolate failed components. Hooks fail open and do not ask the agent to repair/retry the IDE indefinitely. A circuit-breaker/cooldown suppresses repeated broken calls and repeated warnings; recovery is explicit or bounded. Static tool discovery and a minimal degraded response must not depend on a healthy domain backend.

A failed IDE operation returns compact effects/uncertainty and a practical native-tool fallback. Missing IDE verification is reported as unavailable, not converted to verified success or an agent-wide stop gate. After a possibly partial edit, fallback first inspects the identified paths/current bytes rather than blindly replaying the mutation. Losing assistance does not broaden existing host/sandbox permissions. Ordinary MCP transport failure is reported to the host with bounded configured handling; host behavior outside supported adapter guarantees is measured and disclosed.

The LSP cache belongs to the Git worktree and has that worktree's lifecycle, not an actor/session lifecycle. Finish, stop, actor exit and reviewer handoff do not delete compatible persistent cache. Verified worktree deletion or explicit cache reset retires it; transient unavailability/move is not deletion. Memory-pressure eviction of a hot process is separate from deletion of persisted worktree cache.
