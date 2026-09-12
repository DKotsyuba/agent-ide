# Claude foreground helper

Claude Code never supplies the measured sandbox metadata Codex returns in
`codex/sandbox-state-meta`, and its native tool lifecycle offers no long-lived process the
daemon may borrow. A Claude operation therefore does not run inside the daemon. It runs in a
short **foreground `Bash` helper** that the model launches itself, so the helper inherits the
host's own real sandbox instead of a sandbox the daemon claims to have measured.

The daemon performs **no Git, source, provider or process effect for a Claude operation**. It
mints correlation, hands out one closed job, and records settlement.

The five public MCP tools are unchanged: `start`, `context`, `diff`, `inspect`, `stop`. No new
tool, reply state or launch flag is introduced; the helper handle is the existing
`Pending.detail_ref`.

The sections below describe the complete contract. **Implementation status** at the end of this
document states precisely which parts are wired today and which are not yet, so no reader mistakes
the contract for shipped behaviour.

## Operation flow

1. **Mint.** A validated Claude MCP call creates a pending operation and returns the ordinary
   bounded `Pending` reply. Its text carries the exact foreground command the model must run
   before inspecting, and the reply returns promptly, before any helper work happens.

2. **Launch.** The model runs that command as an ordinary foreground `Bash` call. The normal
   `PreToolUse` hook compares the payload against the daemon-stored expected bytes and matches
   only on byte-exact equality with `run_in_background = false`. The hook then stays **silent**:
   it returns no `permissionDecision`, no `updatedInput` and no rewritten command, so Claude's
   own permission and sandbox evaluation of the unchanged command is what actually authorizes
   the launch. Every other tool payload is discarded. A launch that is denied or never happens
   simply expires with no effect.

3. **Claim.** The helper runs as the fixed command

   ```text
   <agent-ide> claude-worker --runtime-dir <dir> --attachment <opaque> --detail-ref <handle>
   ```

   It opens `<runtime-dir>/claude-helper.sock`, a private owner-only (mode 0600) endpoint the
   daemon binds after Application owns the daemon lock and drops before provider cleanup, and
   claims the bound operation exactly once. It receives a closed daemon-selected job: target and root, accepted
   programs and settings, the known worktree/canonical authority, the selected operation and its
   already validated parameters, finite byte/process/deadline budgets, and the cache scope. Model
   input never selects an executable, shell fragment, scope or permission.

   Each length-prefixed helper frame is capped at 256 KiB before allocation or JSON decoding. Raw
   discovery/baseline streams remain capped at 8 KiB each and rendered owner text at 32 KiB.

4. **Work.** All Git discovery, baseline capture, source reads and language-server traffic for a
   Claude operation happen inside that helper, under the sandbox it inherited from `Bash`. The
   daemon reads no source at any point, including during `inspect` freshness checks and feedback
   emission. Reading trusted operator configuration and accepted binaries is management, not a
   source effect.

5. **Settle.** The helper owns, cancels, drains and reaps its own direct Git and provider
   children, reports real child-settlement counts, closes and exits. The daemon treats the helper
   endpoint as host-managed and borrowed: it never claims a direct-child reap for the helper's
   PID and never kills a borrowed numeric PID.

## What a ticket is and is not

A ticket establishes **correlation and replay exclusion**. It is not sandbox attestation and
proves nothing about what the host actually enforced.

Actor identity is checked at the **native pre-hook**, where the host itself states who ran the
command — not at the socket. A helper process is never asked for its own actor identity, because
it could only repeat back a value it was handed.

A claim is refused, with no job released and therefore no effect of any kind, when:

| Condition | Outcome |
|---|---|
| Handle presented without a recognized native launch (a bare copied reference) | `invalid_detail` |
| A launch by a different actor — including the parent session running a subagent's exact command — which never arms the ticket at all | `invalid_detail` |
| A different transport channel | `invalid_detail` |
| A stale or revoked binding generation | `workspace_authority` |
| A second or replayed claim of the same handle | `invalid_detail` |
| Deadline already expired | `deadline` |

Expiry distinguishes two honest cases. An **unclaimed** ticket vanishes with no effect: nothing
ran, so nothing needs cleanup or reporting. A **claimed** ticket that never settled becomes
uncertain and is retained, keeping its admission quarantined rather than silently reusable.

## Result visibility and settlement order

A claimed result becomes model-visible only after **both** the helper's final frame and the
exact successful `Bash` `PostToolUse` have arrived, in either order. A failed post, a
disconnect, or a missing half yields a finite failure instead of a silent success.

The helper's own post is special: it settles the operation it belongs to and never invalidates
the result that same helper just produced. Ordinary later native posts still advance the native
epoch and invalidate earlier results as usual.

`inspect` for Claude is same-binding, same-generation retrieval only. It performs no daemon source
read, and labels helper-observed source and diagnostic freshness provisional rather than presenting
unobserved out-of-band changes as current. A later ordinary native Pre/Post pair may consume that
delta once; delivery advances the binding epoch but does not pretend the daemon re-read source.

`stop` revokes and cancels first. It is reported complete only on actual child settlement;
otherwise the outcome is uncertain or a deadline, and the admission stays quarantined.

## Supported profiles

A Claude operation is **per-operation exclusive**: Go or Rust, never both, and each helper reaps
its provider before exiting.

- **Rust** runs with both `cachePriming` and `procMacro` disabled. This is a Claude-specific
  effective configuration; the shared managed path disables cache priming only, and is unchanged. Disabling proc-macro
  expansion is a real semantic limitation, not a tuning choice: derive-generated methods,
  macro-expanded items and references into them are invisible to the analyzer, so Rust results
  for such symbols are incomplete under this profile and are reported as such.
- **Go** runs a helper-private, non-shared logical view. **Shared multi-worktree gopls is
  unavailable for Claude**, because a foreground helper lifecycle cannot retain a safe shared
  listener across operations. That matrix cell remains covered by the existing Codex shared
  listener, which is unchanged.

Stop and handoff retain the IDE-owned worktree cache directories; no analysis runs while inactive.
The daemon derives their private absolute paths from the durable worktree identity/incarnation,
daemon nonce, accepted provider/profile/toolchain/trust, and strict-Claude rights identity. The
helper receives only that selected directory and derives no host path from launcher labels or model
input. Each provider process is still one-shot and is reaped before helper exit: no listener or
in-memory index survives, and retained opaque provider files are not claimed as a proven warm or
reusable native index.

## Authority contract

This path is accepted only under an **operator-managed strict Claude configuration on macOS**,
declared explicitly in launcher configuration:

- sandboxing enabled, and configured to fail rather than run unsandboxed when unavailable;
- `allowUnsandboxedCommands` false;
- no `excludedCommands` entry matching the fixed helper command;
- read, write, network and socket scope declared to match the documented helper needs;
- an accepted helper binary identity, and trusted same-UID host processes and configuration.

The daemon never inspects, guesses or mutates live host settings. An absent or mismatched
profile leaves the Claude path unavailable; it never degrades to unrestricted execution and
never adds an approval prompt. This matches the accidental cross-actor and out-of-authority
threat model — it is not malicious same-UID OS attestation.

**Linux remains unavailable** pending equivalent proof of the same guarantees.

## Execution boundary

A helper's children are spawned through a distinct inherited-sandbox boundary, separate from the
managed Codex path. The ticket claim already consumes one lease from the daemon's shared Execution
controller; sequential Git/provider children consume no second lease or private budget. This path
takes no observed sandbox state and uses no wrapper executable, so no synthetic sandbox observation
can reach Codex Execution through it. It distinguishes a child that provably never started from one
that started but could not be settled, so unreaped children are reported rather than assumed.

## Implementation status

Wired and covered by local checks:

- optional strict `claude_profile` on a launcher target, rejected at load when declared weakened;
- Claude `start`/`context`/`diff` minting a single-use ticket and returning `Pending` with the
  complete, untruncatable helper command in the summary the model reads;
- Claude `Bash` pre-hook selection of the command and background flag, exact-byte recognition with
  actor enforcement, and silent handling with no permission decision or updated input;
- the private claim/finish socket, the `claude-worker` command, one-use claiming, channel and
  generation fencing, replay refusal, ingress expiry, and quarantined uncertainty;
- the inherited-sandbox child boundary and the helper's real fixed Git discovery with measured
  child settlement;
- Claude `inspect` as pure same-generation retrieval with no daemon source read, `stop` revoking
  the ticket ledger first, and a helper's own post settling instead of invalidating its result;
- canonical durable Workspace activation plus a partial/unverified stored baseline built from six
  real settled Git children, with unknown baseline coverage preserved on invalid capture;
- helper-owned bounded source reads, one-shot exclusive Go and Rust provider sessions, durable
  source metadata, composed Workspace/Changes Diff, and one provisional diagnostic delta delivered
  through an ordinary later native hook;
- rights-aware durable worktree cache directory retention across Stop/handoff, without a warm native
  backend or opaque-index reuse claim. Claude Go remains per-operation exclusive; the compatible
  two-worktree shared-gopls guarantee belongs only to the managed Codex matrix.

No live Claude host acceptance or `model_seen` proof has been run.
