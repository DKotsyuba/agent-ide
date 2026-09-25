# Error log v0.3 contract

Revision: ERRORLOG-r2. Provider: `errorlog`. Direct consumers: Assistance, Checks, Application, the
`errors` CLI reader. Vocabulary: [common](common.md).

## Purpose and relationship to telemetry

[Telemetry](telemetry-v0.2.md) stays a bucketed, restart-only, durable, closed-vocabulary count of
what happened; it deliberately has no per-event detail and no free-form field. The error log exists
because telemetry alone cannot tell a normal Claude pending/helper/inspect round trip apart from a
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
(`start`, `context`, `diff`, `edit`, `inspect`, `stop`, `hook`, `helper_claim`, `check`, `daemon`,
`client`, `feed`); a closed `outcome` tag; an optional closed `reason` tag, which is always the most
specific existing enum variant at the point of failure (for example `BindingUnavailable::MissingPre`,
`FailureCode::InvalidDetail`, or `UnavailableReason::Fatal`, rendered
`missing_pre`/`invalid_detail`/`check_fatal`); an optional `worktree` path; an optional `host` tag
(`claude`/`codex`); an optional `actor` id; an optional `correlation` id; an optional bounded
`detail` string of at most 160 bytes; and an optional `duration_ms`.

`correlation` is the call id / `detail_ref` / activation id the daemon already has for one
operation, so one agent action (hook -> mint -> helper claim -> settle -> inspect) can be followed
across its several log lines by that one opaque id; it is never source text.

`detail` is never source text, file contents, a diff, a helper command line, a prompt, an
environment value, or an arbitrary OS error string. It is only an opaque id already known to the
model, existing already-sanitized checker text (`checks::ProblemSnapshot::detail`, itself capped at
160 bytes), or (for a completed check with no such text) a fixed `errors=<n> warnings=<n>` count
summary. `worktree`, when present, is an absolute path; every other path-shaped fact stays relative
to the worktree, matching what a reply already exposes to the model.

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

T38B: a Claude foreground helper that settles a provider operation as `provider_unavailable` also
logs its own `<method> failed provider_unavailable` event from the helper process (the helper
initializes the same per-repository log from its runtime directory). Its `detail` names exactly
which accepted-provider condition failed, from this closed vocabulary:
`provider_spawn_refused:<go|rust|python|typescript>`, `provider_child_exit:<go|rust|python|
typescript>`, and `provider_session_failed:<go|rust|python|typescript>`. Only these class names and
closed provider tags are ever recorded — never paths, digests, commands, or OS error strings.

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
