# v0.3 host feed probe — compact `<agent-ide>` block delivery

Feasibility probe P5, 2026-09-17. Question: can a compact `<agent-ide>` problem block reach a
Claude Code agent after a native edit, only when counts change, ≤256 bytes. Evidence only —
no production code changed. Probe branch: `probe/v03-host-feed` (from `claude/v0-2-release-prep-69807e`).

Scripts: `probes/v03/host-feed/` (hook scripts, two settings files, raw run outputs).

## 1. Current envelope (repo code)

Rendering lives in one function, shared by both host binaries:

- `src/assistance/facade.rs:576-591` — `render_hook_context(event, text)` emits
  `{"hookSpecificOutput":{"hookEventName":<name>,"additionalContext":text}}`. Returns `None`
  (no stdout output at all) for `HookPhase::Pre` and `HookPhase::PermissionDenied`; maps
  Post → `"PostToolUse"`, PostFailure → `"PostToolUseFailure"`, PostBatch → `"PostToolBatch"`.
  Empty or >`MAX_FEEDBACK_BYTES` text → `None` (`src/assistance/reply.rs:9`, 4 KiB).
- Printed to hook stdout at `src/assistance/codex_hook.rs:258-261`; the same binary entry serves
  Claude and Codex (`HostKind::Claude`/`HostKind::Codex` at `src/assistance/codex_hook.rs:241-245`).
- Events wired in `hooks/hooks.json:3-27`: PreToolUse, PostToolUse, PostToolUseFailure,
  PermissionDenied (one hook script each). No UserPromptSubmit, no Stop.
- Input parser accepts only `PreToolUse`/`PostToolUse`/`PostToolUseFailure`
  (`src/assistance/host_binding.rs:1178-1180` Claude, `:1222-1224` Codex).

Deadlines and fail-open:

- Product deadline 250 ms for the whole hook round-trip (450 ms test ceiling):
  `docs/assistance-host-binding.md:187-188,210`.
- Daemon submit transport: `HookTransportLimits::new(128*1024, 64*1024, 1s)` —
  `src/assistance/facade.rs:549-551`; stdin cap `MAX_HOOK_BYTES` 64 KiB (`facade.rs:48`).
- Total protocol deadline 60 s: `docs/assistance-host-binding.md:244`.
- Fail-open: "Failure is local and fail-open" (`docs/assistance-host-binding.md:12`);
  malformed hooks, open stdin, hung-daemon deadlines all exit successfully with no output
  (`docs/assistance-host-binding.md:96-98`); the launcher script exits 0 without a configured
  binary (`hooks/claude-hook.sh:2-8`).

## 2. Real check (Claude Code 2.1.267, haiku) — 2 runs

Command shape (both runs, `HOME=/Users/pluto`, existing login, no credentials touched):

```
claude -p "<prompt: run `true` via Bash, then report verbatim any <agent-ide> block>" \
  --model haiku --strict-mcp-config \
  --settings probes/v03/host-feed/settings-fast.json \
  --allowedTools Bash --output-format json --max-turns 6 [--debug]
```

Settings file adds ONLY the probe hooks (absolute paths into this worktree, `timeout` in seconds).

**Run 1** (`settings-fast.json`): PostToolUse hook sleeping 200 ms + UserPromptSubmit hook,
`timeout: 10`. Prompt asked for two Bash calls.

- UserPromptSubmit IS supported and delivered:
  `<agent-ide>\nerrors 1 · prompt-submit probe\n</agent-ide>` appeared at prompt time.
- PostToolUse delivered after EVERY tool call — the same block repeated after tool 1 and
  tool 2. Conclusion: dedup MUST live in the emitter; the harness repeats the hook context
  per firing without any memory.
- Newlines inside `additionalContext` (JSON `\n`) render as a real multi-line block.
- 200 ms hook: Claude waited; block reached the model.

**Run 2** (`settings-slow.json`): PostToolUse hook sleeping 2 s with declared `"timeout": 1`.

- Model answered exactly `NONE` — the block was dropped.
- Run completed normally, `exit=0`, ~18 s wall — no error surfaced to the model. Hook was
  killed at its declared timeout and the run continued (fail-open, matches repo docs §1).

Raw outputs: `probes/v03/host-feed/run1.json`, `run2.json` (plus `*-debug.log`).

### Working envelope (verbatim, proven end-to-end)

```json
{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"<agent-ide>\nerrors 3 · warnings 5 · checked 0s ago\n</agent-ide>"}}
```

For prompt-time delivery the same shape with `"hookEventName":"UserPromptSubmit"` also works.

## 3. Timing

- 200 ms hook → waited, delivered (run 1).
- 2 s hook with `timeout: 1` → killed at declared timeout, dropped silently, run unaffected (run 2).
- Claude itself did not impose a sub-second cutoff; the practical budget is the declared
  `timeout` field (default 60 s), but the Agent IDE product deadline is 250 ms
  (`docs/assistance-host-binding.md:187`), so a v0.3 feed must serve cached counts, not run
  a live check inside the hook path.

## 4. Unusable events

- Repo-side: only PreToolUse/PostToolUse/PostToolUseFailure are parsed
  (`src/assistance/host_binding.rs:1178-1180`), and PreToolUse/PermissionDenied never render
  context (`src/assistance/facade.rs:582-585`). `hooks/hooks.json` wires exactly these four.
- Docs-level: no local copy of the Claude Code hooks reference exists in the repo; the only
  in-repo Stop mention is generation/settlement semantics (`src/assistance/host_binding.rs:469,
  996-1017`), i.e. Stop closes a generation rather than annotating the next step. Stop and
  SubagentStop are continuation-control events (their output decides whether the agent stops),
  not context-annotation events, so they are unusable for this feature. PostToolUse is the
  right carrier; UserPromptSubmit is a viable supplement (proven in run 1).

## 5. Codex (docs/code only — real runs forbidden)

- `src/assistance/codex_hook.rs:258-261` prints the same Claude-shaped `hookSpecificOutput`
  envelope for `HostKind::Codex` too.
- `docs/examples/codex-hooks.toml` wires only PreToolUse/PostToolUse with `timeout = 1` (seconds).
- No in-repo evidence that Codex consumes hook stdout as model context; `hookSpecificOutput`
  is a Claude contract, and for Codex the envelope is printed with no documented consumer.
  Status: emitter-side printing exists; host-side context injection unproven (and unverifiable
  here). Treat Codex as does-not-work-until-proven; a Codex-native mechanism (if any) needs its
  own probe with real quota.

## Verdict

**Works (Claude).** PostToolUse `hookSpecificOutput.additionalContext` reliably reaches the
model, multi-line ≤256-byte blocks render correctly, and 200 ms is well inside the budget.
Dedup must be emitter-owned because Claude repeats the context on every hook firing.

## Recommended delivery design (Claude)

- **Event:** PostToolUse, matcher `""` (fires for native edits and every other tool; the block
  only prints when counts changed). Do not use PreToolUse/PermissionDenied (renderer returns
  `None`), Stop/SubagentStop (continuation semantics, not parsed here).
- **Dedup:** hash the diagnostic message set — `diagnostic_fingerprint` already exists
  (`src/assistance/facade.rs:612-625`, blake3 over ordered messages) — and print the block only
  when the fingerprint changes; otherwise emit nothing (silence is the common case).
- **Size:** ≤256 B target fits easily under `MAX_FEEDBACK_BYTES` (4 KiB, `reply.rs:9`).
- **Latency:** serve last-known counts from the daemon; the hook path has a 250 ms product
  deadline and run 2 proves an over-time hook is silently dropped — never check diagnostics
  synchronously inside the hook.
- **Failure mode:** stay fail-open (exit 0, no output) exactly as the current pipeline does.
- **Codex:** keep out of scope until a real-quota probe shows Codex consumes hook stdout.
