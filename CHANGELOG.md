# Changelog

## Unreleased

### Changed

- Language-server native cache namespaces (rust-analyzer's build-script and proc-macro target, gopls' build and module caches, …) live in `~/.agent-ide/providers/<namespace>` instead of the daemon runtime in `/private/tmp`, so they survive a daemon restart and a reboot. The namespace key is now derived from the worktree directory (canonical path, inode and creation time) instead of the boot-local database identity, and hashes the accepted server and toolchain executables (path, identity, measured digest), settings, effective configuration, toolchain and trust: a restarted daemon adopts the namespace only when all of them are unchanged, a changed one starts cold in another directory and the old one ages out. Each namespace carries a `worktree.path` marker; `agent-ide cache status` lists them as `providers`, and the checks rules retire them (worktree gone, 14 days idle, 8 GiB budget least recently used first). When the state root is unusable the namespaces stay in the runtime directory as before.
- Check-cache eviction is tiered. A worktree idle for 2 days loses its rustc `incremental/` state first and keeps `deps/` and `build/`; whole worktrees go only by the existing gone, idle (7 days) and budget rules. Independently, finalized rustc incremental sessions older than the newest of their crate are swept (newest by rustc's base-36 session timestamp, never mtime), each only while its sibling rustc session lock `s-<timestamp>-<random>.lock` is held exclusively, never `-working` sessions, and only under the worktree's exclusive claim; a malformed `s-…` name or a tie at the newest timestamp leaves the crate alone.
- Retention budgets charge what is actually allocated once: hard-linked inodes and perfect APFS clones (sibling worktree caches are copy-on-write clones of each other) are counted once per family, and removing an entry frees only what no surviving entry still holds. `agent-ide cache status` shows logical (file lengths), charged (budgeted) and private (APFS private bytes, a reclaim estimate that is never charged) bytes per family.

### Fixed

- A panic in an accepted-connection task is journaled (`daemon failed internal`, detail `connection_task_panic`) on both the live accept loop and the shutdown drain; cancellation by the drain stays silent.
- The daemon's poisoned-lock policy is stated and tested (`docs/architecture.md`, "Poisoned locks"): the lease idle clock, the shutdown hooks and the environment selection cache recover a poisoned lock instead of panicking; authority state keeps failing closed.

## 0.10.7 — 2026-10-08

### Fixed

- A read-only activation no longer schedules a project check: its `ide.start`, native post-edit triggers and environment changes start none, so three readers no longer kick a cold `cargo check`, and a writer's results are not marked stale by a reader's start. A reader still sees the problems a writer's check produced.
- `ide.start` of another root by the actor that already holds an activation elsewhere now hands the old activation over by itself, with no `ide.stop` first, when nothing is pending on it (no queued or pending job, no unsettled or `outcome_unknown` edit, no running test). The old authority, provider sessions, cache ownership and leases are released as a stop releases them, the same session's grant moves in one durable step (a stopped binding cannot start again; another session of the actor is revoked first), the new work of the old binding is refused while the handover settles, and the handover is journaled. With pending work the `start:actor_owns_another_worktree` refusal stays, now naming the held worktree path and what keeps it, and releases nothing. Another actor's activation is never touched.
- Cache eviction is no longer paused by development and test builds of `agent-ide` (a gate's or scratch `target/debug/agent-ide`). A running `agent-ide` whose executable file embeds the lease-protocol proof of this source and was not rebuilt since the process started counts as lease-taking; installed 0.9.1 and older releases, older dev builds without the proof and unreadable executables still pause eviction, and `agent-ide cache status` lists both groups. See `docs/cache-retention.md`, rule 3.
- Telemetry write-ahead logs are bounded. Writable stores keep at most 1 MiB of WAL after a reset (`journal_size_limit`), and the telemetry writer truncates the WAL on the store's owner connection (`wal_checkpoint(TRUNCATE)`) after its shutdown drain and whenever it has been quiet for 30 seconds; a checkpoint blocked by a reader is retried at the next quiet period. WAL files are never deleted by hand and no `VACUUM` runs.
- A launch now publishes its telemetry marker atomically (fresh `0600` file, synced and renamed under the launch directory's lease, read back) instead of an unchecked write, replaces a torn or historical `0644` marker and refuses a symlink or foreign file. When the marker cannot be published the daemon starts with runtime-local telemetry and the failure is journaled as `telemetry_marker_unavailable`, so no new persistent store is left without a marker.
- Stale Claude hook key hints (`/private/tmp/ai-k-*`; 5,206 were found on one machine) are collected by the hourly sweep and `agent-ide cache prune`: only private directories older than a day whose cached key path and runtime directory definitely do not exist, under an exclusive directory lock the publisher shares. `cache status` counts them; collection pauses while an older front that publishes without the lock is alive.
- A long-lived daemon no longer reaches a receipt limit. The 65,536-receipt lifetime cap and its `COUNT(*)` per write are gone: every 64 admissions (and at open) the store retires settled receipts (`committed`, `rolled_back`, `busy`) older than the newest `store.receipt_capacity` admissions into a 16-byte tombstone, one transaction per batch. A replay of a retired operation ID is refused as `ReceiptExpired` (an edit answers `outcome_unknown`), never executed, also after a restart. A queued, started, `outcome_unknown` or unrecognized receipt is never retired and never limits admission. Followup: tombstones are kept forever (16 bytes each) and unresolved receipts are never aged; the durable-store rework owns their retention.
- `self-install --replace` no longer replaces or deletes a release directory that a live process (a running front or daemon, or the installer itself) executes from: it refuses and names the process, or refuses when the process list is unavailable. A retired copy that is still in use is kept and swept by a later install.
- A scratch install with `--home` or `--prefix` no longer rewrites the live `~/.local/bin/agent-ide` launcher: without `--bin-dir` and `--share-dir` they follow the scratch root (`<home or prefix>/bin`, `<home or prefix>/share/agent-ide`).
- `agent-ide cache --help` (and `-h` after any subcommand) prints the usage instead of `invalid daemon response`.

## 0.10.6 — 2026-10-08

### Fixed

- The fault report recognizes only the journal producers' panic record shapes. An observation failure mentioning a file such as `panic at.txt` no longer raises a panic alert; real hook, job and containment panic records still do.
- Crash containment now reaches the fault report: fatal daemon failures and caught job panics raise the panic alert once, uncollected panicked jobs keep their request id, and crash-drained pending jobs leave classified terminal records. An already-written edit settled as `outcome_unknown` stays unknown and is never counted as a success. Forced daemon replacements appear as lifecycle events without adding tool calls.
- A read-only activation with no writer beside it now gets working `ide.symbol`, `ide.graph` and the bare-name tools. It owns the provider namespace from its first semantic call, other readers borrow its session, and a writer that starts later takes the namespace over only after the reader owner's sessions, views and namespace were released, so a namespace never has two live owners; when the writer leaves, the next reader call claims it again. Reader upgrade, writer downgrade and sibling worktrees follow the same rule, and a reader whose cleanup fails refuses the writer's start without losing its own namespace (a retry redoes the cleanup). Until now only a writer ever retained the namespace, so every semantic call of a writer-less reader failed with a stage-less `provider_unavailable`.
- Every `provider_unavailable` names a parenthesised stage. The Rust, Python and TypeScript backends name the view, spawn, initialize and request steps; a bare refusal is named after its server and step (a refused namespace lookup names its own cause); the bare-name search and no-anchor refusals are staged; and the real no-server refusal journals `<tool>:provider_unavailable ext=<ext> (provider: no server for this file type)` while the reply text is unchanged. A refusal that nothing named gets `(provider: cause not reported)` instead of rendering as "no language server is configured".
- A provider session that failed its workspace load, or that a call failed on, recovers: it is retired when its project input files (manifests, lock files, configuration; `LanguageServer::project_inputs`) changed or Git's `HEAD` moved, and the next call starts a fresh session. Stated ceiling: the scan goes four directory levels and 5,000 entries deep and stamps each regular input file by its length, modification time and a hash of its first 256 KiB, so an ordinary edit is seen, while a same-length edit past 256 KiB that keeps the modification time, a manifest deeper than four levels or a symlinked manifest is recovered only on the next `HEAD` move or session restart. The failure is recorded per session where a provider call fails — session start, workspace load, call hierarchy, workspace symbols, references and document symbols — so a tool that then answers from a source outline still counts it, and one session's failure never marks or clears another's. An unchanged invalid project keeps its staged refusal and is never restarted in a loop or on a timer; a successful call clears the failure mark.
- A daemon whose worker, inspection task or provider backend failed no longer keeps answering `ok` while every call hangs or answers `internal` until the transport deadline. Any caught panic or unexpected task end now marks the daemon failed: its health answers `restarting`, new calls are refused as `restarting` ("nothing was applied; repeat this call"), queued jobs are answered without running, and it exits by itself after 500 ms, keeping its runtime store and receipts (an orderly exit still removes them). A call that panicked after an edit wrote answers `outcome_unknown`, never `internal`, and the edit is never replayed. A panic inside the provider's session start no longer leaves the language-server slot empty. `accept` errors from descriptor or memory exhaustion are retried instead of ending the daemon.
- The front now recovers on its own. A call that times out, loses its reply or answers `internal` triggers one liveness probe of the daemon's health path (served independently of the job queue): a daemon that says `restarting` or has left is re-established at once, in place for managed Codex (store and receipts kept); a daemon whose health path answers nothing across at least two probes spanning 30 seconds is force-replaced (`SIGTERM`, 8 s, `SIGKILL`) after the front verified the lock holder (the daemon lock now records its pid; only a live process running the `agent-ide` executable that still holds the lock), and the replacement is journaled once (`wedged_daemon_replaced`). A daemon whose health path answers is never signalled however long its jobs run, and a shorter stall keeps its daemon and binding.
- A transport timeout of a tool call (connect or reply) is now written to the client error journal (`timeout`, reason `deadline`, `transport:<phase>_timed_out:<tool>`) instead of leaving no trace.
- A Claude session whose shell left the project (`cd` into a log directory) no longer loses every call to `host_binding (missing_pre)`. The `claude-hook` used to route a pre by the shell's current directory while the session's MCP is registered under its `CLAUDE_PROJECT_DIR`, so every pre fired from outside the project found no rendezvous (11 of 11 calls of one reproduced session). A directory route is now accepted only inside the same repository as `CLAUDE_PROJECT_DIR`; when it finds no rendezvous of the session the hook routes through `CLAUDE_PROJECT_DIR`'s own registration. It never reaches a different registered repository (a shell standing in another repository does not deliver the session's pre there), keeps `allowed_roots`, the attachment and session checks and exact `toolUseId` pairing, and a `CLAUDE_PROJECT_DIR` that is present but empty, relative, not a directory or removed drops the pre locally (`hook_project_dir_invalid`) instead of falling back to the directory route. The miss is journaled as a per-call `warn` (`hook_cwd_rerouted:<reason>`), never the directory.
- No early exit of the daemon's ingress answers a bare `unavailable: host_binding` any more: an invalid envelope, invalid parameters, an unknown hook phase, a mismatched call, an unknown attachment, a poisoned daemon lock and a daemon without a worker each name their closed cause (`invalid_metadata`, `missing_field`, `invalid_parameters`, `unsupported_hook_phase`, `mismatch`, `invalid_attachment`, `internal_lock`, `worker_unavailable`).
- A store failure outside `ide.stop` is no longer reported as an authority, source or identity problem. A busy or locked store is `capacity (store:busy)`, a full receipt store `capacity (store:store_full)`, an expired wait `deadline (store:store_deadline)` and any other store failure `store:unavailable`, on reads, `ide.start` (worktree identity and activation, which keeps its unknown-outcome handling), environment choices and the re-authorization of a retained `ide.inspect` result (a busy store keeps the result so the inspection can be repeated).

### Changed

- Timing assertions are separated from correctness in four flaky tests (scheduler cancelled-check lease, second-worktree name index, durable-unavailable reprobes, warm Rust inline answers); a combination smoke reads `.log`, `.md` and `.toml` files in every form (path, lines, ranges, batch, mixed batch, symbol-only batch), each followed by a second call.
- The test-only variable `AGENT_IDE_TEST_FAULT=<point>:<flag file>` (points `inspection`, `loop`, `worker_exit`, `worker_construct`, `ensure`, `edit_after_write`) injects one fault per consumed flag file; like the panic seam it exists only in builds with the `test-seams` feature.

### Added

- The error journal is complete enough to attribute every fault (contract `ERRORLOG-r3`, `docs/contracts/error-log-v0.3.md`). Every dispatch line carries the serving `version`, `host`, `role` (reader/writer/none), `language`, request `form` (parameter names only), the host call id as `request`, and an `eligible` flag, with explicit `unknown`/`none` values; an inspection names the call that queued its result as `origin`. A success through a weaker path (the lexical context or outline fallback, edit diagnostics left unknown) is the new `degraded` outcome. A job that finishes after its caller was told `pending` writes a `pending_completion` record classified like a dispatch line. The front journals the calls that never got a typed reply (`front:invalid_parameters`, `front:unavailable`, `front:timed_out`, `front:busy`, `front:outcome_unknown`, `front:reestablish_failed`, `front:incomplete`, `front:missing_host_metadata`). A hook refused on a channel that already holds a binding leaves one per-call `warn` (`hook_refused:<cause>`), and `daemon failed` names its stage and error class (`initialize|serving|shutdown:<class>`; a failed initialization used to leave no line). Only closed values and opaque ids are written: no source text, path, model-chosen name or request value.
- `cargo xtask fault-report` is the official daily fault report and alert, replacing the `errstats.py` counter. It applies the stability plan's failure taxonomy (fault, unexplained, caller, honest) to the error journals: per-day and per-class tables with known versus unexplained faults, splits by method and repository, the longest outage streak, pending settlement, daemon starts, failures and panics, hook warns, and `--alert-threshold`/`--min-calls` to fail on a fault rate above a stage target (a journaled panic also fails; an unexplained share above 0.1% and a class that doubled day over day only warn). On the frozen ten-day journals it reproduces 16,379 terminal field calls and 692 IDE faults (500 known, 192 unexplained; 4.22%).

## 0.10.5 — 2026-10-07

### Fixed

- A ranged `ide.read {path, ranges}` of a file no IDE language reads (a `.log`, Markdown, YAML) no longer kills the daemon's worker. Every range item used to become an `unsupported_file` refusal with no delivered file, the batch then drained the first edit source from an empty list and panicked, and the worker — the daemon's only job task — died: every later call of every session of that repository answered `internal` or hung until the transport deadline, until the daemon was replaced.
- A panic inside one job no longer kills the worker. That call answers `internal`, the error journal gets one line with the panic's source location and the call's method (`panic at crates/…/symbols.rs:561:44 during read`, under the call's correlation id) and later calls keep working. The panic text itself is never journaled, since it can carry paths or source. A daemon panic hook now writes the location of panics outside a job (`panic at <file>:<line>:<column>`) to the journal too; a daemon's stderr is `/dev/null`, so they left no trace before.

### Changed

- `ide.read` returns the text of any readable text file whatever its type: `{path}` alone (the whole file, paged like any read), `{path, lines}`, `{path, ranges}` and a bare file path as an item of `{symbols}`. It is numbered as ever and, when no IDE language analyzes the file, carries one `note: no code analysis for this format` line before `source_ref`. A symbol address into such a file (`notes.md#title`) is one soft item, `notes.md has no code symbols; read it with ide.read {path, lines|ranges}`, and never fails a batch. A binary file (invalid UTF-8 or a NUL byte), a directory or an unreadable file is refused softly, never `internal`: `source_unavailable (read:not_text)` for the single forms, one `not a readable text file` item for a batch, both naming a native tool (`file`, `xxd`). `ide.outline`, `ide.symbol`, `ide.graph` and `ide.read {symbol}` of such a file still answer `unsupported_file`, now pointing at `ide.read` with `lines` or `ranges`.
- The test-only environment variable `AGENT_IDE_TEST_PANIC_READ_PATH` (panics an `ide.read` of one path) is read only by builds with the `test-seams` cargo feature; `xtask check` runs a default-build test proving a release build ignores it.

## 0.10.4 — 2026-10-07

### Fixed

- An orchestrator and its subagents no longer lose their connection when one agent starts work in a different repository. `ide.start {root}` naming another repository, while the session has other actors, is refused with `host_binding (other_repository: bound to <path>, asked <path>)` and moves nothing; before, the whole session followed the start to the other repository's daemon, the other agents' hooks stayed behind and their next calls failed `missing_pre`. A session with a single actor still moves as before.
- A long review session no longer fails with `capacity` after reading 64 different files. The files an activation keeps registered for refresh have a budget of their own (256) that is independent of the retained-results limit; when it is full the least recently read file is evicted to make room, so a read is never refused for it (an edit built on an evicted file is judged by its bytes as always: it applies while they still match and gets the usual `stale_source` once they changed). It used to refuse the 65th file for the rest of the session, and the refused read still left an observation row behind.
- `ide.stop` no longer answers `workspace_authority` ("this result belongs to an older IDE activation") for every durable store failure. It names the cause: `capacity (stop:busy)` for a busy or locked store, `capacity (stop:store_full)` for a full receipt store, `deadline (stop:store_deadline)` when the wait expired, `workspace_authority (stop:store_unavailable)` for other store failures. The daemon retries a failed stop itself, by its idempotent operation id — twice inline, then in the background with a doubling pause (one to sixteen seconds) until the store answers — so the agent never has to repeat it.
- A daemon whose connection permits are exhausted (parallel agents of one repository) now answers a call with a typed `busy` reply ("nothing was applied; repeat this call") instead of dropping the connection, which the client had to report as an unknown outcome — and, for a lost pre-hook, as a binding error. Hooks have their own small lane of four, so a burst of slow tool calls no longer starves the hooks that authenticate them, and every refusal is counted in the error journal (one line per lane per minute with the suppressed count).
- A Claude session that re-rooted to another repository no longer pins the old repository's daemon: the session's lease watcher now follows the current lease, drops the old one so the old daemon can idle out, and notices when the new daemon dies instead of only at the next failed call.
- `doctor` no longer counts every old file or directory in the temporary directory as a "stale agent-ide runtime entry" (it reported tens of thousands, mostly other programs' files). It looks only at the product's own runtime directories (`ai-` and `ai-r-` plus sixteen hex digits, owned by you and private), removes those older than a day with no daemon listening, and reports how many it removed; every other entry, the `ai-k-` key caches included, is left alone.
- A Python environment selector refused as outside the allowed roots (for example a base
  interpreter passed instead of the project venv) now answers with a text that names the
  selector and the expected project venv form, instead of telling the agent to move the project
  root.

### Changed

- The environment variables that exist only for the product tests (`AGENT_IDE_TEST_DROP_REPLY`,
  `AGENT_IDE_TEST_DAEMON_VERSION`, `AGENT_IDE_TEST_FRONT_VERSION`,
  `AGENT_IDE_TEST_LEGACY_CLAUDE_DAEMON`, `AGENT_IDE_DAEMON_STARTUP_STALL_MS`,
  `AGENT_IDE_MANAGED_CODEX_RESTART_STALL_MS`, `AGENT_IDE_CODEX_RENDEZVOUS_STALL_MS`) are read only
  by builds with the `test-seams` cargo feature. A release build ignores them, so a wrapper or CI
  job that inherits one can no longer change binding or transport behaviour.

## 0.10.3 — 2026-10-06

### Fixed

- A repository's IDE no longer stops working after about a thousand reads: every read took a
  permanent entry in the daemon's state store, and once its fixed cap was reached every durable
  write failed, surfacing as `source_unavailable` on reads, `workspace_authority` on `ide.stop`
  and `worktree_unresolved:identity_commit` on `ide.start` for every session of that
  repository. Reads no longer take an entry, and the cap for the remaining operations is
  65 536 (was 1 024), which also unblocks stores that already reached the old cap.
- Several agents of one Claude Code session (an orchestrator and its subagents, each in its own
  worktree of the same repository) work side by side through one IDE: one agent's `ide.start`
  no longer moves the others' calls to its worktree (they failed `missing_pre` or
  `hooks_not_delivered`, and `ide.stop` never succeeded), and removing a finished subagent's
  worktree affects nobody else. After a daemon restart each agent is re-activated with its own
  root and role on its next call, however much later it comes; a stopped agent never is. While
  an older daemon still serves right after an upgrade, a session works as in 0.10.2; after a
  downgrade, an agent the older daemon cannot identify is refused `never_activated` and
  recovers with `ide.start`, never bound to another agent.
- A call whose reply was lost after it reached the IDE is never sent again: an edit, test or
  stop answers `outcome_unknown` (check with `ide.diff` or the test status) instead of a false
  `missing_pre`, and a repeat of the same call answers as a replay. The client accepts the
  daemon's full reply size.
- The shared result store no longer fills up with parallel agents (`worker:actor_share_full`):
  an agent can always free its own oldest results, the newest file versions an agent keeps
  count inside its share, and an agent idle for 15 minutes gives up its results to others.
- A line-range edit can no longer land on shifted lines: line numbers on the `source_ref` of
  an edit that moved lines are refused `stale_source (edit:lines_moved)` until the file is
  re-read, and every edit reply states its own shift (`lines after N moved +K`). An unmatched
  `old` text names the closest lines, the first differing line and its text, or says
  `no similar text`.

## 0.10.2 — 2026-10-06

### Fixed

- Cache cleanup no longer stops after an upgrade while sessions of the previous release are
  still open: every release after 0.9.1 takes leases, so a sweeper trusts older ones too. It
  used to treat any release older than itself as lease-unaware and keep every check and
  telemetry cache until those sessions ended (29 GiB kept against a 20 GiB budget).
- A Rust file whose workspace failed to load and that the source outline also refuses is
  refused with `use native reads`; it used to say `ide.outline` and `ide.read` still answer
  from source, which was false for that file.

### Changed

- The bundled `agent-ide` skill matches the 0.10 tools: sixteen listed test failures and
  the reproducing `rerun:` line, edit diagnostics split into `new:`/`pre-existing:` with the
  `project errors in other files` and `not_analysed` next steps, rename and batch limits,
  Python environments and its missing call hierarchy, `unsupported_file`, diff paging by
  parts and `paths`, page markers instead of a `continuation` field, and the exact retry
  facts. The bundled reviewer agent stops its activation before reporting.
- The check caches' budget is 15 GiB (was 20 GiB): least recently used worktrees past it are
  removed and their next project check builds again from cold.

## 0.10.1 — 2026-10-05

### Fixed

- A Rust file with a plain comment separated from the next item by an empty line (a
  `// xtask:…` marker, a section rule) gets its outline, symbol reads and edits while
  waiting for rust-analyzer; the source outline used to refuse it and answer `provider_unavailable`.
- A symbol address naming both a field of a type and a method of one of its blocks
  (`PathSnapshot/source`) resolves to the method; it used to pick the field silently, so an
  insert or replace aimed at the method landed inside the type declaration.
- Two `old` text changes in one `ide.edit` batch overlap only when their matched bytes
  intersect: separate substrings of one line (parts of one string literal) apply together, right
  to left, with exact landing lines; they used to be refused as overlapping lines.
- An edit that leaves its file clean while the project check reports errors in other files says
  so (`current_clean for this file; the project check reports N errors in other files`) and points
  at `ide.context` problems instead of `ide.diff`.
- `ide.diff` no longer fails `capacity` on a hunk larger than one reply: the hunk arrives in
  line-bounded parts on consecutive `ide.inspect` pages (`hunk N of T, lines A-B of L`), and a
  single line too long for any reply becomes a notice with the `ide.read` that shows it, after
  which paging continues. Capacity refusals name the resource that is full.
- `ide.diff` accepts `paths` (literal worktree-relative files or directories) and shows bounded
  untracked text as additions, without staging it. Untracked content is a best-effort snapshot:
  a file that changes during capture stays name-only and never breaks the diff. `staged` mode
  lists untracked names only, as before.
- A test run's `rerun:` line reproduces the run: `cd <dir> && env NAME=VALUE … <argv>` when
  `ide.test` was given `cwd` or `env`.
- A failed run lists up to 16 failing tests (was 8) and says how many more are in the full output
  instead of dropping them silently.
- `ide.edit` inserts count the blank lines already beside the insertion point toward the
  spacing, so inserting next to a neighbour that is already separated (two blank lines before a
  comment, say) no longer doubles the separation; the landing line numbers follow.
- Readers whose activation predates the current writer can borrow its live language session:
  `ide.outline`, symbol reads, symbol cards and semantic context retain the reader's source
  authority instead of failing on the writer's different epoch, including nested Python packages
  without a virtual environment.
- Python usages cross roots that import each other by name only: `ide.symbol` on a method of a
  package in one nested root (`libs/contracts`) now counts its calls in another root that
  reaches it through `PYTHONPATH` (`services/agent` with just a `requirements.txt`, no
  environment installing the package). The Pyright session receives every nested Python root
  (its `src` under a src layout) as `python.analysis.extraPaths`; before, the import was
  unresolved there, the module-level instance had no type and its method calls were never
  references. A project config file's own `extraPaths` still wins.
- `ide.edit` on a file the project check cannot analyse (a Python package with no environment,
  a check that could not run) now answers the language server's diagnostics for the exact
  post-edit source as `current_reported`, labelled as language-server diagnostics with the
  check's reason, or `not_analysed (<reason>)` when the server has none — it used to drop them
  and answer `unknown`. Language-server diagnostics in edit replies and `ide.context` carry
  `path:line:col severity [code]`.

## 0.10.0 — 2026-10-05

### Added

- Automatic cache retention (`docs/cache-retention.md`): each daemon sweeps `~/.agent-ide` 60 s
  after start and then hourly (one sweep per hour machine-wide). Check caches go when their
  worktree is gone, idle 7 days, or least recently used over a 20 GiB budget; telemetry stores
  when gone, idle 30 days or over 1 GiB; installed releases other than `current`, the newest
  three, those installed in the last 14 days and those a live process executes. Every
  activation, check and test run holds a shared lease on its worktree, a sweep claims a cache
  only with an exclusive lock and renames it to trash before deleting, and nothing in checks or
  telemetry is removed while an older (lease-unaware) `agent-ide` process is alive. Removals
  are logged as `retention` events with the bytes freed.
- `agent-ide cache status` (dry run) and `agent-ide cache prune` (apply now).

### Changed

- The lock-free start-up sweep of gone worktrees' check caches is replaced by the leased sweep;
  `ide.start`, project checks and `ide.test` refuse to run when the worktree's retention lease
  cannot be taken (`start:cache_lease`).

## 0.9.1 — 2026-10-05

### Fixed

- `ide.outline`, `ide.read`, `ide.symbol` and `ide.graph` on a file no IDE
  language reads (`Cargo.toml`, a shell script) answer
  `unsupported_file: <path>` with the read that works (`ide.read` `path` and
  `lines`) instead of a misleading `provider_unavailable`; a batch read names
  such a file per item.
- An edit of a Rust file reached only behind a gate — an integration test with
  `#![cfg(feature = "…")]`, a `cfg`-gated `mod` — says it is built only under
  that condition instead of advising to declare it. A `#[path = "…"]`
  declaration naming a file beside the declaring file now reaches it, so such
  files get their project check. `not_analysed` reasons carry their own next
  step; Python's name the missing root or environment.
- An edit of a file no language checks (a template, a manifest) answers
  `not_analysed (no IDE language checks this file type)`; `unknown`
  diagnostics point at `ide.context` with `kind: "problems"`.
- A test run stopped with its activation no longer appears on the plate of the
  next activation in the same session; its status still answers by number.
- `ide.stop` accepts an optional `activation_id` instead of refusing it.
- `ide.symbol`/`ide.graph` with a bare Rust function or method name find every
  definition: rust-analyzer's workspace symbol search now covers all symbol
  kinds, not types only.
- `ide.test {"path": …}` for a Rust file runs in that file's package
  (`--manifest-path <package>/Cargo.toml`) instead of building every target in
  the workspace; a file under `src/tests/` is a module, not an integration test.
- A failed Rust test's summary keeps up to four message lines (an assertion's
  `left:`/`right:` values), not just the first.

## 0.9.0 — 2026-10-04

### Added

- Environment selection for Python. Each project root has one current
  environment; the `ide.start` card shows it with its source (selected,
  pinned by `pyrightconfig.json`/`[tool.pyright]`, or discovered), its version
  from `pyvenv.cfg`, and the other candidates with a ready `choose:` hint.
  `ide.start {"environment": {"python": ".venv-py314"}}` switches it
  (`python:<root>` for a nested project root), `auto` resets it. The choice is
  kept per worktree across daemon restarts and is not inherited by a
  recreated worktree. A reader activation cannot change it.
- One resolver answers for the card, the project check, the Pyright session,
  test and format commands and the syntax probe. A change of the environment
  (selection, recreation, disappearance) restarts the Pyright session, reruns
  the check and shows a one-shot line on the plate.
- Test and format commands run from the resolved environment
  (`<venv>/bin/python -m pytest|black|ruff`) without `uv run`; explicit
  commands get the environment's `bin` on `PATH` and `VIRTUAL_ENV`.
  `uv run` stays only when no environment resolves at all.

### Changed

- Suffixed environments (`.venv-py314`) are listed in name order and count as
  a Python project marker on their own. A `pyrightconfig.json` without venv
  keys no longer lets `[tool.pyright]` pin an environment.
- A missing selected or pinned environment is never silently replaced: the
  check, tests, format and probe report the cause and the way out (recreate,
  `auto`, or edit the pin). A broken environment (base interpreter gone) stays
  listed and cannot be selected.

### Known limitations

- Nested project roots with diverging environments run tests and formatting in
  the worktree root's environment; per-root routing comes with Rust toolchain
  selection.

## 0.8.1 — 2026-10-04

### Added

- Reader and writer activations: `ide.start {"read_only": true}` admits any
  number of readers beside the single writer of a worktree; every call that can
  change code (all `ide.edit` forms, `ide.test`) is refused for a reader before
  any work, with one uniform message. A second writer is refused with the
  holder's activation, start time and last activity. `activation_id` is optional.
- `ide.test` explicit commands accept a worktree-relative `cwd` and bounded
  `env`; the 16 KiB limit applies to the whole argv.
- Python projects in nested directories (no root manifest) are discovered and
  listed on the card with checks per root; suffixed environments such as
  `.venv-py314` are found and named; usages cover sibling packages.
- The plugin ships a read-only `ide-reviewer` agent with the IDE read tools.

### Changed

- Edit diagnostics carry `path:line:col` and separate problems the edit
  introduced from those present before (shifted lines stay pre-existing).
- A clean git baseline reads `git <sha> (clean)` on the start card.
- Outlines show declaration lines; the Rust lexical outline names inline
  struct-variant fields.

### Fixed

- Switching a live start between reader and writer roles keeps its durable activation bound to the active host binding; problem checks return the current running state without stalling other queued work; the bundled reviewer starts read-only.
- A fresh read is no longer evicted before the edit that uses it; a stale
  refusal never hints the reference it just refused.
- Formatting an edited Rust module never rewrites its child files (regression
  test).
- The first `ide.start` waits longer for its hook instead of refusing
  `missing_pre`; never-activated sessions are told to start; a stop after lost
  authority answers "nothing active"; worktree resolution failures name their
  stage.
- A missing Python environment is reported once and not re-probed until inputs
  change; import noise collapses to one line. `.tsx` outline and read fall back
  to source when module resolution is unverified.
- Known runner summaries (pytest, cargo test, vitest/jest, go test) are parsed
  whatever launched them; other commands report their exit code and output
  tail; status plates show the starting actor's own run.
- Refusals name the cause and a working next step: reads past the end of a
  file, stageless read failures, `ide.outline` without `path`, old-text edits
  without `source_ref`, duplicate symbol candidates; problems context waits
  briefly for a running check.
- A same-version source reinstall (`scripts/install-local.sh`, `self-install
  --replace`) restages the plugin instead of keeping the previous one.

## 0.8.0 — 2026-10-03

### Changed

- MCP 2026-07-28 ("modern": no `initialize`; every request carries its
  protocol version and client capabilities in `_meta`) is served on
  `rmcp =3.4.0` (from 3.2.0). Modern `tools/list` now returns
  `resultType=complete`, `ttlMs=60000` and `cacheScope=private`, which
  Claude Code requires or it drops every tool; legacy sessions keep their
  exact original wire (no `ttlMs`, `cacheScope` or `resultType`).
- Host identity never depended on the handshake: the Codex-vs-Claude
  envelope, the tool-call correlation and the managed-Claude hook pairing
  are all selected from per-request `_meta`, so modern sessions are
  unchanged. Raw-wire tests pin each decision in both eras (modern
  tools/list, legacy tools/list, modern `server/discover`, a modern
  fail-open `tools/call`, a modern managed-Claude paired call and a modern
  Codex call with `structuredContent`).

## 0.7.0 — 2026-10-02

### Added

- The agent-* family shape: `AGENTS.md`/`CLAUDE.md`, this changelog,
  `SECURITY.md`, `LICENSE` (MIT), `docs/MCP_RESPONSE_STANDARD.md` and
  `docs/FAMILY_CONTRACT.md`, `docs/qualification.md`, `family.toml` with
  `.family/` template provenance, workspace Cargo hygiene (resolver 3,
  `rust-version`, license, shared lints, hoisted dependencies), `deny.toml`
  with a CI supply-chain job and Dependabot, the `xtask` gate
  (`cargo xtask check`, `standard check`, `contract check|update`), the
  exported `schemas/tools.json` tool-contract snapshot with drift tests, and
  SHA-pinned actions with concurrency groups in both workflows.
- The family release flow: `cargo xtask package` (byte-identical to the
  former shell packager) and `package verify` (the release smoke test plus
  the manifest binding), `release prepare` (local version/CHANGELOG edits,
  preview by default), `release manifest`, `release publish` (draft → verify
  → publish, never overwriting) and `release wait` (REL-02 observer, wrapped
  by `scripts/wait-release.sh`). Releases gain the `release-manifest.json`
  and `acceptance.json` assets; the CI product acceptance runs on the
  packaged executable and names the archive's SHA-256.

### Changed

- The release workflow is split into a read-only build job and a tag-only
  publish job (`release` environment); `workflow_dispatch` with `dry_run`
  exercises the build job on any ref. `scripts/package-release.sh` and
  `scripts/release-smoke.sh` are thin wrappers over the xtask commands, and
  `xtask` carries its own version (0.1.0) so a release bump touches only the
  `agent-ide*` packages.
- `initialize` names the product: `serverInfo` is `agent-ide` with its
  version and title `Agent IDE` (it used to report the rmcp SDK).
- Every tool carries truthful MCP annotations: the read tools are read-only,
  `ide.start`, `ide.stop` and `ide.edit` are idempotent by their durable
  receipts, `ide.test` claims nothing beyond the defaults.
- An executed edit, stop or activation whose reply cannot be presented still
  reports its outcome, path, `operation_id` and whether it may be repeated.
- The reply engine registers only the helpers the template uses and runs
  under a fuel budget; paths and messages are escaped so they cannot forge
  reply lines.

## 0.6.9 — 2026-10-01

### Added

- 0.6.7's one-call `ide.read` (several symbols or line ranges in one reply,
  one shared `source_ref`) and the validated `ide.edit` batch form
  (`changes`: 1–32 edits to one file, every address resolved against the
  named source version before anything is written).
- CI-portable tests: the Python gate test finds `python3` on `PATH` instead of
  a developer's local interpreter.

Older versions are listed on
[GitHub Releases](https://github.com/DKotsyuba/agent-ide/releases).
