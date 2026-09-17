# v0.3 feasibility probe P1: service sandbox + reachability

Branch: `probe/v03-service-sandbox`, worktree `agent-ide-worktrees/v03-probe-service-sandbox`,
based on `claude/v0-2-release-prep-69807e`. Time-boxed to 60 minutes. Host: macOS 27.0 (26A428),
Apple Silicon, `sandbox-exec` at `/usr/bin/sandbox-exec`.

Scripts referenced below live in `probes/v03/service-sandbox/`.

## Premise correction (before Q1)

The task brief assumes "The repo already has macOS OS-sandbox execution profiles for providers
(src/execution/ — find the sandbox profile builder, `network_access=false`)". This is **not
true** in the current tree (checked against `git grep` over the full history of `src/execution`
for `sandbox-exec`, `network_access`, `Seatbelt`, `SandboxProfile`, `scfilter`): there is no
`sandbox-exec` profile generator anywhere in the repo. `src/execution/mod.rs:437-511`
(`ValidatedHostInvocation`, `HostSandboxState`) and `src/assistance/claude_worker.rs:69-101`
model and validate **Claude's own** reported Bash-sandbox state (the foreground-helper path
described in `docs/assistance-claude-worker.md`); they do not build an OS sandbox profile to
confine a provider child. So Q1's "apply the existing generator" step is answered as: **there is
nothing to reuse — a v0.3 service would have to author this from scratch.** Everything below was
built standalone for the probe, not adapted from repo code.

## Q1 — macOS sandbox confinement for a long-lived service's provider children

**Setup**: `roots/allowed` (disposable project root), `outside/secret.txt` (sentinel outside any
allowed root), `cache` (private cache dir). Profile template: `profile.sb.tmpl`, rendered by
`run-profile.sh <allowed_root> <cache_root> -- <cmd...>`.

**Blocking discovery**: the first draft profile (`deny default` + explicit `file-read*` /
`file-write*` allows per subpath) made every child process — even `/bin/echo` — die with
`SIGABRT` (exit 134), not a clean permission error. `log show --predicate 'eventMessage contains
"Sandbox"'` showed the real cause:

```
Sandbox: echo(49362) deny(1) sysctl-read security.mac.lockdown_mode_state
Sandbox: echo(49362) deny(1) sysctl-read kern.bootargs
Sandbox: echo(49362) deny(1) file-read-data /
```

On macOS 27, ordinary dynamic binaries read `security.mac.lockdown_mode_state` /
`kern.bootargs` via `sysctl` and stat `/` itself during process bootstrap; denying either is
fatal (`SIGABRT`), not a graceful `EPERM`. Fix, now baked into `profile.sb.tmpl`:

```
(allow sysctl-read)
(allow file-read-data (literal "/"))
(allow file-read-metadata (subpath "/"))
```

This is new OS-version-specific plumbing an implementer would lose hours to without a
`log show` pass — worth flagging to the owner as a blocker for whoever builds this for real
(comment in `profile.sb.tmpl` explains it).

**Results** (commands run via `run-profile.sh`, full transcript in conversation; reproducible by
re-running the script):

| Check | Command | Result |
|---|---|---|
| Read inside allowed root | `cat roots/allowed/inside.txt` | succeeds, prints `inside` |
| Read outside sentinel | `cat outside/secret.txt` | fails: `Operation not permitted` |
| Write outside sentinel | `sh -c 'echo pwned >> outside/secret.txt'` | fails: `Operation not permitted`; file unchanged |
| Write to private cache | `sh -c 'echo cached_ok > cache/wrote.txt'` | succeeds |
| Outbound TCP | `nc -z -G 2 1.1.1.1 443` | fails immediately (`exit=1`, ~0.03s, not a 2s timeout — `deny network*` rejects the syscall rather than blocking) |

**`cargo check --offline` on a tiny crate** (`roots/allowed/tinycrate`), starting from the base
profile (only `roots/allowed` + `cache` writable/readable): failed on
`fopen('/private/etc/ssl/openssl.cnf', 'rb')` (LibreSSL config lookup, triggered even with
`--offline`). Iterating to a working profile (`profile-cargo.sb.tmpl`,
`run-cargo-profile.sh <allowed_root> <cache_root> <rustup_home> <cargo_home> -- <cmd...>`), the
extra paths a confined `cargo check --offline` needs beyond the project root are:

- **read**: `~/.rustup` (toolchain), `~/.cargo` (registry index/cache), `/private/etc` (OpenSSL
  config the LibreSSL/vendored-ssl codepath opens even offline)
- **write**: none beyond the project's own `target/` (already inside the allowed root) for this
  no-new-deps case — a run that needs to *fetch* a new dependency would additionally need
  `~/.cargo/registry` write (tested: `cargo check --offline` with no new deps succeeds without
  it, confirmed by a run with that write rule removed)

**Verdict Q1: works**, with two caveats for the owner: (a) no existing code to reuse — write from
scratch; (b) the macOS-27 bootstrap-syscall allowance above is mandatory or every child aborts,
which will look like "the tool is crashing" rather than "the sandbox is too strict" to whoever
debugs it first.

## Q2 — child/grandchild cleanup after supervisor SIGKILL

**Naive case** (`q2-supervisor-naive.sh`): supervisor forks a child that forks a grandchild
(`sh -c 'sleep 300 & sleep 300'`), no process-group or session isolation. `ps` showed the
supervisor's own `pgid` equal to the *caller's* pgid (backgrounding with plain `&` in a
non-interactive shell does not create a new process group). `kill -9` on the supervisor pid left
all three descendants running (confirmed via `pgrep -fl`) — the well-known "orphans survive"
behavior.

**Mitigation A — process-group kill from a watchdog** (`q2_watchdog_pgroup.py`): watchdog starts
the supervisor with `start_new_session=True` (`setsid(2)`), giving it its own pgid equal to its
own pid; grandchildren spawned by plain `sh -c` inherit that pgid since nothing downstream calls
`setpgid`/`setsid` itself. Watchdog then does `os.killpg(pgid, SIGKILL)` instead of killing just
the supervisor pid. **Result: 0 survivors** — confirmed live.

**Mitigation B — kqueue `EVFILT_PROC`/`NOTE_EXIT` parent-death watch**
(`q2_child_kqueue_watch.py`): macOS has no `PR_SET_PDEATHSIG` (that's Linux-only); the documented
equivalent is for the *child wrapper itself* to register a kqueue event watching its own parent
pid for `NOTE_EXIT`, spawn the real grandchild, then block on the kqueue. When the supervisor is
`SIGKILL`ed (which gives it zero chance to signal anyone), the wrapper's own kqueue independently
fires, and it kills its grandchild. **Result, live 3-level test (supervisor → wrapper → grandchild,
`kill -9` on supervisor only): wrapper log shows**
`wrapper observed parent exit: [...] / wrapper killed grandchild, exiting`, **and the grandchild
pid is gone from `pgrep` afterward.**

**Verdict Q2: works.** Both mitigations are usable without root. Recommendation: use A (process
group + `setsid`) as the primary mechanism for a v0.3 service supervising provider children — it
requires no code inside the child at all, so it also covers third-party binaries (rust-analyzer,
pyright) the service didn't write. B is a good defense-in-depth for a child wrapper the service
*does* control (e.g. its own spawn shim), since it survives even if the child somehow ends up in
a different process group.

## Q3 — reachability from sandboxed Claude Code over a Unix socket

Listener: `q3_echo_listener.py`, one-shot echo server, `/private/tmp/aiv3-probe/s.sock` (30
bytes, well under the ~104-byte `sockaddr_un` path limit). Temp settings:
`settings/allow.json` / `settings/deny.json` (schema copied from the existing, read-only
reference in `~/.claude/settings.json`'s `sandbox` block, not invented).

Two live runs (`claude -p ... --model haiku --strict-mcp-config --settings <file>`), both
`HOME=/Users/pluto`, no `--mcp-config` (so `--strict-mcp-config` yields zero MCP servers, cleanest
possible surface):

| Run | Settings | Prompt result |
|---|---|---|
| 1 | `allow.json` (`allowUnixSockets: ["/private/tmp/aiv3-probe/s.sock"]`) | Claude ran `printf ping \| nc -U .../s.sock`, reported back ```ECHO:ping```; listener log independently shows `received: b'ping'` |
| 2 | `deny.json` (`allowUnixSockets: []`) | Claude reported `Output: (none)`, `Exit status: 1`; listener log shows **no** connection was ever received (only "listening on ...", nothing else) before it was torn down |

**Temporary `--settings` sandbox config is honored** — confirmed both ways (allow lets the
socket through, empty list blocks it at the sandbox layer, not by `nc` failing for some other
reason, since the listener itself never observed a connection attempt in run 2).

**Wildcard/dir allowlist**: not re-tested live (staying at 2 of the 3 allowed runs). The repo's
own `docs/assistance-launcher.md:161-167` already states, as current documented behavior:
"Current Claude Code does not expand a wildcard in this allowlist, so the operator must add the
exact path... `allowAllUnixSockets` is outside this strict profile because it grants access to
unrelated host sockets." Re-deriving this with a third live run would have been redundant with an
existing, current-repo, first-party statement, so I cited it instead of spending the run.

**Verdict Q3: works**, with the constraint (already known to the team, per the doc above) that
v0.3's socket path must be added to `allowUnixSockets` as an **exact string**, one entry per
worktree/session if the service uses per-session sockets — no glob support today.

## Q4 — launchd on-demand service lifecycle

Live-tested the load/unload cycle only (see reasoning below on why full socket-activation
FD-passing was not attempted in the time box):

```
launchctl bootstrap gui/501 /private/tmp/aiv3-probe/launchd/<TEST-LABEL>.plist   # exit 0
launchctl print gui/501/<TEST-LABEL>                                             # shows registered, "not running"
launchctl kickstart -k gui/501/<TEST-LABEL>                                      # exit 0, job ran on demand (KeepAlive=false, RunAtLoad=false)
launchctl bootout gui/501/<TEST-LABEL>                                           # exit 0
launchctl print gui/501/<TEST-LABEL>                                             # "Could not find service ... in domain" -- fully deregistered
```
Plist and log file deleted afterward; no LaunchAgent left registered (verified above).

**Full `Sockets`-key socket activation** (the launchd mechanism where the *first* connection
attempt to a registered socket path launches the job, and the job receives the already-bound fd
via `launch_activate_socket(3)`) was **not live-tested**: doing it for real needs a small
compiled helper (or at minimum a scripting binding) that calls `launch_activate_socket()` to
retrieve the fd launchd handed it — that's not expressible in a shell `ProgramArguments` script,
and writing/compiling+cleaning up a throwaway Swift/C helper safely did not fit the remaining time
budget alongside Q1-Q3. Documented instead:

- **Socket-activation approach**: per-user `LaunchAgent` with a `Sockets` dict (`SockPathName`,
  `SockPathMode` restrictive e.g. `0600`) and no `RunAtLoad`/`KeepAlive`; launchd creates and owns
  the socket file before the service exists, and starts the service on first connect. Service
  binary calls `launch_activate_socket("Listener")` to get the fd. Cleanup story is identical to
  what was just demonstrated (`bootout` + delete plist), plus launchd also removes the socket file
  on `bootout`.
- **Trade-offs vs. socket activation**: requires an installed (not repo-relative) plist under
  `~/Library/LaunchAgents`, i.e. a real install step outside the sandboxed worktree — this is the
  first place in the whole probe that necessarily touches durable, per-user global state, which
  the task explicitly told me not to touch for real; that's exactly why this was documented rather
  than installed for real under the user's actual label namespace.
- **Alternative — first-MCP-client-spawns + lock file**: the MCP thin client, on first connect
  attempt, tries the socket; on `ECONNREFUSED`/missing path, it takes an flock'd lock file, spawns
  the service (`setsid`-detached per Q2's mitigation A), waits for the socket to appear, retries.
  No launchd install needed, works identically across machines/users, but: no automatic restart-on-
  crash (launchd's `KeepAlive` gives that for free), and a thundering-herd of simultaneous first
  clients needs the lock file to serialize spawn attempts (solvable, just extra code the launchd
  path gets for free).

**Verdict Q4: partial** — on-demand load/kickstart/bootout lifecycle confirmed to work safely
under a disposable test label with guaranteed cleanup; true `Sockets`-key FD-passing activation is
documented, not live-verified, because verifying it safely needs a compiled helper and/or a real
`~/Library/LaunchAgents` install, both out of scope for this time box.

## Recommended service confinement + lifecycle for MVP

- **Confinement**: `sandbox-exec` profile per Q1's `profile.sb.tmpl` pattern — `deny default`,
  `deny network*`, explicit read+write on the configured roots array + a private cache dir, plus
  the macOS-27 bootstrap-syscall allowances (`sysctl-read`, `file-read-data "/"`) so children
  don't just abort. Per-provider extra read roots (toolchain/registry dirs) added the same way Q1
  derived them for `cargo check` — expect Pyright/node to need an analogous, separately-derived
  list (not probed here; time-boxed out).
- **Lifecycle**: spawn provider children via a supervisor using Q2 Mitigation A (`setsid` +
  group-kill watchdog) as the baseline; consider layering Mitigation B for children the service's
  own code wraps.
- **Service startup**: for MVP, prefer the "first MCP client spawns + lock file" path over a
  launchd install — it needs no per-user global state and is what the task's constraints already
  push toward; revisit launchd `Sockets` activation once a compiled helper for
  `launch_activate_socket()` exists and there is time to test it with a disposable label the same
  way Q4's plain job was tested here.
- **Reachability**: the MCP thin client and the service must agree on an exact socket path added
  to `sandbox.network.allowUnixSockets` in Claude's settings per Q3 — no wildcard today, so a
  fixed, predictable path per project/worktree (not a fresh random path per session) minimizes how
  often the operator has to edit settings.

## Blockers needing the owner

1. **Confirm the "config array of allowed roots" location/format** for v0.3 — this probe assumed
   a disposable `roots/allowed`-style array item; the real config surface (file? flag? env?) isn't
   specified anywhere in the repo yet.
2. **Decide MVP startup mechanism** (first-client+lock-file vs. launchd install) — Q4 couldn't
   fully resolve this inside the time box; whoever builds v0.3 should budget a follow-up probe
   specifically for the launchd `Sockets`-activation path with a compiled helper if launchd is
   preferred.
3. **Per-provider extra read-root lists** (Pyright/node, gopls, etc., analogous to the cargo list
   in Q1) still need to be derived the same empirical way; only cargo was probed here.

## Cleanup performed

- All `sleep 300`/`sleep 600` test processes, the Q2 Python watchdog/wrapper processes, and both
  Q3 socket listeners were killed; `pgrep -fl` confirms none remain.
- `/private/tmp/aiv3-probe/s.sock` and the Q2/Q3 temp files under `/private/tmp/aiv3-probe/` were
  removed by the scripts themselves (the socket) or by explicit `rm`.
- The Q4 test LaunchAgent was booted out (`launchctl bootout`) and its plist/log deleted;
  `launchctl print` confirms it is not registered.
- Nothing under `~/.claude/settings.json`, `~/.config/agent-ide`, `~/.local/bin/agent-ide`, or any
  real (non-test-label) launchd job was touched.
- `/private/tmp/aiv3-probe/roots`, `/private/tmp/aiv3-probe/outside`, `/private/tmp/aiv3-probe/cache`
  and the tiny `tinycrate` test crate remain on disk (harmless scratch data under `/private/tmp`,
  left in case the owner wants to re-run any check by hand); safe to delete at any time.
