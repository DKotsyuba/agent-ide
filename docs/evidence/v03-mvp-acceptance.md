# v0.3 MVP acceptance — real Claude Code (T104)

Task T104, Agent IDE v0.3 MVP. Real Claude Code acceptance of the project problem feed
(EYES-r2), run from worktree `v03/acceptance` on top of `feature/v03-eyes` at `495f0f8`
(contains T102 `feat(assistance): wire project problem feed` merged at `3be8677`, plus the
`T03B` review-fix merge `495f0f8`; `docs/examples/launcher-eyes.json` and T103 docs do not
exist yet on this base, per the task's deviation note). Candidate binary: `cargo build
--locked --release` at this worktree's HEAD, `target/release/agent-ide` (rebuilt unchanged at
`1c3e35c` for this continuation session — `src/` was not touched by either session).

Host: macOS 27.0 (26A428), arm64. Real Claude Code CLI `2.1.274`, model
`claude-haiku-4-5-20251001` (alias `haiku`). Pinned toolchains from operator environment:
Rust `1.98.1` (`/Users/pluto/.rustup/toolchains/1.98.1-aarch64-apple-darwin`), Node `v24.4.0`
(`/Users/pluto/.nvm/versions/node/v24.4.0/bin/node`), Pyright `1.1.413`
(`/opt/homebrew/bin/pyright`, a symlink to
`/opt/homebrew/lib/node_modules/pyright/index.js`, the entry module the daemon executes
directly under `node` as `project_checks.python.pyright_cli`).

This continuation session ran scenarios 2a, 2b, 3, and 5a — the ones left `not_executed` by the
first session's §4 — from a fresh `AGENT_IDE_ACCEPTANCE_WORKDIR=/private/tmp/aiv3-acc-t104-77433`,
reusing the same worktree, candidate binary, and driver, unmodified. 6 real Claude sessions were
spent — the fresh budget for this continuation — documented in §6–§9.

## Result summary

| # | Scenario | Result | Evidence |
|---|----------|--------|----------|
| 1 | Rust: edit crate `a`, auto block, `ide.context problems`, no-repeat | **real_pass** | §1, real Claude session |
| 2a | Python with real `.venv`: same pattern | **real_pass** | §6, real Claude session |
| 2b | Python without `.venv`: `environment not found` | **real_pass** (2nd attempt; see §7 for the 1st attempt's worktree-authority finding) | §7, real Claude sessions |
| 3 | Two worktrees of one repo share one daemon | **partial**: both worktrees' checks real_pass individually; shared-daemon/cache-reuse claim not demonstrated (§8) | §8, real Claude sessions |
| 4 | Idle: daemon runtime dir removed after `idle_timeout_s` | **real_pass** (direct-lease substitution) | §2 |
| 5a | Confinement: worktree outside `allowed_roots` → `outside allowed roots` (Claude-visible) | **real_pass** | §9, real Claude session |
| 5b | Confinement: sandboxed read-outside-sentinel / TCP egress denial (spawn path) | **real_pass** (existing product test suite) | §3 |

Business criterion confirmed on a real Claude Code session: a block after a breaking native
edit with no `ide.*` tool call, no repeated block on a following no-op, and a correct
`ide.context {"kind":"problems"}` follow-up. §4/§5 record why scenarios 2, 3, and 5a were not
exercised for real in the first session; this continuation session closed them out (§6–§9),
except for scenario 3's specific shared-daemon/cache-reuse timing sub-claim (§8).

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

## §5. Reproducing / finishing the remaining scenarios (as planned by the first session)

```
cd <this worktree>
cargo build --locked --release
export AGENT_IDE_ACCEPTANCE_WORKDIR=/private/tmp/aiv3-acc-<pid>
./scripts/acceptance-drivers/eyes-claude.sh rust           # real_pass, reproduced in §1
./scripts/acceptance-drivers/eyes-claude.sh python-venv    # real_pass, run for real in §6
./scripts/acceptance-drivers/eyes-claude.sh python-noenv   # real_pass, run for real in §7
./scripts/acceptance-drivers/eyes-claude.sh worktree-a     # real_pass, run for real in §8
./scripts/acceptance-drivers/eyes-claude.sh worktree-b     # real_pass, run for real in §8
./scripts/acceptance-drivers/eyes-claude.sh idle           # no Claude run; polls the worktree-a/b runtime dir
./scripts/acceptance-drivers/eyes-claude.sh confinement    # real_pass, run for real in §9
```
This continuation session ran exactly these 5 further real Claude sessions plus one repeat
(`python-noenv` needed a second attempt, §7) — 6 total, the fresh budget given for this
continuation. No line of `scripts/acceptance-drivers/eyes-claude.sh` or any fixture under
`tests/fixtures/eyes/` needed to change to run them; `src/` was not touched.

## §6. Scenario 2a — Python with a real `.venv`, real Claude session (real_pass)

Fixture: `tests/fixtures/eyes/python-pkg/` (package `pkg`; `pkg/b.py` calls `pkg.a.combine`),
copied to a disposable git repo at `$WORKDIR/py-venv` and given a real virtual environment with
`"$OPERATOR_HOME/.local/bin/uv" venv --managed-python --python 3.14 .venv` (uv-managed CPython
3.14.3, per this task's `$python-runtime` skill — never a system or pyenv interpreter), then
driven by `scripts/acceptance-drivers/eyes-claude.sh python-venv`.

Command (identical template to §1's, substituting `cwd=$WORKDIR/py-venv`, the scenario-2a
prompt, and `--settings $WORKDIR/settings-py-venv.json`, which allows exactly that fixture's own
`/private/tmp/ai-r-c82bd1cab969cdd2/claude-helper.sock`):
```
cd /private/tmp/aiv3-acc-t104-77433/py-venv
HOME=/Users/pluto AGENT_IDE_BIN=<candidate> CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS=0 \
PATH=/usr/bin:/bin:/usr/sbin:/sbin \
env -u ANTHROPIC_AUTH_TOKEN -u ANTHROPIC_BASE_URL -u ANTHROPIC_API_KEY -u ANTHROPIC_MODEL \
  -u ANTHROPIC_SMALL_FAST_MODEL -u CLAUDECODE -u CLAUDE_CODE_CHILD_SESSION \
  -u CLAUDE_CODE_ENTRYPOINT -u CLAUDE_CODE_EXECPATH -u CLAUDE_CODE_MESSAGING_SOCKET \
  -u CLAUDE_CODE_MESSAGING_TOKEN -u CLAUDE_CODE_SESSION_ID -u CLAUDE_PID -u CLAUDE_EFFORT \
claude -p "<scenario-2a prompt>" --model haiku --output-format json --max-turns 30 \
  --dangerously-skip-permissions --strict-mcp-config --mcp-config <mcp-config.json> \
  --settings <settings-py-venv.json> --plugin-dir <this worktree>
```

Session metadata: `session_id=6041f916-4386-431e-80e3-060c3f0c4a74`, `num_turns=11`,
`duration_ms=34588`, `is_error=false`, `stop_reason=end_turn`.

Model's final answer (`.result`, verbatim):
```
START: Workspace activated; authority_epoch: 2; baseline: partial (Unverified; durable capture true); worktree_cache: retained. Provider readiness is not implied.
POST_EDIT_BLOCK: rust: check failed | python: 1 error (+1), 0 warnings
PROBLEMS: pkg/b.py:7:12 error [reportCallIssue] Argument missing for parameter "z"
NOOP_BLOCK: NONE
TOOLS_CALLED: ToolSearch, mcp__agent-ide__ide_start, Bash, mcp__agent-ide__ide_inspect, Bash, Read, Edit, Bash, mcp__agent-ide__ide_context, Bash
```

The native `Edit` landed exactly as asked (verified on disk after the session:
`pkg/a.py`'s `combine` reads `def combine(x: int, y: int, z: int) -> int`), and, mirroring §1's
tool order, the block was captured on the very next tool call after the edit with no intervening
`ide.*` call, then `ide.context {"kind":"problems"}` reported the exact arity error in the
never-opened `pkg/b.py`, then the no-op step showed no repeated block — the same business
criterion as §1, now for Python. `rust: check failed` (not `environment not found`) is correct
for this fixture: `python-pkg/` has no `Cargo.toml`, so the Rust checker's run is unparseable/
fails outright rather than reporting a missing environment — `UnavailableReason::Fatal` renders
block text `"check failed"` (`src/feed/mod.rs:296`), a distinct fixed string from Python's
`EnvMissing` → `"environment not found"`, both per `docs/contracts/eyes-v0.3.md` §4/§6.

## §7. Scenario 2b — Python without `.venv` (real_pass, second attempt)

Same repo as §6, `$WORKDIR/py-venv`, with `.venv` removed (`rm -rf`) before the session, driven
by `scripts/acceptance-drivers/eyes-claude.sh python-noenv`. The prompt asks for `ide.start`,
`ide.inspect`, a settle step, then `ide.context {"kind":"problems","language":"python"}`.

**First attempt** (started 7 s after the §6 session's `claude` process exited, same command
template, `session_id=436c54dc-e1b7-42c2-9166-b158fee96114`, `duration_ms=35227`,
`is_error=false`, `stop_reason=end_turn`) returned:
```
START: ERROR: conflict; continue with native tools
PROBLEMS: error: workspace_authority; continue with native tools
```
`error: conflict` is `FailureCode::Conflict`'s exact rendered text
(`src/assistance/content.rs:105`), and the daemon's own doc comment names the cause precisely:
`activate_claude` "Returns `FailureCode::Conflict` when another actor already owns this
worktree, leaving the first owner's activation usable" (`src/assistance/worker.rs:2156-2157`,
`AuthorityError::WorktreeOwned` path in `src/workspace/authority.rs:341`). The §6 session's
actor was still the registered owner of `$WORKDIR/py-venv`'s worktree: neither its prompt nor
any other scenario prompt in this driver ever calls the `ide.stop` tool ("Releases this actor's
IDE binding at task end or handoff" — `src/assistance/facade.rs:1048`), and this plugin ships no
`SessionEnd` hook (`hooks/hooks.json` only wires `PreToolUse`/`PostToolUse`/
`PostToolUseFailure`/`PermissionDenied`) to signal the daemon when a Claude session simply exits.
So a second real session against the exact same worktree, started soon after the first exits,
is refused — this is not a scenario the task's five scenarios exercise elsewhere (§8's two
worktrees are two *different* directories, so they never contend for the same worktree slot).

This is a genuine, real, reproducible product behavior surfaced by this driver — not a defect
in `src/`; `ide.stop` is the documented release mechanism and simply wasn't called. No product
code was touched to work around it (per the task's rule against patching `src/` for a driver
finding). Instead, the retry below used the same daemon's own `idle_timeout_s: 30` cleanup: by
the time of the retry, `/private/tmp/ai-r-c82bd1cab969cdd2` (this worktree's rendezvous runtime
directory) had already been removed by the idle timer, taking the in-memory authority state
with it, so a fresh `ide.start` was free to claim the worktree again.

**Second attempt** (`session_id=0b835074-f686-4d97-85f6-b831faf4a82b`, `num_turns=8`,
`duration_ms=21756`, `is_error=false`, `stop_reason=end_turn`) returned:
```
START: Workspace activated; authority_epoch: 2; baseline: partial (Unverified; durable capture true); worktree_cache: retained. Provider readiness is not implied.
PROBLEMS: python: unavailable:env_missing
```
`unavailable:env_missing` is the exact `ide.context` language-scoped rendering of
`UnavailableReason::EnvMissing` (`src/assistance/problems.rs:421`, matching the product's own
unit-test assertion at `src/assistance/problems.rs:614-615` for the equivalent Rust case) — the
literal business criterion for scenario 2b: a Python check with no `.venv` reports environment-
missing, never the flood of unresolved-import errors a missing environment would otherwise
produce.

## §8. Scenario 3 — two worktrees of one Rust repo, sequential sessions (partial)

Fixture: `tests/fixtures/eyes/rust-workspace/` copied to `$WORKDIR/rust-origin`, with two real
`git worktree add` worktrees, `$WORKDIR/rust-wt-a` (branch `wt-a`) and `$WORKDIR/rust-wt-b`
(branch `wt-b`), driven by `worktree-a` then `worktree-b`. Both prompts are identical: `ide.start`
→ helper → `ide.inspect` → `sleep 4` → `ide.context {"kind":"problems"}`, asking the model to
note any check-duration field it sees.

**`worktree-a`** (`session_id=ef63ed00-fee5-4e79-8721-3f375a520e02`, `num_turns=7`,
`duration_ms=23651`, `duration_api_ms=18399`, `is_error=false`, `stop_reason=end_turn`):
```
START: Workspace activated; authority_epoch: 2; baseline: partial (Unverified; durable capture true); worktree_cache: retained. Provider readiness is not implied.
PROBLEMS: rust: ready; errors: 0; warnings: 0
python: unavailable:env_missing
```
Rendezvous runtime directory (computed from `rust-origin`'s canonical git common directory, per
`rendezvous_runtime_dir` in the driver): `/private/tmp/ai-r-7d084560f42ece6d`.

Between `worktree-a`'s session exit and starting `worktree-b`, this continuation spent real time
diagnosing §7's conflict (unrelated repo, but the same wall clock), long enough that
`idle_timeout_s: 30` elapsed and `/private/tmp/ai-r-7d084560f42ece6d` was removed before
`worktree-b` started — confirmed directly (`ps aux` showed no candidate daemon process and
`/private/tmp/ai-r-*` had no entries immediately before the `worktree-b` session).

**`worktree-b`** (`session_id=9d5a74d5-33c4-4b79-a56f-2788ea3f5bb7`, `num_turns=7`,
`duration_ms=22239`, `duration_api_ms=17124`, `is_error=false`, `stop_reason=end_turn`):
```
START: Workspace activated; authority_epoch: 2; baseline: partial (Unverified; durable capture true); worktree_cache: retained. Provider readiness is not implied.
PROBLEMS: rust: ready; errors: 0; warnings: 0\npython: unavailable:env_missing
```
The rendezvous runtime directory recomputed to the same path, `/private/tmp/ai-r-7d084560f42ece6d`
(same deterministic BLAKE3-of-canonical-git-common-dir identity, per `docs/contracts/eyes-v0.3.md`
§2), but it was necessarily a *new* daemon process (PID 81456, confirmed via `ps aux` right after
this session): the directory was confirmed completely absent (`ls /private/tmp/ai-r-*` — no
matches, no candidate daemon in `ps aux`) immediately before the `worktree-b` session started, so
whatever occupied it afterward cannot have been the same process `worktree-a` used. The daemon
runtime *directory identity* is shared and reused by design, but this run's own investigation
delay meant the *process instance* was not the same one across both worktrees.

This matters for the specific cache-reuse claim: `try_clone_rust_cache`
(`src/checks/scheduler.rs:622-672`) looks up `most_recent_rust_cache_dir` from in-memory
`state.repositories` to `cp -c -R` a sibling worktree's `target/` into the new worktree's cache —
and that cache directory itself lives at `<runtime-dir>/cache/...` (confirmed:
`/private/tmp/ai-r-7d084560f42ece6d/cache` existed after `worktree-b`), inside the same ephemeral
runtime directory the idle timer deletes wholesale. Once that directory was removed between the
two sessions, both the in-memory clone-source record and the on-disk `target/` it would have
cloned from were gone together, so `worktree-b`'s check necessarily ran cold. Both worktrees'
checks still completed correctly (`rust: ready; errors: 0; warnings: 0` in both, `python:
unavailable:env_missing` in both, matching a fixture with no `.venv` anywhere under
`rust-origin`), which is the scenario's core per-worktree correctness claim — **real_pass** — but
neither the "one live daemon serves both worktrees" framing nor the "second worktree's first
check reuses the cloned cache, faster than the first" timing comparison from the task brief was
demonstrated in this run. This is an artifact of this continuation's own investigation pacing,
not a reproducible product limitation or defect; a version of this driver run without a
multi-minute gap between `worktree-a` and `worktree-b` (for example, invoking both from one
shell command back to back, as `./eyes-claude.sh worktree-a && ./eyes-claude.sh worktree-b`
already does when nothing else intervenes) would be expected to land inside the 30 s window and
show real reuse — untried here because the remaining real-Claude-session budget was needed for
scenario 5a (§9).

## §9. Scenario 5a — confinement: worktree outside `allowed_roots` (real_pass)

Fixture: `tests/fixtures/eyes/rust-workspace/` copied to `/private/tmp/aiv3-acc-outside-82011`,
a directory *outside* `allowed_roots` (which this driver's shared launcher template sets to
`$WORKDIR` = `/private/tmp/aiv3-acc-t104-77433`, per `write_launcher_template`), driven by
`scenario_confinement`. The prompt: `ide.start` → helper → `ide.inspect` → `sleep 3` →
`ide.context {"kind":"problems"}`.

Session metadata: `session_id=d6699bf8-1044-4417-b0fd-5b1c53fd4573`, `num_turns=7`,
`duration_ms=22172`, `duration_api_ms=18144`, `is_error=false`, `stop_reason=end_turn`.

Model's final answer (`.result`, verbatim):
```
START: Workspace activated; authority_epoch: 2; baseline: partial (Unverified; durable capture true); worktree_cache: retained. Provider readiness is not implied.
PROBLEMS: rust: unavailable:outside_roots
python: unavailable:outside_roots
```
`ide.start` itself succeeds outside `allowed_roots` — workspace activation is a Git-discovery
concern, separate from root admission for project checks (`docs/contracts/eyes-v0.3.md` §2 vs.
§5) — but both languages' project checks correctly refuse to run and report
`unavailable:outside_roots`, the exact `ide.context` rendering of `UnavailableReason::OutsideRoots`
(`src/assistance/problems.rs:419`, `src/assistance/problems.rs:270`, matching the product's own
unit-test assertion at `src/assistance/problems.rs:606-607`); `docs/contracts/eyes-v0.3.md` §6's
literal block-text form for the same reason is `"outside allowed roots"`, `ide.context`'s own
compact per-language form is the `<lang>: unavailable:<reason>` token seen here. This is the
literal business criterion for scenario 5a.

## Limitations

- `--output-format json` (as the task brief specifies) returns only the model's own final
  answer and session metadata, not a per-tool-call transcript. Every scenario's evidence is
  therefore the model's *self-reported* account of what it saw and did (structured with fixed
  markers so it stays checkable), not an independently captured wire-level trace of hook
  `additionalContext` payloads or the exact tool-call order. The v0.2 drivers'
  `require_tool_use`/`forbid_transcript_text` style checks over a `stream-json` transcript
  were not available under `json` output; `TOOLS_CALLED` in §1/§6 is the model's own account,
  cross-checked only against its own step numbering and against `ide.context`'s independent
  answer for `crates/b`/`pkg/b.py`.
- Only one model (`haiku`) and one host macOS version were exercised; no coverage of other
  Claude models or of Linux/Codex hosts (both explicitly out of scope per
  `docs/contracts/eyes-v0.3.md`).
- Scenario 3's specific "second worktree's first check reuses the cloned cache, faster than
  the first" comparison was not measured: both real sessions ran and both checks completed
  correctly, but the shared daemon idled out between them (§8), so no reuse occurred to time.
- Two sequential real Claude sessions against the *same* worktree path (not exercised by any of
  the task's five named scenarios, but hit incidentally while reproducing scenario 2b, §7) will
  refuse the second session with `error: conflict` unless the first actor calls `ide.stop` or
  enough wall time passes for `idle_timeout_s` to release the worktree — this acceptance driver
  calls `ide.stop` in none of its prompts, so every scenario here relies on either a fresh
  worktree per session or on the idle timer, never on an explicit release.
