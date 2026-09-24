# Project problem feed v0.3 contract (EYES-r2)

Revision r2 (2026-09-17) applies the architecture review of r1: lease admission, cache location,
daemon adoption, trigger source, problems-kind dispatch, `--locked`, Python interpreter rules,
snapshot retention, feedback concatenation, unbound actors, telemetry scope, cooldown, and the
Claude-only scope of the shared rendezvous. Sections are otherwise unchanged.

This contract defines the v0.3 MVP: a shared per-repository Agent IDE service that runs confined
background project checks for Rust, Python, and root-config TypeScript/JavaScript and gives the
active agent a compact problem-count block. macOS is the supported platform and both supported
hosts receive the block: Claude Code in
hook context, Codex (and any future host without hook delivery) at the top of its terminal `ide.*`
replies (T28B). There is no split in the data itself — every host sees the same plate, emitted
under the same rules; only the carrier differs. Linux is `not_tested`. Evidence for the design
choices is in `docs/evidence/v03-*-probe.md` on the `probe/v03-*` branches.

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
    "python": { "node": "/abs/node", "pyright_cli": "/abs/pyright" },
    "typescript": { "node": "/abs/node-v24.4.0/bin/node", "tsc_cli": "/abs/typescript-5.9.3/lib/tsc.js" }
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
  connections. A managed Codex client's connect, request, and acknowledgement must finish within
  three seconds; the held-open lease has no connection deadline. EOF releases a lease immediately.
  A managed Claude MCP registers
  its own canonical candidate on that lease and receives a separate opaque attachment, including
  for the daemon's first worktree. The foreground helper and Workspace still
  validate Git identity and authority before activation. Missing candidates are removed from the
  daemon's additional target map; they cannot redirect another worktree's attachment.
- Idle: when the lease count is 0 and no check is running, the daemon starts `idle_timeout_s`.
  A new lease cancels the timer. On expiry the daemon cancels checks, kills their process groups,
  closes the socket and removes its runtime directory.
- A managed Codex MCP owns a private daemon and holds one `ClientLease` for its entire lifetime,
  so the daemon does not idle out while the session is open. If transport is lost after a daemon
  exit or crash, the next call starts one replacement owned daemon, opens a new lease, republishes
  its known native-hook actor routes against the new runtime and retries the call once. A failed
  restart reports daemon re-establishment failure; missing host metadata is reported separately.
  A transport timeout while the exact owned child and runtime remain live does not replace them.
  Replacement registers its private runtime before async startup, and MCP exit waits for any
  replacement before retiring routes, terminating the child, and removing that runtime.
- The existing per-call IPC (hook submit and the five assistance methods) is unchanged.
- Client-side re-establishment: a Claude MCP server never caches a dead connection. Each tool call
  dispatches against the live `runtime_dir`/attachment pair it currently holds; if that dispatch
  comes back transport-unavailable (the daemon exited — idle timeout, `SIGTERM`, a crash, or a
  binary upgrade — while this MCP process kept running), the server re-runs the exact same
  rendezvous it started with (adopt-or-spawn under the runtime-dir lock, so several MCP servers
  racing to relaunch still end up with one daemon), stores the refreshed pair for itself and every
  later call, and retries that one call once. On a successful rendezvous it also opens a fresh
  `ClientLease` against the re-established daemon and replaces its own held-open one, so the new
  generation still counts this MCP's lease and its idle countdown keeps only ever running while zero
  managed Claude MCPs are attached, exactly as at startup. No retry loop and no background polling: a
  rendezvous failure, or a retry that is still unavailable, surfaces the normal unavailable outcome
  for that call, and this MCP's lease stays on the dead connection until it is dropped. A pending
  peer-side binding from the dead daemon is gone; the next `ide.start` establishes a fresh one.
- Retry hint (T08B): the retried dispatch above reaches the freshly re-established daemon, which has
  no pre-hook observation for the call whose own hook fired before that daemon existed, so it reports
  the ordinary `{"state":"unavailable","reason":"host_binding"}` outcome even though the daemon itself
  is back. Only for that specific retried dispatch, the reply adds a stable
  `"retry":"daemon restarted; repeat this call once"` fact for `ide.start`. For another method,
  the hint tells the agent to call `ide.start` first and then repeat with fresh references: the
  replacement has no actor binding, so repeating Context directly cannot recover. The hint is a
  `structuredContent` field and compact text on Codex, and compact text alone on Claude
  (agent-content-v0.2 §"Host-specific projection", T14B). Every other reply,
  including a still-unavailable retry, is unchanged.

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
  interpreter prefix; TypeScript: the Node executable and TypeScript package). `/private/etc` is
  included in the fixed system grants. The check's private cache and temp directories are writable.
- Rust ancestor manifest reads (T07B): before resolving a workspace, cargo walks every ancestor
  of the worktree looking for a `[workspace]` root, reading each ancestor's `Cargo.toml` and
  `.cargo/config.toml`/`.cargo/config` even when that ancestor is not a workspace root — the
  owner's live case is a worktree nested inside another checkout, for example a Claude Desktop
  worktree at `<repo>/.claude/worktrees/<name>`. The profile therefore admits read of exactly
  those files (never the ancestor directories) for every ancestor of the canonical worktree path
  up to `/`; a bare ancestor `Cargo.toml` that does not list the worktree as a workspace member
  leaves cargo treating the worktree as its own workspace root, same as if the ancestor did not
  exist, while an ancestor `[workspace]` that also omits the worktree is a genuine project
  misconfiguration cargo rejects outright — both surface through the run's own stdout/stderr, not
  through profile denial. Without this, the walk fails with `Operation not permitted` before
  cargo produces a single JSON event, reported as `Unavailable(Fatal)` with cargo's own
  `error: failed searching for potential workspace` as the snapshot detail (§4).
- Cargo's build-script fingerprint scan reads the standard Git excludes file at
  `$HOME/.config/git/ignore` when it exists. The Rust profile admits that exact file as a read
  root; denying it makes Cargo abort before any compiler diagnostic or `build-finished` event.
- Own process group; on cancel, timeout (`check_timeout_s`) or daemon shutdown the whole group is
  killed (SIGTERM, 2 s, SIGKILL) and reaped. No pattern-based kill of processes the daemon did not
  start.
- Environment is rebuilt from an allowlist (PATH to toolchain bins, HOME, CARGO_HOME set to the
  configured read root, TMPDIR to the private temp dir, CARGO_TARGET_DIR, CARGO_NET_OFFLINE=true,
  plus Rust's linker-bypass variables below);
  ambient credentials are not passed.
- Rust linker bypass (T06B): `/usr/bin/cc`, which every native build script's link step reaches by
  default, is Apple's `xcrun` shim — it writes an `xcrun_db` cache into the real Darwin user temp
  directory via `confstr(_CS_DARWIN_USER_TEMP_DIR)` (ignoring `TMPDIR`) and dyld-loads Xcode
  frameworks outside `Contents/Developer`, both denied by this profile, so every build script fails
  linking with exit status 71 even though compilation itself succeeds. `/usr/bin/ar`, which the `cc`
  crate reaches to archive compiled C sources into a static library (for example `blake3`'s
  `libblake3_neon.a`), is the same kind of shim and fails the same way. When a toolchain `clang` is
  found under the resolved Apple developer directory (`<dir>/Toolchains/XcodeDefault.xctoolchain/
  usr/bin/clang` for Xcode, `<dir>/usr/bin/clang` for the Command Line Tools), the check environment
  points the link step straight at it instead: `CC=<clang>`, `CXX=<clang>++` when that sibling
  exists, `AR=<clang dir>/ar` and `RANLIB=<clang dir>/ranlib` when those siblings exist,
  `SDKROOT=<dir>/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk` (Xcode) or
  `<dir>/SDKs/MacOSX.sdk` (Command Line Tools) when it exists, and either
  `CARGO_TARGET_<TRIPLE>_LINKER=<clang>` when the toolchain directory's own name (for example
  `1.98.1-aarch64-apple-darwin`) yields a target triple, or `RUSTFLAGS=-Clinker=<clang>` otherwise.
  No clang found: the environment is unchanged and the build fails exactly as it would unconfined.

Error example: a build script tries to read `~/.ssh/id_ed25519` or open a TCP connection; the
access is denied by the OS profile; the check reports whatever cargo reports.

## 4. Problem snapshot

### Language presence (T10B)

Both languages are configurable, but only the ones actually present in a worktree are ever
checked or mentioned. Presence is a handful of `stat` calls at the worktree root — cheap and
deterministic, never a tree walk for source files (for example `*.py`) — and it is re-evaluated
every time a check would be scheduled (the top of each scheduler run, §5), not cached once per
worktree: a worktree that gains its manifest between triggers is checked starting on its next
trigger, no daemon restart required.

- Rust is present iff `<worktree>/Cargo.toml` exists.
- Python is present iff the worktree root has any of `pyproject.toml`, `setup.py`, `setup.cfg`,
  `requirements.txt`, `Pipfile`, `pyrightconfig.json`, or a `.venv`/`venv` directory.
- TypeScript/JavaScript is present only when the worktree root has `tsconfig.json` or
  `jsconfig.json`; `package.json` alone does not enable it.

An absent language spawns no confined process, creates no cache directory, and records no
`ProjectCheckCompleted` telemetry (§8); the scheduler stores `Unavailable(Disabled)` for it
directly, without dispatching the configured [`Checker`]. Every renderer (the `<agent-ide>` block,
§6, and the `ide.context` problems page, §7) treats `Unavailable(Disabled)` as nothing — omitted
entirely, not a fixed phrase — so a pure-Python worktree never sees `rust: check failed` and a
pure-Rust worktree never sees `python: environment not found`. `Unavailable(EnvMissing)` stays
reserved for a language that *is* present but whose interpreter or environment could not be
resolved (§4 Python).

```rust
pub enum Language { Rust, Python, TypeScript }
pub enum UnavailableReason { Disabled, OutsideRoots, ToolMissing, EnvMissing, NoFiles, Fatal, Timeout }
pub enum CheckState {
    Ready,          // complete result for the configured scope
    Partial,        // result present but coverage incomplete
    Checking,       // first result not yet available
    Unavailable(UnavailableReason),
}
pub struct Problem { pub path: String, pub line: u32, pub column: u32,
                     pub severity: Severity /* Error | Warning */, pub code: Option<String>,
                     pub message: String /* <= 200 chars, cuts end in `…`, untrusted data */ }
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
  units produced no result, state is `Partial`. Missing `build-finished` → `Unavailable(Fatal)`,
  carrying the first `error:`-prefixed line of stderr as `detail` (T07B) when one exists — cargo
  dying before any JSON event (for example the ancestor-workspace-search failure in §3) never
  produces a `compiler-message` to draw a detail from otherwise.
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
  `Unavailable(NoFiles)` (T12B), carrying the fixed `detail` `pyright analyzed 0 files; check
  "include"/"exclude" in pyrightconfig.json or [tool.pyright]` — pyright ran against a resolved
  environment but had nothing to analyze (for example a malformed `include` glob), which is a
  project-configuration problem, not a missing interpreter; `EnvMissing` stays reserved for that
  (§4 Language presence). Exit ≥ 2 or unparseable output → `Unavailable(Fatal)`.
- Environment: use the project's own interpreter. Order: `<venvPath>/<venv>/bin/python` from
  `pyrightconfig.json` or `[tool.pyright]` in `pyproject.toml`; else `<worktree>/.venv/bin/python`.
  The interpreter file must exist; the resolved path *as found inside the venv* (symlinks not
  resolved) is passed as `--pythonpath`, so Python's own venv detection (`pyvenv.cfg` sitting next
  to that path) still applies — a uv-managed venv's `bin/python` is a symlink to a base
  installation, and passing its canonical target instead would run pyright against the base
  interpreter with no venv `site-packages` visible. `--pythonpath` takes precedence over any
  `venvPath`/`venv` pyright would otherwise read from the same config files, so the two sources
  never disagree. The interpreter's canonical target's installation prefix (its parent-of-parent)
  is added to the check's read set alongside the (unresolved) venv root, since pyright starts the
  interpreter to enumerate its search paths and, under Seatbelt, that exec follows the symlink to
  the base installation's binary and standard library. None found → `Unavailable(EnvMissing)`
  (never report the flood of unresolved-import errors a missing environment produces).
- Project source is never executed. Pyright does start the interpreter to enumerate search paths,
  so interpreter start-up hooks (`sitecustomize`, `.pth`) run under the same confinement.

### TypeScript/JavaScript (`node <private adapter> --project <root config> --pretty false --diagnostics --listFiles --noEmit`)

- Select a regular non-symlink root `tsconfig.json`, otherwise a regular root `jsconfig.json`.
  A denied or unprovable config is unavailable. Nested-only configs and project-reference graphs
  are outside this check's coverage; a solution-style root with no own files is `NoFiles`.
- The configured paths name pinned Node v24.4.0 and TypeScript 5.9.3. A first-party adapter calls
  that package's `executeCommandLine` with guarded filesystem methods under the existing
  network-denied runner. `--tsBuildInfoFile <private cache>/check.tsbuildinfo` preserves the
  project's `incremental` and `composite` diagnostics while keeping build metadata private;
  `--noEmit` prevents project output. `--extendedDiagnostics false --explainFiles false
  --traceResolution false` preserve the bounded report format. A project requiring a build or emit
  is not represented as a clean type check. Node and TypeScript have separate read grants;
  TypeScript's ancestor `node_modules` and `package.json` lookup paths receive narrow grants.
- `Ready` requires an untruncated, fully parsed output with a complete diagnostics footer, a
  matching file list, normal exit status, and at least one listed file under the worktree.
  Diagnostics report `TS` codes and exact file, line, and column; file-less diagnostics attach to
  the chosen config at line 0. Malformed output, denied diagnostic paths, and abnormal exits are
  `Fatal`. JavaScript diagnostics require the project's own `checkJs` setting.
- The adapter checks each compiler read and probe against host denies before touching the path,
  including symlink targets. TypeScript's file discovery uses its own `matchFiles` walker with
  guarded directory enumeration and explicit read-root admission, so refused reads cannot
  disappear as absent files. An alias is skipped before target resolution only when the pinned
  extension, include, and exclude patterns prove it can be neither a source file nor a traversed
  directory. An ambiguous alias outside the read grants remains `ReadRestricted`; ordinary
  non-file entries such as FIFOs follow TypeScript's own omission. Unrelated `.env`, `.key`, and
  `.pem` files may exist; a compiler-selected denied input yields `ReadRestricted`. Adapter write
  and delete operations are restricted to the private cache.
  Arbitrary diagnostic messages are redacted while path, severity and code remain. The runner's
  Seatbelt policy remains the host-enforced boundary, and the scheduler fences observed input
  generations; a transient edit that appears and disappears during one run is outside that fence.

## 5. Scheduling

- Triggers: successful `ide.start` (initial warm check), a native `PostToolUse`/`PostToolUseFailure`
  hook event whose retained `tool_name` passes the host's trigger predicate (the hook parser
  retains `tool_name` for post phases only; tool input and output stay discarded), a completed
  `ide.edit`, and — (T28B) for every host whose plate delivery rides replies (see §6) — each
  `ide.*` tool call of that host, so a native edit made between two calls (`apply_patch`, shell)
  is noticed even without any hook stream. That per-call trigger is free while the worktree inputs
  are unchanged (see the T20B skip below). An actor whose `ide.start` was refused (for example
  `conflict` because another actor owns the worktree) has no binding: it triggers no check and
  receives no block.
- Trigger predicate (`triggers_check`, T29B §4): Claude keeps the exact
  `Edit`/`Write`/`MultiEdit`/`NotebookEdit`/`Bash` writer allowlist. Codex has no certified writer
  allowlist, so every paired native `PostToolUse` triggers — except this product's own MCP tool
  names beginning `mcp__agent_ide__` or `mcp__agent-ide__`, whose results the helper just produced
  and whose paired native post must stay silent (the managed admission records the call as
  completed, so its own post is a replay and neither triggers a check nor delivers a plate).
  Other servers' MCP tools are deliberately never excluded because they may edit files; a missing
  or unrecognized `tool_name` triggers conservatively. This favors correctness over an incomplete
  writer allowlist; the T20B skip bounds the repeated-check cost (broad triggering is inexpensive
  in the common case, not universally free). Codex plate delivery is evaluated on every paired
  native post even when the event does not trigger a check.
- Per `(worktree, language)`: debounce `debounce_ms` after the last trigger; at most one running
  check; a trigger during a run marks it dirty and one more run follows (latest wins), and that
  follow-up run is subject to the same T20B skip-unchanged rule — with inputs unchanged from the
  last completed `Ready` run it spawns no process (T28B; reply-delivered hosts trigger on every
  tool call, so an unchanged worktree must not keep a check perpetually dirty). The next run
  for a pair starts no earlier than `max(debounce_ms, previous run duration)` after the previous
  completion. At most two checks run concurrently across the daemon. (T10B) Language presence (§4)
  is the first thing re-evaluated once a debounce fires, before the cooldown wait, cache directory
  preparation or checker dispatch: an absent language stores `Unavailable(Disabled)` directly and
  skips the rest of the run, so it spawns no process and creates no cache directory.
- (T20B) After presence, a firing for a pair whose last completed run ended `Ready` fingerprints
  the worktree inputs off the scheduler lock — a bounded `git ls-files -z --cached --others
  --exclude-standard` walk hashing each path with its size and mtime; failure, timeout or a
  non-git worktree counts as unknown — and skips the run when the fingerprint equals the one
  recorded at that completed run's start: no checker process, no `running` flag, no snapshot, no
  completion hook, and the stored snapshot stays current. A check requested by `ide.start`
  activation always runs, as does a pair with no completed snapshot, a pair whose last run ended
  in any non-`Ready` state, and any run whose inputs changed (the fingerprint is retaken at run
  start, so an edit made mid-run is not lost).
- A new worktree's Rust cache directory, when absent, is first cloned copy-on-write
  (`clonefile`/`cp -c -R`) from the most recently completed sibling worktree of the same repository;
  clone failure falls back to a cold check.
- The latest completed snapshot per `(worktree, language)` is kept in memory; a running check never
  replaces it until it completes. A completed `Unavailable(Fatal)` or `Unavailable(Timeout)` never
  replaces an existing `Ready`/`Partial` snapshot (the previous counts stay visible); other
  unavailable reasons replace it, including `NoFiles` (T12B): a project misconfiguration that
  makes pyright analyze zero files is a durable condition, not one bad run, so it must surface
  even if it appears only after an earlier `Ready`/`Partial` result — the same replacement
  semantics as `EnvMissing`.

## 6. The `<agent-ide>` block

The block is a **status plate** (T18B): it always tells the agent the current state of each
configured **and present** (§4) language, in fixed order (rust, python, typescript), process states
included,
and it is (re)sent whenever the rendered status changes — and only then. A mixed-stack worktree
with both a `Cargo.toml` and a `pyproject.toml`:

```
<agent-ide>
rust: 3 errors (+2), 5 warnings | python: environment not found
</agent-ide>
```

- Item forms:
  - Result: `<lang>: <E> errors, <W> warnings`; append ` (partial)` for `Partial`; the delta
    `(+N)`/`(-N)` after a count appears only when that count changed since the last counts
    delivered to this actor for this worktree (a `checking` plate keeps that baseline).
  - `<lang>: checking (first check)` — this session has no result for the language yet (none
    exists, or the stored one predates this session's activation of the worktree; its counts are
    never shown).
  - `<lang>: checking (files changed; last result: <E> errors, <W> warnings)` — a check is running
    after the session's last result; the last counts stay visible (`checking (files changed)` when
    the last result carried no counts).
  - `Unavailable` renders fixed text: `outside allowed roots`, `tool not found`, `environment not
    found`, `no files analyzed` (T12B), `check timed out`, and `check failed` — the last with the
    first 80 UTF-8 bytes of the snapshot detail in parentheses, `check failed (<detail>)`, control
    characters replaced by spaces and `<`/`>` by `?` so the detail cannot forge the framing.
  - (T10B) A language absent from the worktree (`Unavailable(Disabled)`, §4) is never mentioned; a
    pure-Python worktree renders `<agent-ide>\npython: 2 errors, 0 warnings\n</agent-ide>` with no
    `rust:` item. When no configured language is present, no block is emitted.
- Total size including tags ≤ 256 UTF-8 bytes. Over the cap the block shrinks in order: deltas,
  then details and last-result texts (`check failed`, `checking`), then an equal byte share per
  item; a language is never dropped.
- When `checking` is shown: only while a check is actually **running** (or when the language has no
  session result), not while a debounce timer is merely armed. Hook events cannot tell a source
  change from a no-op tool, so a trigger alone never flips the plate. A real change therefore costs
  at most two plates (`checking (…)` while the check runs, then the result with its delta), and a
  quiet session costs none; a no-op check observed while running costs the same two, the second
  being the unchanged pre-check text. A hook that fires before the check starts sees the
  previous plate and emits nothing.
- Emission rule: the plate is emitted only when the item set without deltas differs from the last
  block delivered to that `(actor binding, worktree)`. Identical state is never re-emitted. A
  language's disappearance from the rendered set is only ever a consequence of an actual state
  change (it stopped being present, or an admitted worktree went `outside_roots`, etc.) — never
  something the emission or delta logic manufactures on its own account (for example, on a binary
  upgrade with no change underneath, the daemon restarts and the next block for each actor is
  simply a fresh first delivery, not a synthetic delta).
- Delivery: `HostKind::feed_delivery` decides the carriers in one place, and consumers use only
  its `allows_hooks`/`allows_replies` capability predicates — `Hooks` for Claude, and (T29B)
  `HooksAndReplies` for Codex, unconditionally. The plate data and emission rule are identical;
  only the carriers differ. Codex's policy never inspects `hooks.json` and never latches a
  "hooks work" flag: when the operator's hooks are absent, untrusted, or stopped working, the same
  capability set simply operates replies-only.
- Deduplication across carriers (T29B §5): both carriers resolve exactly the same binding
  fingerprint — the hook derives the daemon channel from the same published attachment, the hook
  actor is the MCP `threadId`, and a paired native post looks up the already-active actor/channel
  binding instead of minting a generation — and both call the same feed under its wiring lock with
  fingerprint plus worktree. The first carrier to find a due status consumes it; the second sees
  unchanged status and stays silent, for either arrival order and for concurrent delivery. This
  guarantees deduplication at response construction, **not exactly-once model receipt**: a plate
  is marked sent when the response is produced, and a lost response is never retried.
- Claude (`Hooks`): `PostToolUse` / `PostToolUseFailure` hook `additionalContext`, taken from the
  ready cache inside the existing hook deadline. The hook never waits for a check. Marking as
  delivered happens when the hook response is produced (at most once; a lost hook response is not
  retried). When the v0.2 one-shot native feedback is eligible in the same hook response, the block
  is prepended and both are concatenated once within `MAX_FEEDBACK_BYTES`; each is marked delivered
  independently. A due plate is also delivered on a foreground helper's own `Bash` post hook after
  its settlement, and on the `PostToolUse` of the Claude MCP tool whose invocation settled (T22B);
  neither triggers a recheck nor advances the native epoch. Claude `ide.*` replies never carry the
  plate, even while one is due.
- Codex (`HooksAndReplies`, T28B/T29B): the native hook carrier works like Claude's — a paired
  native `PostToolUse` delivers the due plate in `hookSpecificOutput.additionalContext` inside the
  existing 250 ms hook deadline, never waiting for a check, followed by any eligible one-shot
  native feedback within the same bounded context. In addition, every terminal `ide.*` reply —
  `ide.context` (file pages and the problems kind), `ide.diff`, `ide.edit` in every outcome,
  `ide.inspect` results that resolve a pending job, `ide.start` completion, and typed error
  replies — carries the due plate at the start of what the model reads, followed by a newline and
  the normal reply text. Concretely, the plate leads the compact `content` text and is exposed
  verbatim as the `status` string field of the `structuredContent` object (the field
  Codex reads). `ide.stop` replies and `pending` placeholders carry no plate: a placeholder is
  not terminal, so the plate goes with the answer that resolves it. Before the plate is attached,
  the complete final MCP envelope (plate plus possibly shrunk reply body) is fitted by the daemon
  against the same byte bound the reply renderer applies, shrinking only owner text; the plate is
  marked delivered only when it was attached whole — a plate that cannot fit stays due for the
  next terminal reply, exactly like the hook-carried case.

## 7. `ide.context` with `kind: "problems"`

Parameters: `{"kind": "problems", "language": "rust" | "python" | "typescript" (optional), "offset": u32 (optional)}`.
`path` is not required for this kind; any other `kind` value or absent `kind` keeps v0.2 behaviour.
Reply: per language the state, counts, and up to 20 problems from `offset`, each
`path:line:column severity [code] message`, plus `next_offset` when more exist. Messages are
untrusted text. An `unavailable:<reason>` state line carrying a [`ProblemSnapshot::detail`] (T05B)
appends it in parentheses, for example `rust: unavailable:fatal (error: failed to run custom
build command for \`blake3 v1.5.0\`)` or (T12B) `python: unavailable:no_files (pyright analyzed 0
files; check "include"/"exclude" in pyrightconfig.json or [tool.pyright])`; a snapshot with no
detail renders exactly as before. The `<agent-ide>` block (§6) carries only the first 80 bytes of a
failed check's detail. Process states use the block's vocabulary (T18B): a check running after the
session's last result renders `<lang>: checking (files changed); last result: errors: N; warnings:
M`, and a stored result predating this session's activation renders `<lang>: checking (first check
in this session); previous session result: errors: N; warnings: M`; a language with no result yet
renders `<lang>: checking (first check in this session)`. A language whose snapshot dropped
problems to the 500-entry cap (T19B) adds one header line directly after its state line, on every
page: `<lang>: list truncated to first 500 problems; counts above are complete` — the counts in
the state line are computed before the cap, so only the list is cut. A truncated `Problem`
message ends in `…` (T19B). No
new tool and no new `AssistanceMethod`. The problems kind is answered from the daemon's in-memory
snapshots for the caller's bound worktree on every host; it is never dispatched to the Claude
foreground helper and needs no provider.

A language absent from the worktree (T10B: `Unavailable(Disabled)`, §4) contributes no state line
and no problems at all — omitted exactly as it is from the `<agent-ide>` block, never rendered as
`rust: unavailable:disabled`. `checks disabled` stays reserved for no configured language at all
(no `project_checks`/`allowed_roots`) or a `language` filter matching no configured language, as
before. When every language the request matches is present-but-absent this way (for example a
pure-Python worktree with no `language` filter, or `language=rust` against a pure-Python
worktree), the page renders the single line `no supported project detected` instead.

## 8. Telemetry

One new event `ProjectCheckCompleted { language, state, duration_ms, errors_bucket, warnings_bucket }`
with counts bucketed (`0`, `1-9`, `10-99`, `100+`), under TELEMETRY-r1 privacy rules (no paths or
messages). This extends the TELEMETRY-r1 event scope (six MCP methods and native fallback) by one
tag, which is added to the closed telemetry query filter.
