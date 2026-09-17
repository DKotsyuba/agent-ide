# Project problem feed v0.3 contract (EYES-r1)

This contract defines the v0.3 MVP: a shared per-repository Agent IDE service that runs confined
background project checks for Rust and Python and gives the active agent a compact problem-count
block. macOS and Claude Code are the supported target; Codex keeps working through the existing
v0.2 path without the active block. Linux is `not_tested`. Evidence for the design choices is in
`docs/evidence/v03-*-probe.md` on the `probe/v03-*` branches.

All v0.2 contracts (TELEMETRY-r1, EDIT-r1, TYPESCRIPT-r3, AGENT-CONTENT-r1) stay valid. Every
failure here is fail-open: a missing service, check, toolchain or feed never blocks native tools,
`ide.*` calls or turn completion.

## 1. Configuration

The launcher configuration (`src/assistance/launcher.rs`, `deny_unknown_fields`) gains two optional
top-level fields:

```json
{
  "allowed_roots": ["/absolute/normalized/path"],
  "project_checks": {
    "debounce_ms": 1500,
    "idle_timeout_s": 300,
    "check_timeout_s": 300,
    "rust": { "toolchain_dir": "/abs/.rustup/toolchains/<name>" },
    "python": { "node": "/abs/node", "pyright_cli": "/abs/pyright" }
  }
}
```

- `allowed_roots`: 0..=16 absolute, normalized paths (no `..`, no trailing `/`). The product default
  is an empty list. Machine-specific values (for example the owner's `/Users/pluto/projects`) live
  only in the operator's configuration, never in code.
- A worktree is admitted when its canonical path (`std::fs::canonicalize`) is equal to or below a
  canonicalized allowed root. Otherwise every language state is `unavailable(outside_roots)`.
- `project_checks` absent, or `allowed_roots` empty: project checks are disabled and no block is
  emitted. v0.2 behaviour is unchanged.
- `debounce_ms` 100..=10000, `idle_timeout_s` 30..=3600, `check_timeout_s` 10..=900.
- A language subsection absent: that language is not checked and never appears in the block.

Error example: `"allowed_roots": ["relative/path"]` rejects the whole configuration at parse time
with the existing launcher configuration error.

## 2. Shared service lifecycle

- Rendezvous: the daemon runtime directory is keyed by the repository, not by one MCP process:
  `/private/tmp/ai-r-<16 lowercase hex of blake3(canonical git common dir)>`, created 0700 with
  the existing dev/inode fencing. All worktrees of one repository share one daemon.
- The first MCP server that finds no live daemon (lock not held) spawns it in its own session
  (`setsid`) and does not own its lifetime. Later MCP servers adopt the live daemon. An MCP server
  exiting never terminates or removes a daemon it adopted or spawned.
- Lease: each MCP server keeps one long-lived `ClientLease` connection open for its lifetime. The
  daemon counts open lease connections. EOF on a lease connection releases it immediately.
- Idle: when the lease count is 0 and no check is running, the daemon starts `idle_timeout_s`.
  A new lease cancels the timer. On expiry the daemon cancels checks, kills their process groups,
  closes the socket and removes its runtime directory.
- The existing per-call IPC (hook submit and the five assistance methods) is unchanged.

Success example: agents A and B in two worktrees of one repository call `ide.start`; one daemon
serves both; A exits; B keeps working; B exits; the daemon stops 300 s later.
Error example: the daemon crashes; the next MCP call finds no lock holder, spawns a new daemon and
returns the normal typed unavailable/pending outcome for that call; native work is unaffected.

## 3. Confined check execution

Every project check process is started through one spawn path:

- `sandbox-exec -f <generated profile>`; profile: `(deny default)`, `(deny network*)`,
  process fork/exec, `sysctl-read`, `file-read-data (literal "/")`, `file-read-metadata (subpath
  "/")` (required on macOS 27, otherwise children abort with SIGABRT), read of system paths,
  **read-only** of the admitted worktree root, read of the configured toolchain directories and
  `/private/etc`, read+write of the check's private cache directory and a private temp directory.
- Own process group; on cancel, timeout (`check_timeout_s`) or daemon shutdown the whole group is
  killed (SIGTERM, 2 s, SIGKILL) and reaped. No pattern-based kill of processes the daemon did not
  start.
- Environment is rebuilt from an allowlist (PATH to toolchain bins, HOME, TMPDIR to the private
  temp dir, CARGO_TARGET_DIR, CARGO_NET_OFFLINE=true); ambient credentials are not passed.

Error example: a build script tries to read `~/.ssh/id_ed25519` or open a TCP connection; the
access is denied by the OS profile; the check reports whatever cargo reports.

## 4. Problem snapshot

```rust
pub enum Language { Rust, Python }
pub enum UnavailableReason { Disabled, OutsideRoots, ToolMissing, EnvMissing, Fatal, Timeout }
pub enum CheckState {
    Ready,          // complete result for the configured scope
    Partial,        // result present but coverage incomplete
    Checking,       // first result not yet available
    Unavailable(UnavailableReason),
}
pub struct Problem { pub path: String, pub line: u32, pub column: u32,
                     pub severity: Severity /* Error | Warning */, pub code: Option<String>,
                     pub message: String /* <= 200 chars, untrusted data */ }
pub struct ProblemSnapshot { pub language: Language, pub state: CheckState,
                             pub errors: u32, pub warnings: u32,
                             pub problems: Vec<Problem> /* <= 500, sorted: errors first, path, line */,
                             pub truncated: bool, pub input_generation: u64,
                             pub duration_ms: u64 }
```

`errors`/`warnings` count all deduplicated problems even when `problems` is truncated.
`Unavailable` and `Checking` carry zero counts that are never rendered as numbers.

### Rust (`cargo check --workspace --all-targets --message-format=json --offline --keep-going`)

- Count `reason: "compiler-message"` with `message.level` `error` or `warning`; ignore child notes
  and the summary messages (`aborting due to …`, `… warning(s) emitted`, `could not compile`).
- Deduplicate by `(level, code, primary span file_name:line_start:column_start, message)`:
  `--all-targets` repeats a diagnostic for lib and lib-test units.
- `Ready` requires the final `build-finished` message. If any compilation unit failed so dependent
  units produced no result, state is `Partial`. Missing `build-finished` → `Unavailable(Fatal)`.
- `CARGO_TARGET_DIR` is the check's private cache dir for that worktree.
- Missing `cargo` in `toolchain_dir` → `Unavailable(ToolMissing)`.

### Python (`pyright --outputjson --project <config or worktree>`)

- Exit 0 or 1 with parseable JSON: counts from `generalDiagnostics` severities `error`/`warning`;
  must equal `summary.errorCount`/`summary.warningCount`; `summary.filesAnalyzed == 0` →
  `Unavailable(EnvMissing)`. Exit ≥ 2 or unparseable output → `Unavailable(Fatal)`.
- Environment: use the project's own interpreter. Order: `venvPath`/`venv` from
  `pyrightconfig.json` or `[tool.pyright]` in `pyproject.toml`; else `<worktree>/.venv/bin/python`;
  pass it as `--pythonpath`. None found → `Unavailable(EnvMissing)` (never report the flood of
  unresolved-import errors a missing environment produces).
- Project code is never executed.

## 5. Scheduling

- Triggers: successful `ide.start` (initial warm check), a native post-hook for a may-write tool
  (Claude `Edit`, `Write`, `MultiEdit`, `NotebookEdit`, `Bash`), and a completed `ide.edit`.
- Per `(worktree, language)`: debounce `debounce_ms` after the last trigger; at most one running
  check; a trigger during a run marks it dirty and one more run follows (latest wins). At most two
  checks run concurrently across the daemon.
- A new worktree's Rust cache directory, when absent, is first cloned copy-on-write
  (`clonefile`/`cp -c -R`) from the most recently completed sibling worktree of the same repository;
  clone failure falls back to a cold check.
- The latest completed snapshot per `(worktree, language)` is kept in memory; a running check never
  replaces it until it completes.

## 6. The `<agent-ide>` block

Rendered from the latest completed snapshots of the actor's worktree, in fixed language order
(rust, python), only for configured languages:

```
<agent-ide>
rust: 3 errors (+2), 5 warnings | python: environment not found
</agent-ide>
```

- Item forms: `<lang>: <E> errors, <W> warnings`; append ` (partial)` for `Partial`; the delta
  `(+N)`/`(-N)` after a count appears only when that count changed since the last block delivered
  to this actor for this worktree. `Unavailable` renders fixed text: `checks disabled`,
  `outside allowed roots`, `tool not found`, `environment not found`, `check failed`,
  `check timed out`. A language still `Checking` without any completed snapshot is omitted.
- Total size including tags ≤ 256 UTF-8 bytes; no paths, messages or code.
- Emission rule: the block is emitted only when the item set without deltas differs from the last
  block delivered to that `(actor binding, worktree)`. The first completed result is emitted once.
  Identical state is never re-emitted.
- Delivery: Claude `PostToolUse` / `PostToolUseFailure` hook `additionalContext`, taken from the
  ready cache inside the existing hook deadline. The hook never waits for a check. Marking as
  delivered happens when the hook response is produced (at most once; a lost hook response is not
  retried).
- Codex and other hosts: no active block in v0.3.

## 7. `ide.context` with `kind: "problems"`

Parameters: `{"kind": "problems", "language": "rust" | "python" (optional), "offset": u32 (optional)}`.
`path` is not required for this kind; any other `kind` value or absent `kind` keeps v0.2 behaviour.
Reply: per language the state, counts, and up to 20 problems from `offset`, each
`path:line:column severity [code] message`, plus `next_offset` when more exist. Messages are
untrusted text. No new tool and no new `AssistanceMethod`.

## 8. Telemetry

One new event `ProjectCheckCompleted { language, state, duration_ms, errors_bucket, warnings_bucket }`
with counts bucketed (`0`, `1-9`, `10-99`, `100+`), under TELEMETRY-r1 privacy rules (no paths or
messages).
