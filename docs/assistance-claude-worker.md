# Claude foreground helper

A Claude operation does not run inside the daemon today. It runs in a short **foreground
`Bash` helper** that the model launches itself, so the helper inherits the host's own sandbox.
This route predates the `allowed_roots` policy (see [assistance-launcher](assistance-launcher.md));
the same activation gate applies to the worktree and Git directory the helper discovers, and the
optional `ide.start` `root` argument is not yet forwarded to the helper, which discovers from the
target `candidate`. Moving Claude onto the daemon's own route is planned.

The daemon performs **no Git, source-file, provider or process effect for a Claude operation**. For
Edit it first prepares the Changes receipt durably, then mints correlation, hands out one closed
job, and records settlement. SQLite receipt/source-observation writes remain daemon-owned.

The v0.1 five public MCP tools are `start`, `context`, `diff`, `inspect`, `stop`. Proposed v0.2
adds only `edit` under [EDIT-r1](contracts/assistance-v0.2.md); no other new tool, reply state
or launch flag is introduced; the helper handle is the existing
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
   input never selects an executable, shell fragment, scope or permission. Edit additionally
   carries the daemon-selected completed-context digest, length, sequence and revision so the
   helper can reject stale bytes without trusting a path reopened by the daemon.

   Each length-prefixed helper frame is capped at 8 MiB before allocation or JSON decoding: room
   for one rendered owner text of up to 1.25 MiB (a 1 MiB source plus its header) even when every
   byte is a control character that JSON escapes to six bytes. Raw discovery/baseline streams
   remain capped at 8 KiB each. A Context render above the owner-text ceiling fails closed as
   `capacity`; a source is never cut to fit it.

4. **Work.** All Git discovery, baseline capture, source reads, descriptor-safe single-file writes
   and language-server traffic for a
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

Expiry distinguishes two honest cases. An **unclaimed Edit** settles `deadline_no_effect`: no job
was released and the prepared receipt is never left replayable. Other unclaimed tickets vanish.
A **claimed Edit** that loses settlement reports `outcome_unknown(operation_id,path)`; claimed work
is retained with admission quarantined rather than silently reusable.

## Result visibility and settlement order

A claimed result becomes model-visible only after **both** the helper's final frame and the
exact successful `Bash` `PostToolUse` have arrived, in either order. A failed post, a
disconnect, or a missing half yields a finite failure instead of a silent success.

The helper's own post is special: it settles the operation it belongs to and never invalidates
the result that same helper just produced. Ordinary later native posts still advance the native
epoch and invalidate earlier results as usual.

`inspect`, or `context`/`diff` carrying their optional `detail_ref`, is same-binding,
same-generation retrieval only. Retrieval launches no second helper and performs no daemon source
read. Helper-observed source and diagnostic freshness remain provisional rather than presenting
unobserved out-of-band changes as current. A later ordinary native Pre/Post pair may consume that
delta once; delivery advances the binding epoch but does not pretend the daemon re-read source.
A completed helper Context or Diff whose composed text does not fit one reply retains that exact
text as a daemon-side pagination cursor: the reply reports `truncated`/`continuation: true`, and
each further `ide.inspect` on the same `detail_ref` slices the next chunk from the same capture,
with no second helper launch and no daemon re-read, until a chunk reports `continuation: false`.

Paging contract (T16B):

- A source up to the 1 MiB read ceiling is paged whole; the render is never cut to a prefix. Every
  page is cut on a line boundary, and the pages of a Context join to the exact file bytes.
- The reply that settles the helper ticket already delivers page one. The first `ide.inspect` of
  the reply's `detail_ref` therefore returns page two (on the managed path, where page one was
  never delivered, the first inspect still returns page one). Each later call advances by one page.
- Every page of a multi-page result starts with `page N; bytes A-B of TOTAL` (`TOTAL` is the
  source length for Context, the composed text length for Diff). The last page is
  `page N (last); bytes A-TOTAL of TOTAL; complete`. A single-page result carries no marker.
- Inspecting again after the last page re-serves that last page unchanged (idempotent, like a
  single-page result); it never advances past the end.
- `ide.edit` is refused (`stale_source`) on the `source_ref` of a paged Context until its last
  page was delivered: the reference names the whole observed source, but the caller has seen only
  part of it.
- A paged Diff header says `more_available: true`; every hunk is preceded by a `file: <path>` line
  and a page that starts inside a file's hunks begins with `file: <path> (continued)`.
A failed helper finalization retires its transient daemon detail (including a failed Start
mapping), so repeated inspection returns the same bounded failure without consuming the global
detail ceiling.

`stop` is daemon-owned and never launches a helper. It first closes the binding to external calls
and helper claims. Ready Edit proof captured for that exact generation may still settle its
already-prepared receipt through a cleanup-only path; a failed or timed-out known settlement has a
finite `outcome_unknown` fallback and cannot retain ticket, receipt or admission capacity. Stop
then revokes remaining tickets and durable Workspace authority. An already-running unsettled helper
may report late cleanup evidence, but cannot revive authority. Stop success still requires the
daemon's actual provider/revocation settlement.

## Supported profiles

A Claude operation is **per-operation exclusive**: Go, Rust, or Pyright, never more than one, and each helper reaps
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
- **Python** runs the same fixed Pyright defaults profile as Codex for `.py` and `.pyi`. The
  helper carries only the launcher's accepted Pyright script and Node paths, identities, and
  BLAKE3 digests; it reconstructs the profile, verifies the script at the final boundary, and
  retains the inherited-process Node recheck before spawning its one-shot child.
- **TypeScript** uses the same closed `ProjectResolutionInputsV1` and immutable bundle as Codex for
  `.js`, `.jsx`, `.ts`, and `.tsx`, but only when its separate bundle-bound Claude macOS record is
  present. Ticket validation is structural and nonblocking; startup and the receiving helper's
  immediate pre-spawn reconstruction perform the required byte measurements. A missing, copied,
  or mismatched Claude record leaves selection unavailable.

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
- Claude `start`/`context`/`diff` plus durably prepared `edit` minting a single-use ticket and returning `Pending` with the
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
  source metadata, descriptor-safe one-file Edit with exact post-read settlement, composed
  Workspace/Changes Diff, and one provisional diagnostic delta delivered
  through an ordinary later native hook;
- rights-aware durable worktree cache directory retention across Stop/handoff, without a warm native
  backend or opaque-index reuse claim. Claude Go remains per-operation exclusive; the compatible
  two-worktree shared-gopls guarantee belongs only to the managed Codex matrix.
- the fourth closed TypeScript launcher/helper frame, independently bound Claude macOS record, and
  one-shot semantic helper execution with normal child shutdown. The accepted Codex record alone
  still cannot enable Claude selection.

Live checks with Claude Code 2.1.267 on macOS 26.6.2 exercised Go and Rust semantic
context, diagnostic changes after native edits, actual Git diff and Stop. Parallel native
actors in separate roots preserved their own source and diagnostics; an attempted context
request through another actor's attachment was refused, a peer remained usable after Stop,
and a fresh actor activated the stopped worktree with a new authority epoch. These checks
do not establish hot-index retention or formal per-event `model_seen` accounting.
