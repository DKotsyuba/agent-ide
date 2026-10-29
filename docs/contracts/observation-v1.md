# Read-only repository observation

`agent-ide observe --repo ABS_PATH --json [--cursor N]` emits exactly one JSON
line, including its newline at most 1 MiB. Invalid arguments/path input exits 2
with a closed error, never raw stderr. A successful observation exits 0 even
when retained evidence is unavailable. It is not a project check or a claim
that the repository is clean.

Success has only `schema_version: 1`, `kind: "ide_observation"`, and `data`.
Failure has only the same discriminator fields and
`error: {code, outcome, detail}`. `outcome` is `blocked`, `unavailable`, or
`outcome_unknown`; `code` and `detail` are producer-owned strings without OS
error messages, command output, or source contents.

The data fields are:

| Field | Meaning |
|---|---|
| `product` | `agent-ide` |
| `package_version` | This CLI's actual compiled package version; no build/provenance inference |
| `received_at_ms` | CLI receipt wall clock, Unix milliseconds, null if unavailable |
| `repository.identity` | Existing rendezvous digest of the canonical Git common directory |
| `repository.common_dir` | Canonical absolute Git common directory, shared by linked worktrees |
| `repository.source_path` | Canonical absolute input directory, distinct from repository identity |
| `instances` | One expected shared repository endpoint observation, including explicit missing/unavailable state |
| `checks` | Bounded retained scheduler page described below |
| `telemetry` | Bounded closed event counts described below |
| `coverage` | `{scope:"shared_repository_daemon", complete:false, reason}` |

An instance contains `instance_id` (an opaque digest scoped to the health-observed
generation, or an unknown-generation endpoint), `product`, `package_version`,
`health` (`healthy` or `unavailable`), `daemon_version` (only a positive health
observation supplies a version; otherwise null), `runtime` (`private`, `missing`,
`unsafe`, or `unavailable`), `endpoint` (`socket`, `missing`, `unexpected`, or
`unavailable`), and `lock` (`held`, `unheld`, `missing`, or `unavailable`).
Health and evidence are separate observations and can race daemon replacement.
No raw generation, executable identity, PID, arguments, launcher attachment,
actor/host metadata, diagnostic text, or credentials enter this projection.

`checks` contains `availability` (`available` or `unavailable`), `snapshots`,
`coverage` (`complete`, `partial`, or `unknown`), and `next_cursor` (row offset
or null). At most 16 summaries are returned, sorted by canonical source path
and registered language. Each summary has `snapshot_id`, `source_path`,
`language`, the existing serialized `CheckState` in `state`, `errors`,
`warnings`, `input_generation`, `observed_at_ms`, `freshness`, and `rechecking`.
`state` is `Ready`, `Partial`, `Checking`, or `{Unavailable: <existing closed
reason>}`. Snapshot IDs are scoped to the instance. Publication time is stored
when the scheduler accepts the result and is never replaced by CLI receipt
time; null means no original time is known. Known newer inputs produce
`freshness:"stale"`; otherwise freshness is `unknown`, because observation never
refreshes files or Git status. `rechecking` reports an already pending/running
check and never schedules one.

The encoded snapshot array is additionally capped at 128 KiB, with continuation
when escaped path bytes reach that ceiling before the row limit.
Coverage describes retained rows, not all repository files or all historical
checks. Empty, pruned, unconfigured, old-daemon, poisoned/busy-owner, malformed,
or missing evidence is unknown/unavailable, never a passing check. Paths are
limited to 4096 UTF-8 bytes. Inspection visits at most 4096 worktrees, 128
languages per worktree, and retains at most 4096 candidate summaries; omitted
or oversized evidence makes coverage unknown. Pagination is best effort over
current retained state, not an atomic history. Passing `next_cursor` back as
`--cursor N` continues that ordering.

`telemetry` contains `availability`, `counts` keyed by the existing six closed
event tags, `coverage`, `next_cursor` (durable sequence or null), `dropped`
(the existing writer's count or null), and `observed_at_ms` (always null because
the existing event schema has no original timestamp). At most the first 100
retained events are counted through the already-open Store owner. Counts
describe this bounded sample, not lifetime usage. Further evidence remains
partial; a standalone observer never opens a database, creates WAL/shm files,
migrates, repairs, records telemetry, or changes retention. A telemetry cursor
belongs to the existing telemetry query command, not the check row offset.

Lookup uses the fixed read-only Git common-directory query under its existing
two-second budget. Health and the fixed observation exchange each have a
two-second total ceiling. Same-user peer validation and private runtime checks
reuse the existing endpoint. The new v1 `observation` branch precedes Assistance
dispatch and does not mark lease activity, admit an actor/job, initialize a
worker, start a daemon/LSP/provider, or run a project check. Old daemons that
do not understand this request yield unavailable evidence. Private per-front
runtimes and removed runtime state are explicitly outside discovery coverage.

`agent-ide doctor --runtime-dir PATH --json` adds a structured
`{schema_version:1,kind:"ide_daemon_diagnostic",data:...}` projection of health,
runtime, endpoint, and lock with CLI and positively observed daemon versions.
Its legacy text form and success/unavailable/error exit statuses are retained.

`agent-ide doctor --read-only` emits the existing installation report plus
`read_only:true`. It shares installation discovery, but suppresses executable
toolchain probes and stale-runtime cleanup/lock creation. The historical
argument-free `doctor` intentionally keeps its abandoned-runtime cleanup and
original JSON/exit behavior. Consumers requiring non-mutation must require
the explicit marker and treat an old CLI's unknown flag as incompatible; they
must never fall back to argument-free `doctor`.

Feature-enabled fixtures use the existing `test_seams::var` API with
`AGENT_IDE_TEST_DOCTOR_SCAN_ROOT` to select exactly one private fixture directory.
An invalid selector scans nothing. Default/release builds ignore this selector;
the gate verifies pure path selection in a default-build child without calling
the mutating report. Production temp/shared roots and cleanup semantics remain
unchanged. Doctor cleanup integration tests explicitly require `test-seams`.

Shared runtimes, key caches and retention reuse the public
`AGENT_IDE_CLAUDE_RENDEZVOUS_ROOT` setting of 0.10.9. Default-build controls
ignore test-only seams while inheriting that configured parent; no test-only
shared-root replacement or canonical-temp gate blocker is required.
