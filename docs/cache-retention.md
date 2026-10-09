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
| D | `providers/<key>/` | the native cache namespace of one language-server launch (see below): `<key>` is the 64-hex BLAKE3 of the worktree state, accepted executable, settings, effective configuration, toolchain and trust; `worktree.path` marker | the leases of the marker's worktree; a namespace without a marker (the shared native namespace of gopls, or one whose marker is not yet published) needs the machine-wide `any` lease exclusively |

Before this version the provider namespaces lived in `/private/tmp/ai-r-*/cache`, out of reach of
every sweep (158 MiB measured) and lost at reboot. A daemon whose state root is unusable still
keeps them there.

## Provider caches

Each retained language-server launch has one namespace `~/.agent-ide/providers/<key>` holding the
subdirectories its server needs (rust-analyzer's build-script and proc-macro `target`, gopls' build
and module caches, …). The key is stable across daemon restarts and reboots because it names the
worktree by its directory — canonical path, inode and creation time, never the device number nor
the per-database identity a new daemon mints afresh — and hashes every input that makes a native
cache unsafe to reuse: the server and the language's own toolchain executables (compiler,
interpreter) by path, identity and measured BLAKE3 digest — a selector such as `stable` or an
unchanged label cannot hide a replaced binary — settings, effective initialization configuration,
toolchain and effective trust. A restarted daemon whose launch declaration is
unchanged therefore finds and adopts the existing namespace; a changed toolchain, setting,
configuration, executable or trust derives another key and starts cold, and the old namespace ages
out below. A worktree deleted and recreated at the same path has a new creation time and a new
key. After the namespaces of a launch are retained, each per-worktree one gets its `worktree.path`
marker (a fresh `0600` file renamed into place), which lets the rules below see the worktree and
claim it through its lease; the shared native namespace has none. `cache status` lists them as
`providers`.

## Policy and defaults

No knobs: the defaults are constants in `retention.rs`. Sizes are allocated bytes
(`st_blocks × 512`) of every entry below the cache directory, symlinks not followed, and are
charged once per family, not once per entry (see *Storage accounting*).

| | Removed when | Default |
|---|---|---|
| A checks | 1. **gone**: the marker's worktree path no longer exists. 2. **idle**: last use older than the idle age. 3. **sessions** and **incremental** (partial, below). 4. **budget**: charged total of all A entries above the budget — least recently used first until the total fits | idle 7 days, incremental idle 2 days, budget 15 GiB |
| D providers | same three rules; a namespace without a marker is only removed by idle or budget, and then only while no lease is held anywhere | idle 14 days, budget 8 GiB |
| B telemetry | same three rules; a directory without a marker (written before this version) is only removed by idle or budget | idle 30 days, budget 1 GiB |
| C releases | a completed release (`COMPLETE` present, directory named `X.Y.Z`) that is **not** the `current` target, **not** one of the 3 newest versions, **not** installed within the last 14 days, and **not** containing the executable of any live process | keep current + newest 3 + 14 days + live |

"Last use" of an A or B entry is the newest modification time among its *stamped* lease files
(see safety rule 1) and every entry in its tree; an empty lease file, created by a sweeper's or
`cache status`'s lock probe, is not a use. mtime only orders entries for idle and LRU; it is never the in-use proof. The idle
and gone rules run first over every entry, then the budget rule over what is left, so a gone
entry is never kept while a recent one is evicted. The budget bounds what can be reclaimed: bytes
held by in-use caches are protected even when they alone exceed it (`cache status` reports them).

## Storage accounting

Sibling worktree caches are APFS copy-on-write clones of one another and a cargo target holds hard
links, so summing allocation per entry charges the same extents again for every clone and link (36 GiB
summed against at most 24 GiB of distinct streams on one machine). A family (A, B or D) is therefore
measured as a whole and `cache status` shows three numbers for it:

- **logical**: the sum of file lengths, every link and clone counted;
- **charged**: allocation with each hard-linked inode and each perfect-clone stream (device and
  APFS clone id, read with `getattrlist`; one inode where the volume reports none) counted once.
  This is what the budget bounds. It is an upper bound of the physical footprint — partially shared
  extents stay overcharged — and directories, symlinks and unshared files count as they are;
- **private**: an estimate of what the volume gets back at once if everything went (APFS private
  bytes summed once per inode, and only for inodes all of whose hard links are inside the family: a
  link outside, for example a file also linked from a build outside the cache, keeps the data
  allocated). It explains reclaim and is never charged: a family of perfect clones
  has almost no private bytes yet still occupies the shared extents.

Removing one entry frees only the streams no surviving entry still holds, so the charge is
recomputed from the survivors after every removal (the verdict's bytes are that freed charge), never
reduced by the removed entry's own size; the budget loop therefore stops as soon as the survivors
fit. Where the attributes are unavailable (another filesystem) hard links are still recognized and
every file is charged by its own inode.

## Check cache tiers

A worktree's check cache loses parts before it is evicted whole. Both steps run under the same
exclusive claim of the worktree lease as a whole removal (busy means in use and nothing is touched,
and no legacy process may be alive):

1. **incremental**: a worktree idle for 2 days loses every rustc `incremental/` directory below
   `<digest>/<language>/target` (renamed into the trash, deleted after the locks are released) and
   keeps `deps/` and `build/` — incremental state was 69–91 % of a worktree's cache while sibling
   dependencies are 88–99 % identical. The price is one slower first check after the break.
2. **sessions**: in any other worktree, finalized sessions
   `incremental/<crate>/s-<timestamp>-<random>-<svh>` older than the newest of their crate are
   removed; rustc loads only the newest and collects the rest itself only when that crate is built
   again. The newest is chosen by the base-36 `<timestamp>` rustc encodes, never by mtime. Each
   session is removed only while its sibling `s-<timestamp>-<random>.lock` — the file rustc
   locks while it reads, writes or collects the session (an `fcntl` lock on macOS, which the `flock`
   taken here excludes) — is held exclusively and without waiting (a busy lock keeps the session), the lock file
   goes last, still held. `-working` sessions and their locks are never touched. A crate directory
   with a malformed `s-…` entry, an unreadable one, a finalized entry that is not a real directory,
   or a tie at the newest timestamp is left entirely alone.

Whole-worktree rules (gone, idle, budget) follow, so a budget eviction only reaches whole worktrees
after every idle worktree's incremental state is already gone. A dry run probes the same locks,
creates no lock file and reports "would remove: checks incremental|sessions".

## Telemetry write-ahead log

Each telemetry store runs in SQLite WAL mode. Its owner connection sets
`journal_size_limit` (1 MiB), so any WAL reset leaves at most that much allocated, and the
telemetry writer runs `PRAGMA wal_checkpoint(TRUNCATE)` on that same owner connection, between
transactions: after a graceful drain (`Telemetry::shutdown`, before the writer releases the store's
lifetime lock) and whenever the writer has been quiet for 30 seconds with frames possibly pending
(also right after opening, which covers a migration or a WAL an earlier owner left). A busy
checkpoint (a reader holds an old snapshot) is not an error and loses nothing: the writer retries
after the next quiet period. WAL and shared-memory files are never unlinked or edited by the
product, and no routine `VACUUM` runs; a store that no process owns is only ever reduced by the
SQLite connection of its next owner or removed whole by the rules above.

## Telemetry marker adoption

Every managed launch publishes the marker of its telemetry store before the daemon is started
(`retention::adopt_marker`, called from `managed_telemetry_database_in`), so no new store waits
behind the machine-wide `any` lease. The store must be exactly
`<state root>/telemetry/<full BLAKE3 hex of the launch directory>`, and all three directories
private; the launch directory's shared lease is held meanwhile (a sweeper's claim locks that lease,
or `any` for a marker-less store, so it cannot run) and the chain and the store's identity are
validated again under it. The marker is a fresh `0600` file synced and renamed over the old one, then
read back: a marker already naming the launch is left alone, a torn one or a historical `0644`
one (0.10.6 wrote them with the umask) is replaced, a symlink, other non-file or marker writable by
others is refused. A live writer is unaffected (its lock is not taken and no SQLite state is
touched). Publishing counts as a use of the store: it touches the directory's mtime, which delays
its idle expiry. When the marker cannot be published the launch does not create a persistent
store: the failed (empty) directory is removed, the daemon starts with its runtime-local telemetry
database, and `errors` shows `daemon unavailable … telemetry_marker_unavailable:<io kind>`. Stores
written before this version and never relaunched stay marker-less and keep the `any` rule above;
nothing guesses their launch directory from the digest.

## Claude hook key hints

The managed Claude MCP leaves one hint directory `/private/tmp/ai-k-<16 hex of the worktree path
digest>` per candidate (`key`, the cached rendezvous key; `candidate-attachment`). The candidate
cannot be recovered from the truncated digest, so a hint is judged only by what it names
(`hook_hints::collect`, run by the hourly sweep under the sweep lock and by `cache prune`;
`cache status` counts what would go). It is removed only when **all** hold, each re-checked under
its exclusive non-blocking directory `flock`, which a publisher (`write_claude_key_cache`,
`write_claude_candidate_attachment`) holds shared while it writes:

- a private (`0700`) real directory of this user, held open without following links and still the
  directory at its path before it is judged and before it is removed, containing only regular
  files of this user named `key`, `candidate-attachment` or a `key-*` temporary;
- the directory and every file older than a day;
- no `key` file (never published), or one whose path is unusable to a hook (not absolute and
  normalized), or one naming a path that definitely does not exist (`NotFound`) **and** whose
  runtime directory `ai-r-<16 hex of the key digest>` does not exist either, so no daemon a hook
  could reach is keyed by it.

A symlink, another owner or mode, an unexpected entry, an unreadable file, any lookup failing for a
reason other than `NotFound` (the key path of a live repository exists), a busy lock or a hint
younger than a day keeps the hint. A hint of a deleted worktree whose repository still exists is
therefore kept: nothing guesses the worktree from the digest. Fronts older than this change publish
without the lock, so the collection is **paused while any live `agent-ide` process is not this
executable or a build proven to lock** (a second file proof, `HINT_LOCK_BUILD_PROOF`, embedded only together with the lock, so a build that merely takes leases does not count; version numbers are not trusted) or the
process list is unreadable; `cache status` and `cache prune` name the blocking processes, and the
sweep records `hook_key_hints paused=N`. Hooks only read hints and never lock.

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
   file (same device and inode), or `standalone/releases/X.Y.Z/agent-ide` with `X.Y.Z` strictly
   newer than 0.9.1 — older than the sweeper or not, so a session started before an upgrade
   never pauses the upgraded sweeper — or is a **proven build**: a regular file that contains the
   lease-protocol proof every build of this source embeds (`LEASE_BUILD_PROOF`) and was not
   modified after the process started (a later rebuild is not the code that runs; the file is also
   re-checked after the scan; a replacement file that keeps an older mtime than the process start
   is the one case not noticed, because the process list offers no inode to compare). A gate or scratch `target/debug/agent-ide` is therefore recognized
   by its contents, never by its path or name, and is listed by `cache status` as a build that
   does not pause eviction. Any other — 0.9.1 or older, an older dev build without the proof, a
   renamed backup, one whose executable was deleted, rebuilt since it started or cannot be read — is legacy, and
   while one is alive **no A or B entry is removed, gone ones and trash included**. The snapshot
   is unknown (and pauses everything the same way) when the process list cannot be read or is
   truncated, or a process other than an exited one cannot be inspected. `cache status` names
   both groups. Eviction starts once old sessions end.
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
