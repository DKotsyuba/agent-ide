# v0.3 MVP acceptance — real Claude Code (T104)

Task T104, Agent IDE v0.3 MVP. Real Claude Code acceptance of the project problem feed
(EYES-r2), run from worktree `v03/acceptance` on top of `feature/v03-eyes` at `495f0f8`
(contains T102 `feat(assistance): wire project problem feed` merged at `3be8677`, plus the
`T03B` review-fix merge `495f0f8`; `docs/examples/launcher-eyes.json` and T103 docs do not
exist yet on this base, per the task's deviation note). Candidate binary: `cargo build
--locked --release` at this worktree's HEAD, `target/release/agent-ide`.

Host: macOS 27.0 (26A428), arm64. Real Claude Code CLI `2.1.274`, model
`claude-haiku-4-5-20251001` (alias `haiku`). Pinned toolchains from operator environment:
Rust `1.98.1` (`/Users/pluto/.rustup/toolchains/1.98.1-aarch64-apple-darwin`), Node `v24.4.0`
(`/Users/pluto/.nvm/versions/node/v24.4.0/bin/node`), Pyright `1.1.413`
(`/opt/homebrew/bin/pyright`, a symlink to
`/opt/homebrew/lib/node_modules/pyright/index.js`, the entry module the daemon executes
directly under `node` as `project_checks.python.pyright_cli`).

## Result summary

| # | Scenario | Result | Evidence |
|---|----------|--------|----------|
| 1 | Rust: edit crate `a`, auto block, `ide.context problems`, no-repeat | **real_pass** | §1, real Claude session |
| 2a | Python with real `.venv`: same pattern | **not_executed** | 8-run budget exhausted before this scenario (§4) |
| 2b | Python without `.venv`: `environment not found` | **not_executed** | same |
| 3 | Two worktrees of one repo share one daemon | **not_executed** | same |
| 4 | Idle: daemon runtime dir removed after `idle_timeout_s` | **real_pass** (direct-lease substitution) | §2 |
| 5a | Confinement: worktree outside `allowed_roots` → `outside allowed roots` (Claude-visible) | **not_executed** | same |
| 5b | Confinement: sandboxed read-outside-sentinel / TCP egress denial (spawn path) | **real_pass** (existing product test suite) | §3 |

Business criterion confirmed on a real Claude Code session: a block after a breaking native
edit with no `ide.*` tool call, no repeated block on a following no-op, and a correct
`ide.context {"kind":"problems"}` follow-up. Scenarios 2, 3, and 5a were not exercised for
real — see §4 for why, and §5 for what would need to happen to close them out.

## §1. Scenario 1 — Rust workspace, real Claude session (real_pass)

Fixture: `tests/fixtures/eyes/rust-workspace/` (workspace `a`/`b`, `b` calls `a::combine`),
copied to a disposable git repo and driven by
`scripts/acceptance-drivers/eyes-claude.sh rust`.

Command (as run by the driver):
```
cd /private/tmp/aiv3-acc-run3/rust1
HOME=/Users/pluto AGENT_IDE_BIN=<candidate> CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS=0 \
PATH=/usr/bin:/bin:/usr/sbin:/sbin \
env -u ANTHROPIC_AUTH_TOKEN -u ANTHROPIC_BASE_URL -u ANTHROPIC_API_KEY -u ANTHROPIC_MODEL \
  -u ANTHROPIC_SMALL_FAST_MODEL -u CLAUDECODE -u CLAUDE_CODE_CHILD_SESSION \
  -u CLAUDE_CODE_ENTRYPOINT -u CLAUDE_CODE_EXECPATH -u CLAUDE_CODE_MESSAGING_SOCKET \
  -u CLAUDE_CODE_MESSAGING_TOKEN -u CLAUDE_CODE_SESSION_ID -u CLAUDE_PID -u CLAUDE_EFFORT \
claude -p "<scenario prompt>" --model haiku --output-format json --max-turns 30 \
  --dangerously-skip-permissions --strict-mcp-config --mcp-config <mcp-config.json> \
  --settings <settings.json> --plugin-dir <this worktree>
```

`<mcp-config.json>` registers `agent-ide mcp --claude-launcher-template <launcher.json>`.
`<settings.json>` is `{"sandbox":{"network":{"allowUnixSockets":["/private/tmp/ai-r-<hash>/claude-helper.sock"]}}}`,
where `<hash>` is the first 16 lowercase hex characters of BLAKE3 of the fixture repo's
canonical git common directory (`git -C <repo> rev-parse --path-format=absolute
--git-common-dir`, canonicalized), matching `claude_rendezvous_identity` in `src/main.rs`.
`<launcher.json>` carries `allowed_roots: ["/private/tmp/aiv3-acc-run3"]` and
`project_checks: {debounce_ms:100, idle_timeout_s:30, check_timeout_s:60, rust:{toolchain_dir},
python:{node, pyright_cli}}`, per `docs/contracts/eyes-v0.3.md` §1 and the parser in
`src/assistance/launcher.rs` (`cargo_home` omitted: the parser does not accept it yet).

Session metadata (`--output-format json`): `session_id=017e58dd-f319-4b6b-b551-e2ad30694fd8`,
`num_turns=11`, `duration_ms=37121`, `is_error=false`, `stop_reason=end_turn`.

Model's final answer (`.result`, verbatim):
```
START: Workspace activated; authority_epoch: 2; baseline: partial (Unverified; durable capture true); worktree_cache: retained. Provider readiness is not implied.
POST_EDIT_BLOCK: rust: 1 error (+1), 0 warnings (partial) | python: environment not found
PROBLEMS: crates/b/src/lib.rs:5:5 error [E0061] this function takes 3 arguments but 2 arguments were supplied
NOOP_BLOCK: NONE
TOOLS_CALLED: ToolSearch, mcp__agent-ide__ide_start, Bash, mcp__agent-ide__ide_inspect, Bash, Read, Edit, Bash, mcp__agent-ide__ide_context, Bash
```

Reading `TOOLS_CALLED` against the prompt's numbered steps: `ide.start` (pending) → `Bash`
(the printed foreground helper, run byte-for-byte) → `ide.inspect` (resolves the pending
ticket to `Workspace activated`) → `Bash` (`sleep 3`, pre-edit settle) → `Read` + `Edit`
(the native breaking edit to `crates/a/src/lib.rs`, never touching `crates/b`) → `Bash`
(the first post-edit no-op call — this is where `POST_EDIT_BLOCK` was captured, with **no**
`ide.*` tool call between the edit and it) → `ide.context {"kind":"problems"}` (only called
*after* the block was already captured, returning the exact `crates/b:5:5` arity error) →
`Bash` (a further no-op, whose block came back `NONE` — the identical-state block is not
re-emitted, matching `docs/contracts/eyes-v0.3.md` §6). This is the literal business
criterion from the task brief: a block after a breaking edit with no intervening `ide.*`
call, followed by a correct `ide.context` problems lookup, followed by a no-op step with no
repeated block.

The block text itself matches the contract's rendered shape exactly: `rust: 1 error (+1), 0
warnings (partial) | python: environment not found` — `(+1)` is the delta against the
previous (0-error) delivered block, `(partial)` marks `CheckState::Partial` (crate `a`
compiled, dependent crate `b` did not, so `build-finished` still lets the whole check land as
`Partial` rather than `Fatal`), and `python: environment not found` is the fixed
`Unavailable(EnvMissing)` text for a Rust-only fixture with no `.venv` — all three exactly as
specified in `docs/contracts/eyes-v0.3.md` §4/§6.

## §2. Scenario 4 — idle shutdown (real_pass, direct-lease substitution)

Not run through a live Claude session (budget; §4). Verified instead by driving the exact
same managed-Claude code path Claude's MCP subprocess uses
(`agent-ide mcp --claude-launcher-template <launcher.json>` with `CLAUDE_PROJECT_DIR` set to
the scenario-1 fixture, `idle_timeout_s: 30` from the same launcher template), holding its
`ClientLease` open for 2 s, then terminating the client (SIGTERM to the process this
invocation itself started) to release the lease:

```
runtime=/private/tmp/ai-r-da26961352fccc39
--- killing MCP client to release its lease at 17:50:16 ---
released at 17:50:16
removed after 26s
```

The runtime directory (lock, sockets, `state.sqlite`, `attachment`) was fully populated at
spawn and completely removed 26 s after the last lease closed, consistent with
`idle_timeout_s: 30` plus the daemon's own countdown-start/cleanup latency
(`docs/contracts/eyes-v0.3.md` §2: "the daemon cancels checks, kills their process groups,
closes the socket and removes its runtime directory"). This does not cover the two-worktree
warm-cache-reuse half of scenario 3 (not executed; §4), only the idle-timer mechanics shared
by every worktree count.

## §3. Scenario 5b — confined spawn path (real_pass, existing product test suite)

Not a new acceptance addition: `tests/check_confinement_contract_sandbox.rs` already drives
the identical `run_confined`/`sandbox-exec` spawn path the Rust and Python project checkers
use (`src/checks/runner.rs`, `src/execution/seatbelt.rs`), against real `sandbox-exec`
fixtures. Run for this task as supporting evidence for "a build.rs attempting to read a
sentinel outside roots or open a TCP connection fails":

```
$ cargo test --locked --release --test check_confinement_contract_sandbox -- \
    read_outside_sentinel_fails network_connection_is_denied_without_timeout --test-threads=1
test network_connection_is_denied_without_timeout ... ok
test read_outside_sentinel_fails ... ok
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 11 filtered out; finished in 0.07s
```

The root-admission half of scenario 5 (`outside allowed roots`) is likewise already covered
offline by `tests/check_confinement_contract.rs`'s `admit_worktree`/`RootAdmissionError`
tests (`config_admission_rejects_outside_worktrees`,
`config_admission_rejects_symlink_escape`, `config_admission_rejects_prefix_trap_siblings`,
etc. — 14/14 passed here). What is **not** covered by either suite, and was not exercised
live, is a real Claude session in a worktree outside `allowed_roots` literally rendering the
fixed `outside allowed roots` text in its own turn (scenario 5a in the results table).

## §4. Why scenarios 2, 3, and 5a were not executed

The task brief bounds this acceptance to **at most 8 real Claude sessions**. All 8 were
spent before scenario 1 could even be attempted correctly, on two genuine problems in this
acceptance harness's own launcher configuration — not in the product under test:

1. **Incomplete launcher schema** (harness bug). `--claude-launcher-template` requires the
   full v0.2 schema (`version`, `limits`, one placeholder `targets[0]` with real `git`/`codex`
   accepted-executable evidence, a real `AcceptedProfile` record, and a strict
   `claude_profile`) *in addition to* the v0.3 `allowed_roots`/`project_checks` fields — a
   template missing any of these fails the managed daemon spawn before any `project_checks`
   logic runs at all (`src/main.rs::start_managed_daemon`,
   `src/assistance/launcher.rs::LauncherConfig::parse`). Runs 1–3 of 8 (`ide.start` returning
   `daemon unavailable` / `invalid bounded parameters`) diagnosed this without spending a full
   scenario attempt each; fixed by building the target with the existing
   `agent-ide evidence executable` / `agent-ide evidence record` CLI helpers
   (`write_launcher_template` in `scripts/acceptance-drivers/eyes-claude.sh`).
2. **`operation_ms` too short for a real model turn** (harness bug). The foreground-helper
   ticket a `Pending` `ide.start` reply mints expires `operation_ms` after mint
   (`src/assistance/assembly.rs`); this driver's first working config copied the v0.2 unit-test
   default of `1000` ms verbatim from `tests/check_confinement_contract.rs`'s `v02_config()`
   helper. A real Claude turn needs several real seconds between receiving the `Pending` reply
   and actually issuing the follow-up `Bash` call (tool search + inference latency), so every
   real attempt at that setting expired the ticket first (`claude-worker: refused` →
   `ide.inspect` → `error: invalid_detail`) — confirmed by comparing the daemon-side hook log
   timestamps for the `ide.start` mint and the `Bash` `PreToolUse` recognition, roughly 3.3 s
   apart, against the 1 s deadline. Runs 4–5 of 8 diagnosed this (one with a `stream-json`
   transcript captured for debugging only, one with a logging wrapper plugin, both outside the
   committed driver). Fixed by raising `operation_ms` to `120000` in the shared launcher
   template.
3. Runs 6–8 of 8 were the first three real attempts at scenario 1 itself: run 6 (block never
   appeared — the prompt's own filler `sleep` calls between the edit and the capture step are
   themselves `CHECK_TRIGGER_TOOLS`, so they kept resetting the check's debounce window before
   the capture read could see a settled result); run 7 (block appeared correctly, but the
   prompt polled `ide.context` *before* the automatic-block capture step, so it no longer
   demonstrated "no `ide.*` call between edit and block"); run 8 (§1) — real_pass, with the
   prompt fixed to capture the block on the very next native tool call and only call
   `ide.context` afterward.

No product defect blocked any scenario. Every fix above is confined to this driver's own
launcher-template construction and prompt wording in
`scripts/acceptance-drivers/eyes-claude.sh`; `src/` was not touched. Scenarios 2 (Python with
and without a real `.venv`), 3 (two worktrees sharing one daemon, with cache-reuse timing),
and 5a (a live Claude session in a worktree outside `allowed_roots`) are implemented in
`scripts/acceptance-drivers/eyes-claude.sh` (`python-venv`, `python-noenv`, `worktree-a`,
`worktree-b`, `confinement`) and ready to run — they were simply never executed live, since
doing so would have exceeded the 8-run budget on top of the runs already spent making
scenario 1 pass for real.

## §5. Reproducing / finishing the remaining scenarios

```
cd <this worktree>
cargo build --locked --release
export AGENT_IDE_ACCEPTANCE_WORKDIR=/private/tmp/aiv3-acc-<pid>
./scripts/acceptance-drivers/eyes-claude.sh rust           # real_pass, reproduced in §1
./scripts/acceptance-drivers/eyes-claude.sh python-venv    # not yet run for real
./scripts/acceptance-drivers/eyes-claude.sh python-noenv   # needs python-venv's $WORKDIR/py-venv first
./scripts/acceptance-drivers/eyes-claude.sh worktree-a     # not yet run for real
./scripts/acceptance-drivers/eyes-claude.sh worktree-b     # needs worktree-a's $WORKDIR/rust-origin first
./scripts/acceptance-drivers/eyes-claude.sh idle           # no Claude run; polls the worktree-a/b runtime dir
./scripts/acceptance-drivers/eyes-claude.sh confinement    # not yet run for real
```
That is 5 further real Claude sessions (`python-venv`, `python-noenv`, `worktree-a`,
`worktree-b`, `confinement`), within a fresh 8-run budget.

## Limitations

- `--output-format json` (as the task brief specifies) returns only the model's own final
  answer and session metadata, not a per-tool-call transcript. Every scenario's evidence is
  therefore the model's *self-reported* account of what it saw and did (structured with fixed
  markers so it stays checkable), not an independently captured wire-level trace of hook
  `additionalContext` payloads or the exact tool-call order. The v0.2 drivers'
  `require_tool_use`/`forbid_transcript_text` style checks over a `stream-json` transcript
  were not available under `json` output; `TOOLS_CALLED` in §1 is the model's own account,
  cross-checked only against its own step numbering and against `ide.context`'s independent
  answer for `crates/b`.
- Only one model (`haiku`) and one host macOS version were exercised; no coverage of other
  Claude models or of Linux/Codex hosts (both explicitly out of scope per
  `docs/contracts/eyes-v0.3.md`).
- Scenario 3's specific "second worktree's first check reuses the cloned cache, faster than
  the first" comparison was not measured (scenario not executed).
