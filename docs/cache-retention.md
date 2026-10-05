# Cache retention

Agent IDE keeps per-user state below `~/.agent-ide` (the real home from the password database,
or `AGENT_IDE_HOME`; a substituted `$HOME` never moves it). Every daemon on the machine shares it.
This note is the policy for removing what is no longer needed, automatically and without ever
removing something a live process uses. The code is `crates/agent-ide-core/src/retention.rs`.

## Caches

| | Path | What one entry is | In-use proof |
|---|---|---|---|
| A | `checks/<repo16>/<worktree16>/` | the project-check build cache of one worktree (`<policy digest>/<language>/{target,tmp}` plus the `worktree.path` marker) | worktree lease |
| B | `telemetry/<digest>/` | the telemetry store of one MCP launch directory (`state.sqlite*`, `worktree.path` marker from this version on) | the store's own writer lock `state.sqlite.lock` (held by its daemon for the daemon's whole life) and the leases of the launch directory and of every ancestor, all locked by a claim (so an activated worktree keeps the stores of its subdirectories); a store without a marker (written before this version) could belong to any worktree, so its claim needs the machine-wide `any` lease exclusively — it goes only while no lease is held anywhere |
| C | `standalone/releases/<X.Y.Z>/` | one installed release | `current` symlink, live-process snapshot, install lock |
| D | `/private/tmp/ai-r-*/cache` | provider cache namespaces of one daemon | **out of scope** |

D is excluded: 158 MiB measured, outside `~/.agent-ide`, exclusive to one daemon, cleared at
reboot; a sweeper never traverses another daemon's runtime directory.

## Policy and defaults

No knobs: the defaults are constants in `retention.rs`. Sizes are allocated bytes
(`st_blocks × 512`) of every entry below the cache directory, symlinks not followed. A cache
cloned from a sibling with APFS copy-on-write shares extents with it, so "freed" is an upper bound
of what the volume gets back.

| | Removed when | Default |
|---|---|---|
| A checks | 1. **gone**: the marker's worktree path no longer exists. 2. **idle**: last use older than the idle age. 3. **budget**: total of all A entries above the budget — least recently used first until the total fits | idle 7 days, budget 15 GiB |
| B telemetry | same three rules; a directory without a marker (written before this version) is only removed by idle or budget | idle 30 days, budget 1 GiB |
| C releases | a completed release (`COMPLETE` present, directory named `X.Y.Z`) that is **not** the `current` target, **not** one of the 3 newest versions, **not** installed within the last 14 days, and **not** containing the executable of any live process | keep current + newest 3 + 14 days + live |

"Last use" of an A or B entry is the newest modification time among its *stamped* lease files
(see safety rule 1) and every entry in its tree; an empty lease file, created by a sweeper's or
`cache status`'s lock probe, is not a use. mtime only orders entries for idle and LRU; it is never the in-use proof. The idle
and gone rules run first over every entry, then the budget rule over what is left, so a gone
entry is never kept while a recent one is evicted. The budget bounds what can be reclaimed: bytes
held by in-use caches are protected even when they alone exceed it (`cache status` reports them).

## When

Each long-lived daemon runs a background task: the first sweep 60 seconds after start (so a
starting session's activation holds its lease first), then every hour, on a blocking thread.
A sweep takes the non-blocking exclusive lock `~/.agent-ide/locks/retention.lock`; a daemon that
does not get it, or finds the lock file touched less than 55 minutes ago (it is touched after
every completed sweep), skips this round. So the machine runs about one sweep an hour however many
daemons run, and a failed or skipped sweep is retried an hour later. Errors never remove anything
(safety rule 6).

## Safety rules

1. **Worktree lease.** `~/.agent-ide/locks/<worktree16>.lock` (`worktree16` = the 16-hex
   BLAKE3 prefix of the canonical worktree path as text — the same key as the A directory
   name) is a stable small file that is never unlinked; the first lease taken on it stamps it
   with `leased`, so a file that is still empty was only created by a lock probe and its mtime
   never counts as use, neither for last use nor for the claim's "used since the scan" check.
   A daemon holds a *shared* `flock` on it:
   - for every activation, from before the durable activation is committed until the binding is
     released after a committed revoke (`ide.stop`) or the daemon exits — a paused activation
     keeps holding it;
   - for every project check, from before the cache directory is prepared until the checker
     returns; during the copy-on-write clone it also holds the sibling source's lease, and
     skips the clone (the check builds cold) when that lease cannot be taken;
   - for every `ide.test` run, from before spawn until the child is settled, independent of
     `ide.stop`.

   A check, clone or test whose future is dropped before it settled (cancellation, shutdown)
   may leave a child running, so its lease is deliberately leaked and lives until the daemon
   process exits (daemons idle-exit). There is no timed grace.
   Every lease also holds `~/.agent-ide/locks/any.lock` shared (see the telemetry row above).
   Taking a lease touches the worktree lock file's mtime, and so does releasing it.
   **No lease, no use:** when a lease cannot be taken (lock file unopenable, unsafe parent,
   descriptor exhaustion) `ide.start` is refused (`start:cache_lease`), the check run stores an
   `unavailable` result instead of running, and `ide.test` refuses to start. The native host
   tools are never affected.
   Lock files are opened without following symlinks, must be regular files owned by the
   effective user, and their parent and grandparent must be real directories owned by the
   effective user that no group or other user can write.
2. **Atomic claim.** A sweeper removes an A or B entry only while holding an *exclusive
   non-blocking* `flock` on its lease (and, for B, on `state.sqlite.lock`); busy means in use and
   the entry is kept. Under the exclusive lock it re-checks that the lock file was not touched
   after the scan, renames the directory into `<cache>/.trash/<name>-<pid>-<nanos>` (same volume,
   unique), releases the lock and only then deletes the trash, so a half-deleted tree is never
   reused and a shared waiter blocks for a rename, not a multi-GiB delete. Leftover trash of a
   crashed sweep is deleted by the next sweep (sweeps are serialized by `retention.lock`).
   The telemetry writer re-checks after locking that its lock file is still the one at the path,
   so it cannot own a lock file that was just moved to trash.
3. **Legacy processes.** 0.9.1 and older do not take leases. Before the scan and again
   immediately before every A/B claim, the sweeper takes one snapshot of the effective user's
   live processes whose name starts with `agent-ide` (`proc_listallpids`, `proc_pidinfo`,
   `proc_pidpath`). Such a process *participates* only if its executable is the sweeper's own
   file (same device and inode) or `standalone/releases/X.Y.Z/agent-ide` with `X.Y.Z` strictly
   newer than 0.9.1 — older than the sweeper or not, so a session started before an upgrade
   never pauses the upgraded sweeper. Any other — 0.9.1 or older, a dev build, a renamed
   backup, one whose executable was deleted or cannot be read — is legacy, and
   while one is alive **no A or B entry is removed, gone ones and trash included**. The snapshot
   is unknown (and pauses everything the same way) when the process list cannot be read or is
   truncated, or a process other than an exited one cannot be inspected. `cache status` names them. Eviction starts once old sessions end.
   Invariant this relies on: every published release after 0.9.1 keeps the lease protocol.
4. **Releases.** Release evaluation — reading `current`, listing, recovery and removal, and the
   dry run too — holds `standalone/.install.lock` exclusively and non-blocking (an install in
   progress skips the round), so `current` cannot move meanwhile. A candidate is renamed to
   `releases/.trash-<X.Y.Z>-<pid>`, a second snapshot is taken, and if it is unknown or any live
   executable lies under the original or the renamed path the release is renamed back;
   otherwise it is deleted. An unknown first snapshot (including any `agent-ide` process whose
   executable path is unreadable) or an incomplete listing keeps every release, and so does an
   unreadable release tree. A quarantine left by a crashed sweep is restored when a live
   executable lies under either name, else deleted (applied sweeps only).
5. **Boundaries.** Only `~/.agent-ide/{checks,telemetry,standalone/releases}` are traversed.
   `~/.agent-ide`, each cache root, `standalone`, `releases` and every trash directory must be a
   real directory owned by the effective user that no group or other user can write, checked
   before anything below it is listed, recovered or removed; entries that are not real
   directories and names of the wrong shape (dot-names included) are skipped; symlinks are never
   followed. Lock files are never removed.
6. **Errors keep.** An unreadable or invalid marker (a missing one is a legacy directory, judged
   by idle and budget only), a marker path whose lookup fails for any reason other than
   `NotFound`, an unreadable `current` link (every release kept), an entry whose tree cannot be fully read (reported `kept, unreadable`), a
   failed lock or rename — each keeps the entry. A family whose listing was incomplete is
   reported so and gets no budget eviction. Bytes reported freed are measured: a delete that
   fails part-way reports only what it provably removed (nothing when the remainder cannot be
   measured) and leaves the rest in the trash for the next sweep, which deletes and records it as
   `leftover`.

Remaining limits, by consequence: a historical binary started by hand between the snapshot and a
claim could build into an A cache that is being renamed (one failed or cold check, no user data);
a self-installed development bundle numbered above 0.9.1 that predates this protocol would be
taken as participating. Both need deliberate manual action.

## Visibility

- `agent-ide cache status` — dry run: per-cache totals and budgets, every entry the policy would
  remove with its reason and size, entries protected because they are in use, and whether
  eviction is paused by legacy processes (with their pids and executables). Removes nothing.
- `agent-ide cache prune` — runs the same sweep now (ignoring the hourly stamp, still honoring
  every lock and rule), prints what it removed and records each removal in the error log of the
  current directory's repository (the log `agent-ide errors` reads there).
- Every removal by a daemon is recorded in that daemon's error log
  (`~/.agent-ide/logs/<repo>/events.jsonl`, read with `agent-ide errors --all`) as method
  `retention`, outcome `completed`, the worktree (or removed path) and detail
  `<checks|telemetry|release> <gone|idle|budget|superseded|leftover> freed=<bytes>`; a sweep
  paused by legacy or unknown processes records outcome `skipped` with detail
  `legacy=<count|unknown>`.
