# Project problem feed v0.3 contract (EYES-r2)

Revision r2 (2026-09-17) applies the architecture review of r1: lease admission, cache location,
daemon adoption, trigger source, problems-kind dispatch, `--locked`, Python interpreter rules,
snapshot retention, feedback concatenation, unbound actors, telemetry scope, cooldown, and the
Claude-only scope of the shared rendezvous. Sections are otherwise unchanged.

This contract defines the v0.3 MVP: a shared per-repository Agent IDE service that runs confined
background project checks for Rust and Python and gives the active agent a compact problem-count
block. macOS and Claude Code are the supported target; Codex keeps working through the existing
v0.2 path without the active block. Linux is `not_tested`. Evidence for the design choices is in
`docs/evidence/v03-*-probe.md` on the `probe/v03-*` branches.

All v0.2 contracts (TELEMETRY-r1, EDIT-r1, TYPESCRIPT-r3, AGENT-CONTENT-r1) stay valid, with two
explicit amendments: §8 extends the TELEMETRY-r1 event scope by one event tag, and §2 supersedes
the sentence in `docs/assistance-host-binding.md` that a second MCP never adopts an existing
runtime directory. Every failure here is fail-open: a missing service, check, toolchain or feed
never blocks native tools, `ide.*` calls or turn completion.

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
    "rust": { "toolchain_dir": "/abs/.rustup/toolchains/<name>", "cargo_home": "/abs/.cargo",
              "developer_dir": "/abs/Xcode.app/Contents/Developer" },
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
- `rust.cargo_home` is optional and defaults to `$HOME/.cargo`; the parent of `toolchain_dir`'s
  `toolchains` directory (normally `$HOME/.rustup`) is derived, not configured.
- `rust.developer_dir` is optional (T05B) and overrides the Apple developer directory read root a
  native build script's `cc`/`xcrun` invocation needs; absent, it is resolved once per checker from
  `/usr/bin/xcode-select -p`, falling back to `/Applications/Xcode.app/Contents/Developer` then
  `/Library/Developer/CommandLineTools`. `/private/var/db/xcode_select_link` and
  `/Library/Developer/CommandLineTools` are always added as extra read roots when they exist,
  because `/usr/bin/cc` resolves through that symlink database independently of the reported
  developer directory. Only existing paths are ever added.
- Check caches live outside any runtime directory, under
  `$HOME/.agent-ide/checks/<16 hex blake3(repository key)>/<16 hex blake3(canonical worktree)>/<language>`
  (0700), so they survive daemon idle stops and crashes. At daemon start, cache directories whose
  recorded worktree path no longer exists are removed.

Error example: `"allowed_roots": ["relative/path"]` rejects the whole configuration at parse time
with the existing launcher configuration error.

## 2. Shared service lifecycle

- Rendezvous (Claude managed mode; Codex keeps its operator `--runtime-dir`): the daemon runtime
  directory is keyed by the repository, not by one MCP process:
  `/private/tmp/ai-r-<16 lowercase hex of blake3(repository key)>` where the repository key is the
  canonical git common dir of the project (fallback: the canonical project dir), created 0700 with
  the existing dev/inode fencing. The MCP server and the hook derive the key with the same
  function. All Claude worktrees of one repository share one daemon.
- The first MCP server that finds no live daemon (lock not held) spawns it in its own session
  (`setsid`) and does not own its lifetime. Later MCP servers adopt the live daemon after the
  adoption check: directory owned by the effective uid, mode 0700, not a symlink, dev/inode
  re-validated after open, lock held, socket answering. This supersedes the v0.2 rule that an
  existing runtime path is never adopted; the operator `allowUnixSockets` entry moves to
  `/private/tmp/ai-r-<hash>/claude-helper.sock`. An MCP server exiting never terminates or removes
  a daemon it adopted or spawned.
- Lease: each MCP server keeps one long-lived `ClientLease` connection open for its lifetime. Lease
  connections are exempt from `ipc.connection_deadline` and are admitted from their own bounded
  pool (32) that never consumes hook/assistance connection capacity. The daemon counts open lease
  connections. EOF on a lease connection releases it immediately.
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
  **read-only** of the admitted worktree root, read of the configured toolchain directories
  (Rust: `toolchain_dir`, `cargo_home`, the derived rustup home, the resolved or configured Apple
  developer directory and its `xcode_select_link`/`CommandLineTools` companions (T05B); Python:
  the node install root, the pyright package root, the venv root and the canonical base
  interpreter prefix) and `/private/etc`, read+write of the check's private cache directory and a
  private temp directory.
- Own process group; on cancel, timeout (`check_timeout_s`) or daemon shutdown the whole group is
  killed (SIGTERM, 2 s, SIGKILL) and reaped. No pattern-based kill of processes the daemon did not
  start.
- Environment is rebuilt from an allowlist (PATH to toolchain bins, HOME, TMPDIR to the private
  temp dir, CARGO_TARGET_DIR, CARGO_NET_OFFLINE=true, plus Rust's linker-bypass variables below);
  ambient credentials are not passed.
- Rust linker bypass (T06B): `/usr/bin/cc`, which every native build script's link step reaches by
  default, is Apple's `xcrun` shim — it writes an `xcrun_db` cache into the real Darwin user temp
  directory via `confstr(_CS_DARWIN_USER_TEMP_DIR)` (ignoring `TMPDIR`) and dyld-loads Xcode
  frameworks outside `Contents/Developer`, both denied by this profile, so every build script fails
  linking with exit status 71 even though compilation itself succeeds. When a toolchain `clang` is
  found under the resolved Apple developer directory (`<dir>/Toolchains/XcodeDefault.xctoolchain/
  usr/bin/clang` for Xcode, `<dir>/usr/bin/clang` for the Command Line Tools), the check environment
  points the link step straight at it instead: `CC=<clang>`, `CXX=<clang>++` when that sibling
  exists, `SDKROOT=<dir>/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk` (Xcode) or
  `<dir>/SDKs/MacOSX.sdk` (Command Line Tools) when it exists, and either
  `CARGO_TARGET_<TRIPLE>_LINKER=<clang>` when the toolchain directory's own name (for example
  `1.98.1-aarch64-apple-darwin`) yields a target triple, or `RUSTFLAGS=-Clinker=<clang>` otherwise.
  No clang found: the environment is unchanged and the build fails exactly as it would unconfined.

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
                             pub duration_ms: u64,
                             pub detail: Option<String> /* T05B: bounded cause, e.g. a build
                                                            failure's first `error:` line */ }
```

`errors`/`warnings` count all deduplicated problems even when `problems` is truncated.
`Unavailable` and `Checking` carry zero counts that are never rendered as numbers.

### Rust (`cargo check --workspace --all-targets --message-format=json --offline --locked --keep-going`)

- `--locked` is always passed: the worktree is read-only for the check, so a lockfile that needs
  updating cannot be written; cargo's "lock file needs to be updated" failure maps to
  `Unavailable(EnvMissing)`.

- Count `reason: "compiler-message"` with `message.level` `error` or `warning`; ignore child notes
  and the summary messages (`aborting due to …`, `… warning(s) emitted`, `could not compile`).
- Deduplicate by `(level, code, primary span file_name:line_start:column_start, message)`:
  `--all-targets` repeats a diagnostic for lib and lib-test units.
- `Ready` requires the final `build-finished` message. If any compilation unit failed so dependent
  units produced no result, state is `Partial`. Missing `build-finished` → `Unavailable(Fatal)`.
- `build-finished.success: false` with zero deduplicated errors (T05B: a build failure with no
  diagnostic to show, for example a build-script link failure — T06B: `cc` exiting nonzero has no
  primary span, so it is never counted) is also `Unavailable(Fatal)`, never `Ready`: counts are
  never fabricated for a run that did not actually compile the workspace. The snapshot's `detail`
  prefers the `message.message` of the first `error`-level `compiler-message`, even without a
  primary span (T06B), over cargo's own summary; only when no such message exists does the first
  `error:` line of cargo's stderr stand in (both trimmed to 160 bytes).
- `CARGO_TARGET_DIR` is the check's private cache dir for that worktree.
- Missing `cargo` in `toolchain_dir` → `Unavailable(ToolMissing)`.

### Python (`pyright --outputjson --project <config or worktree>`)

- Exit 0 or 1 with parseable JSON: counts from `generalDiagnostics` severities `error`/`warning`;
  must equal `summary.errorCount`/`summary.warningCount`; `summary.filesAnalyzed == 0` →
  `Unavailable(EnvMissing)`. Exit ≥ 2 or unparseable output → `Unavailable(Fatal)`.
- Environment: use the project's own interpreter. Order: `<venvPath>/<venv>/bin/python` from
  `pyrightconfig.json` or `[tool.pyright]` in `pyproject.toml`; else `<worktree>/.venv/bin/python`.
  The interpreter file must exist; its canonical path (symlinks resolved) is passed as
  `--pythonpath`, and the canonical interpreter's installation prefix is added to the check's read
  set. None found → `Unavailable(EnvMissing)` (never report the flood of unresolved-import errors a
  missing environment produces).
- Project source is never executed. Pyright does start the interpreter to enumerate search paths,
  so interpreter start-up hooks (`sitecustomize`, `.pth`) run under the same confinement.

## 5. Scheduling

- Triggers: successful `ide.start` (initial warm check), a Claude `PostToolUse`/`PostToolUseFailure`
  hook event whose retained `tool_name` is one of `Edit`, `Write`, `MultiEdit`, `NotebookEdit`,
  `Bash` (the hook parser retains `tool_name` for post phases only; tool input and output stay
  discarded), and a completed `ide.edit`. An actor whose `ide.start` was refused (for example
  `conflict` because another actor owns the worktree) has no binding: it triggers no check and
  receives no block.
- Per `(worktree, language)`: debounce `debounce_ms` after the last trigger; at most one running
  check; a trigger during a run marks it dirty and one more run follows (latest wins). The next run
  for a pair starts no earlier than `max(debounce_ms, previous run duration)` after the previous
  completion. At most two checks run concurrently across the daemon.
- A new worktree's Rust cache directory, when absent, is first cloned copy-on-write
  (`clonefile`/`cp -c -R`) from the most recently completed sibling worktree of the same repository;
  clone failure falls back to a cold check.
- The latest completed snapshot per `(worktree, language)` is kept in memory; a running check never
  replaces it until it completes. A completed `Unavailable(Fatal)` or `Unavailable(Timeout)` never
  replaces an existing `Ready`/`Partial` snapshot (the previous counts stay visible); other
  unavailable reasons replace it.

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
  retried). When the v0.2 one-shot native feedback is eligible in the same hook response, the block
  is prepended and both are concatenated once within `MAX_FEEDBACK_BYTES`; each is marked delivered
  independently.
- Codex and other hosts: no active block in v0.3.

## 7. `ide.context` with `kind: "problems"`

Parameters: `{"kind": "problems", "language": "rust" | "python" (optional), "offset": u32 (optional)}`.
`path` is not required for this kind; any other `kind` value or absent `kind` keeps v0.2 behaviour.
Reply: per language the state, counts, and up to 20 problems from `offset`, each
`path:line:column severity [code] message`, plus `next_offset` when more exist. Messages are
untrusted text. An `unavailable:<reason>` state line carrying a [`ProblemSnapshot::detail`] (T05B)
appends it in parentheses, for example `rust: unavailable:fatal (error: failed to run custom
build command for \`blake3 v1.5.0\`)`; a snapshot with no detail renders exactly as before. This
detail is never included in the `<agent-ide>` block (§6), which stays within its 256-byte cap. No
new tool and no new `AssistanceMethod`. The problems kind is answered from the daemon's in-memory
snapshots for the caller's bound worktree on every host; it is never dispatched to the Claude
foreground helper and needs no provider.

## 8. Telemetry

One new event `ProjectCheckCompleted { language, state, duration_ms, errors_bucket, warnings_bucket }`
with counts bucketed (`0`, `1-9`, `10-99`, `100+`), under TELEMETRY-r1 privacy rules (no paths or
messages). This extends the TELEMETRY-r1 event scope (six MCP methods and native fallback) by one
tag, which is added to the closed telemetry query filter.
