# Error log v0.3 contract

Revision: ERRORLOG-r3. Provider: `errorlog`. Direct consumers: Assistance, Checks, Application, the
`errors` CLI reader. Vocabulary: [common](common.md).

## Purpose and relationship to telemetry

[Telemetry](telemetry-v0.2.md) stays a bucketed, restart-only, durable, closed-vocabulary count of
what happened; it deliberately has no per-event detail and no free-form field. The error log exists
because telemetry alone cannot tell a normal Claude pending/inspect round trip apart from a
real failure, cannot say *why* a real failure happened, and cannot follow one agent operation across
several calls. It is a separate, plain, rotated append-only file rather than SQLite so it can be
read with no daemon running and with none of telemetry's exclusive-writer or migration machinery.

## What is logged and where

The daemon, and the MCP client process for client-side facts (re-establishment, transport
unavailable), append one JSON line per tool-call completion and per lifecycle fact (daemon
start/stop/idle-exit, client lease open/close, a completed project check) to
`~/.agent-ide/logs/<repository-key>/events.jsonl`. `<repository-key>` is the same sixteen lowercase
hex character id an `ai-r-<id>` runtime directory and `~/.agent-ide/checks/<id>` already use, so one
repository's worktrees share one log directory. The directory is `0700` and the file is `0600`.

`~` is the real user home from the password database (`getpwuid_r`), **never** `$HOME`: hosts such
as `agent-run` start MCP clients, daemons and shells with a substituted `HOME`, which used to split
one repository's log across several homes. The only override is the absolute-path environment
variable `AGENT_IDE_HOME` (tests and explicit relocation); it moves every per-user root
(`.agent-ide/logs`, `.agent-ide/checks`, telemetry, and the cargo/rustup homes of the Rust check).
`.cargo/config.toml` sets it to `target/test-home` for every `cargo test` process, and spawned
daemons inherit it, so tests never write under the real home.

A managed Codex daemon runs in a random `ai-<random>` runtime directory that names no repository,
so the spawning MCP passes `AGENT_IDE_LOG_KEY=<repository-key>`, which wins over the runtime
directory name. The reader tries, in order, the key from `git rev-parse --git-common-dir`, the key
from reading `.git` files itself (when `git` fails or times out), and the canonical path itself, and
uses the first whose log directory exists.

r2 (T107 full-logging extension) logs *every* call and lifecycle fact, not only a non-success one:
each line carries a closed `level` (`error`/`warn`/`info`), a pure function of `outcome`
([`Outcome::level`]), so a successful `ide.start` and a `pending` round trip are on the record too,
not just their failures. `agent-ide errors` still defaults to showing only `warn`/`error`; `--all`
opts into `info` as well.

## Closed vocabulary

Every field is one of: an RFC 3339 UTC timestamp; a closed `level` tag; a closed `method` tag
(`start`, `context`, `diff`, `edit`, `outline`, `read`, `symbol`, `graph`, `test`, `inspect`,
`stop`, `hook`, `check`, `daemon`, `client`, `feed`); a closed `outcome` tag; an optional closed
`reason` tag, which is always the most
specific existing enum variant at the point of failure (for example `BindingUnavailable::MissingPre`,
`FailureCode::InvalidDetail`, or `UnavailableReason::Fatal`, rendered
`missing_pre`/`invalid_detail`/`check_fatal`); an optional `worktree` path; an optional `host` tag
(`claude`/`codex`); an optional `actor` id; an optional `correlation` id; an optional bounded
`detail` string of at most 160 bytes; and an optional `duration_ms`.

`correlation` is the call id / `detail_ref` / activation id the daemon already has for one
operation, so one agent action (hook -> mint -> settle -> inspect) can be followed
across its several log lines by that one opaque id; it is never source text.

`detail` is never source text, file contents, a diff, a command line, a prompt, an
environment value, or an arbitrary OS error string. It is only an opaque id already known to the
model, existing already-sanitized checker text (`checks::ProblemSnapshot::detail`, itself capped at
160 bytes), or (for a completed check with no such text) a fixed `errors=<n> warnings=<n>` count
summary. `worktree`, when present, is an absolute path; every other path-shaped fact stays relative
to the worktree, matching what a reply already exposes to the model.

A panic is the one fact whose natural text is unsafe, so it is journaled by *location only*: the
daemon installs a panic hook (`errorlog::install_panic_hook`) that writes `method` `daemon`,
`outcome` `failed`, `reason` `internal` with `detail` `panic at <file>:<line>:<column>` (the
crate-relative source path the compiler recorded, or only the file name when it is absolute) and
never the payload text, which can carry paths, source or secrets. A panic inside one job is caught
by the worker (the call answers `internal`, the worker keeps serving) and journaled once under the
call's own `method` and `correlation` with `detail` `panic at <file>:<line>:<column> during
<method>`; the hook leaves those to the catcher so one panic is one line.

Every typed tool reply leaving the dispatcher is logged once from the dispatcher itself
(`adapters::log_tool_reply`), independently of whether the durable telemetry sink is available:
that sink is absent whenever its lock is contended or its initialization failed, for example while a
replaced daemon generation is still shutting down, and the log must not go silent then. The same
holds for completed checks. Also logged: `check started` (scheduler dispatch, language in
`detail`), client re-establishment (`client reestablished`, or `client unavailable
provider_unavailable`), an oversize reply envelope (`client failed oversize_envelope`), and each
emitted `<agent-ide>` feed block (`feed completed`, `detail` = `languages=<list> bytes=<n>`, no
text).

A reply without its own `detail_ref` (every error) is correlated by the `detail_ref` the call
passed, so a failed retrieval names the result it asked for. A native hook that advances a
binding's native epoch logs `hook completed` with the hook's call id as `correlation` and `detail`
= `native_hint phase=<Post|PostFailure|PostBatch> tool=<host tool name or ->`. An `ide.inspect`
refused because that epoch advanced after its Context/Diff result was captured logs an extra
`inspect failed source_unavailable` with `detail` = `native_epoch_advanced`, distinguishing it from
the other `source_unavailable` causes that share the same reply.

T24B: each `execution_profile` conversion site on the activation path also logs its own
`<method> failed execution_profile` event (in addition to the dispatcher's reply event) whose
`detail` names exactly which condition failed, from this closed vocabulary: `git_policy`,
`query_policy`, `spawn:<process error variant>`, and `git_unsupported`. Only closed variant names
are ever recorded — never paths or OS error strings. The same closed cause appears in
agent-facing `execution_profile` refusal text. An activation whose root, discovered worktree
root or Git common directory is not below a configured allowed root is refused with reason
`outside_allowed_roots`; the log never names the path.

## r3: complete journal (stability W1-C)

Revision ERRORLOG-r3 adds closed fields and records so every fault can be attributed and the daily
fault report (`cargo xtask fault-report`, below) can count every call once. Older lines stay valid:
every new field is optional on read.

Fields (all closed values or opaque ids; never source text, a path, a model-chosen name or a
request value):

- `version` — the serving product version (`CARGO_PKG_VERSION`), on every dispatch, front and
  daemon-failure line. Its presence marks a line of the current format.
- `role` — `reader`, `writer`, or `none` (the call has no activation). Read before the call ran, so
  a stop keeps its role.
- `language` — the registered language id of the file the request names (by extension alone),
  `unknown` for a file no registered language owns, `none` when the request names no file.
- `form` — the request form: the names of the parameters the tool defines that the call carries,
  in the tool's order, joined by `+` (`path+lines`, `symbol`, `kind`); `none` for a request with no
  defined parameter. A model-chosen parameter name is never written.
- `request` — the host call id (`toolUseId` / `callId`), unique per call, shared by the front's
  line, the daemon's dispatch line and the line of the job the call queued. (The JSON-RPC id
  restarts per MCP front and is not used.)
- `origin` — on an inspection's line, the `request` of the call that queued the inspected result,
  beside the inspection's own `request`; `correlation` stays the result's `detail_ref`.
- `delivered` — on the line of a call that retrieves a retained result by `detail_ref`: `true`
  when the inspection path itself delivered that result to the caller (a cached failed result is
  delivered too), `false` for a refused retrieval (stale authority, expired or unknown reference, a
  host-binding refusal). The report's notion of "collected" is this typed evidence, never failure
  prose or the method spelling.
- `probe` — `whois` on the line of the front's private actor query, which is the product's own
  probe and not an agent's call; the report excludes it from its counts.
- `eligible` — `true` when the request validated, `false` when it was refused as input.
- `host` — `claude`, `codex` or `unknown`.

Every dispatch line (`log_tool_reply`) and every front line carries `version`, `host`, `role`,
`language`, `form`, `request` and `eligible`; a value the call could not name is the explicit
`unknown` / `none`, never an absent field.

New records:

- **Degraded success** — outcome `degraded` (a `warn`): the call succeeded through a weaker path,
  either the lexical context or outline fallback (the worker marks the call, the reply text is
  never scanned) or an edit whose post-edit diagnostics are `unknown`.
- **Completion record** — a job that finishes after its caller was told `pending` writes one line
  with `detail` `pending_completion`, its `correlation` (the result reference), the job's `request`
  and its own classified outcome and reason (a refused or unknown edit is not a success). A failed
  job keeps its existing job-failure line, which now carries `request` too.
- **Front transport outcome** — a call whose front outcome carries no typed reply (invalid input,
  missing host metadata, transport unavailable, timed out, busy, outcome unknown, not
  re-established, incomplete, refused as restarting by a failed daemon) writes one line with
  `detail` `front:<kind>` and the call's `request`, because the daemon never saw it or could not
  answer.
- **Refused hook** — a hook the daemon refuses on a channel that already holds a binding writes
  one per-call `hook` warn, `detail` `hook_refused:<cause>`, `correlation` the call id, from the
  one place every refusal exit passes through; on a channel that never bound anything it stays
  rate-limited bookkeeping. The `claude-hook` process writes one per-call warn
  `hook_submit_timeout` when its submission outlived its own 250 ms budget (a lost pre; the call it
  belongs to is then refused `missing_pre`), and one per-call warn
  `hook_cwd_rerouted:<reason>` when a pre paired only through `CLAUDE_PROJECT_DIR` because the
  payload cwd found no rendezvous of the session (QW-8), and `hook_project_dir_invalid` when a
  present `CLAUDE_PROJECT_DIR` cannot be resolved (the pre is then dropped locally).
- **Daemon failure** — `daemon failed` carries `detail` `<stage>:<class>` (stage `initialize`,
  `serving` or `shutdown`; class an application error name or `io:<ErrorKind>`).

Typed causes: no ingress exit of the dispatcher answers a bare `unavailable: host_binding`
(`invalid_metadata`, `missing_field`, `invalid_parameters`, `unsupported_hook_phase`, `mismatch`,
`invalid_attachment`, `internal_lock`, `worker_unavailable`), and a store failure outside stop is
`capacity (store:busy | store:store_full)`, `deadline (store:store_deadline)` or the sole fallback
`store:unavailable`, on reads, activation, inspection re-authorization and environment choices.

## Daily fault report

`cargo xtask fault-report [--root DIR] [--since DAY] [--until DAY|RFC3339] [--days N] [--scope
field|test|all] [--alert-threshold PERCENT] [--min-calls N] [--unexplained-threshold PERCENT]`
replaces the `errstats.py` counter with the stability plan's section (b) taxonomy. It reads
`<root>/<key>/events.jsonl` (+ `.1`) or flat `<key>.jsonl` copies (`--root`, else
`AGENT_IDE_LOG_ROOT`, else `$AGENT_IDE_HOME/.agent-ide/logs`, else `$HOME/.agent-ide/logs`; a host
that substitutes `HOME` passes `--root`), for the last `--days` (default 7) or an explicit window
(`--since`/`--until` are validated calendar days or UTC instants, percentages must be finite and in
`0..=100`). An unreadable journal fails the command, and journal lines that are not JSON are skipped
but counted and disclosed in the header, so a damaged input never looks healthy. A failure line of
the current format honors its `eligible` flag (refused as input is the caller's) and a reason the
rules do not know is `unexplained`, never dropped from the fault numerator. The unit is the terminal dispatch line of a tool call; `pending` replies are excluded; an
uncollected pending job's completion record (or current-format failure line) and a front line no
daemon line shares a call id with each count once. Every failed call is one class of four kinds:
`fault` (known mechanism), `unexplained` (not attributable, counted with faults), `caller`
(wrong request) and `honest` (correct refusal of real state). It prints per-day and per-class
tables (known versus unexplained), splits by method and repository, the longest outage streak,
pending settlement, daemon starts, failures and panics, and hook warns. `--scope field` (default)
excludes keys whose every worktree is a gate, matrix or acceptance clone.

Alerts (exit status 1): the IDE-fault rate above `--alert-threshold` with at least `--min-calls`
(default 300) calls, and any journaled panic. Warnings (printed, exit 0): unexplained faults above
`--unexplained-threshold` (default 0.1%, an instrumentation backlog that stays in the numerator)
and a fault class that doubled day over day with at least ten events. On the frozen section (b)
input (`target/stability/journals`, window 2026-09-28 .. 2026-10-07T19:30:00Z, scope field) it
reproduces 16,379 terminal calls, 692 IDE faults (500 known + 192 unexplained, 4.22%), 491 caller
mistakes, 163 honest refusals and 727 pending replies.

## Failure semantics

Writing is fail-open and nonblocking: a missing or unwritable directory, a rotation failure, or a
write failure drops only that one line and never blocks, delays, retries, or changes the
originating tool call, hook, or daemon lifecycle event. The critical section is one bounded
open-check-write per event under a short-held lock; there is no background writer or channel.

## Rotation

The file rotates to `events.jsonl.1` (overwriting any prior generation) once it exceeds 20 MiB
(raised from 5 MiB in r1 once every call, not only a failure, is logged), then a fresh
`events.jsonl` starts. Exactly one prior generation is kept; nothing older survives a second
rotation. Unlike telemetry, this is a hard local disk bound, not a row/byte-accurate ledger: an
event straddling the rotation boundary always lands in the *new* file.

## Reading

`agent-ide errors [--repo <path>] [--since <minutes>] [--limit <n>] [--summary] [--all]` reads
`events.jsonl.1` then `events.jsonl` for the repository key derived from `--repo` (default: the
current directory), independent of whether a daemon is running. Malformed or partial lines are
skipped rather than aborting the read. By default only `warn`/`error` events are shown; `--all`
also includes `info`. Without `--summary` it prints the most recent `--limit` (default 200) events,
oldest first, as `time level method outcome reason worktree detail` (`-` for an absent field). With
`--summary` it prints `(level, method, outcome, reason)` groups and their counts, most frequent
first. A line written before `level` existed (r1) is read back as `warn`, since every event logged
under r1 was already a non-success outcome.

## Examples

`{"ts":"2026-09-18T09:12:04Z","level":"warn","method":"start","outcome":"unavailable","reason":"missing_pre","host":"claude"}`
records a `BindingUnavailable::MissingPre` refusal collapsed into the generic
`MissingPeer::HostBinding` reply the model actually receives.

`{"ts":"2026-09-18T09:12:05Z","level":"error","method":"inspect","outcome":"failed","reason":"source_unavailable","correlation":"detail-ref-9"}`
records an `ide.inspect` call that failed with `FailureCode::SourceUnavailable`; `correlation`
lets it be matched against the earlier call that minted `detail-ref-9`, and no source text or path
is present.

`{"ts":"2026-09-18T09:12:06Z","level":"info","method":"start","outcome":"completed","correlation":"detail-ref-9","duration_ms":42}`
records an ordinary successful `ide.start` under r2's full-logging extension.

## Gates

Unit tests prove the writer rotates at the size ceiling and keeps exactly one prior generation,
creates `0700`/`0600` state, and never panics on an unwritable directory; reason-code mapping tests
prove at least `BindingUnavailable::MissingPre`, `FailureCode::InvalidDetail`, and a check `Fatal`
snapshot with a `detail` map to their exact closed tags; `Outcome::level` is proved a pure,
exhaustive function of `Outcome`; a level-default-on-read test proves an r1-shaped line without
`level` reads back as `warn`. A binary-level test exercises `agent-ide errors --summary` against a
fixture log. `tests/error_log_contract.rs` proves daemon restarts append rather than truncate, that
`errors --repo` finds the log from the repository, a linked worktree and a subdirectory under a
substituted `$HOME`, that `AGENT_IDE_LOG_KEY` names a random runtime's log, and that a spawned daemon
never writes under the real home; `tests/error_log_replies.rs` proves each typed failure reply
(including `source_too_large`) and `check started` are logged with no telemetry.
