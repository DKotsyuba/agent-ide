# Error log v0.3 contract

Revision: ERRORLOG-r1. Provider: `errorlog`. Direct consumers: Assistance, Checks, Application, the
`errors` CLI reader. Vocabulary: [common](common.md).

## Purpose and relationship to telemetry

[Telemetry](telemetry-v0.2.md) stays a bucketed, restart-only, durable, closed-vocabulary count of
what happened; it deliberately has no per-event detail and no free-form field. The error log exists
because telemetry alone cannot tell a normal Claude pending/helper/inspect round trip apart from a
real failure, and cannot say *why* a real failure happened. It is a separate, plain, rotated
append-only file rather than SQLite so it can be read with no daemon running and with none of
telemetry's exclusive-writer or migration machinery.

## What is logged and where

The daemon, and the MCP client process for client-side failures (re-establishment, transport
unavailable), append one JSON line per non-success tool-call outcome and a small set of lifecycle
facts (daemon start/stop/idle-exit) to
`~/.agent-ide/logs/<repository-key>/events.jsonl`. `<repository-key>` is the same sixteen lowercase
hex character id an `ai-r-<id>` runtime directory and `~/.agent-ide/checks/<id>` already use, so one
repository's worktrees share one log directory. The directory is `0700` and the file is `0600`.

A `pending` reply (a Claude foreground-helper round trip is required) and a `completed` reply are
never written to the error log: only telemetry's `pending`/`completed` outcomes record those.

## Closed vocabulary

Every field is one of: an RFC 3339 UTC timestamp; a closed `method` tag (`start`, `context`,
`diff`, `edit`, `inspect`, `stop`, `hook`, `helper_claim`, `check`, `daemon`); a closed `outcome`
tag; an optional closed `reason` tag, which is always the most specific existing enum variant at
the point of failure (for example `BindingUnavailable::MissingPre`, `FailureCode::InvalidDetail`,
or `UnavailableReason::Fatal`, rendered `missing_pre`/`invalid_detail`/`check_fatal`); an optional
`worktree` path; an optional `host` tag (`claude`/`codex`); an optional `actor` id; and an optional
bounded `detail` string of at most 160 bytes.

`detail` is never source text, file contents, a diff, a helper command line, a prompt, an
environment value, or an arbitrary OS error string. It is only an opaque id already known to the
model (a `detail_ref`/activation id/correlation id) or existing already-sanitized checker text
(`checks::ProblemSnapshot::detail`, itself capped at 160 bytes). `worktree`, when present, is an
absolute path; every other path-shaped fact stays relative to the worktree, matching what a reply
already exposes to the model.

## Failure semantics

Writing is fail-open and nonblocking: a missing or unwritable directory, a rotation failure, or a
write failure drops only that one line and never blocks, delays, retries, or changes the
originating tool call, hook, or daemon lifecycle event. The critical section is one bounded
open-check-write per event under a short-held lock; there is no background writer or channel.

## Rotation

The file rotates to `events.jsonl.1` (overwriting any prior generation) once it exceeds 5 MiB, then
a fresh `events.jsonl` starts. Exactly one prior generation is kept; nothing older survives a second
rotation. Unlike telemetry, this is a hard local disk bound, not a row/byte-accurate ledger: an event
straddling the rotation boundary always lands in the *new* file.

## Reading

`agent-ide errors [--repo <path>] [--since <minutes>] [--limit <n>] [--summary]` reads
`events.jsonl.1` then `events.jsonl` for the repository key derived from `--repo` (default: the
current directory), independent of whether a daemon is running. Malformed or partial lines are
skipped rather than aborting the read. Without `--summary` it prints the most recent `--limit`
(default 200) events, oldest first, as `time method outcome reason worktree detail` (`-` for an
absent field). With `--summary` it prints `(method, outcome, reason)` groups and their counts,
most frequent first.

## Examples

`{"ts":"2026-09-18T09:12:04Z","method":"start","outcome":"unavailable","reason":"missing_pre","host":"claude"}`
records a `BindingUnavailable::MissingPre` refusal collapsed into the generic
`MissingPeer::HostBinding` reply the model actually receives.

`{"ts":"2026-09-18T09:12:05Z","method":"inspect","outcome":"failed","reason":"source_unavailable"}`
records an `ide.inspect` call that failed with `FailureCode::SourceUnavailable`; no detail
reference, source text, or path is present.

## Gates

Unit tests prove the writer rotates at the size ceiling and keeps exactly one prior generation,
creates `0700`/`0600` state, and never panics on an unwritable directory; reason-code mapping tests
prove at least `BindingUnavailable::MissingPre`, `FailureCode::InvalidDetail`, and a check `Fatal`
snapshot with a `detail` map to their exact closed tags. A binary-level test exercises
`agent-ide errors --summary` against a fixture log. No gate proves product adoption at every call
site listed in the T107 brief; some sites (client re-establishment, the MCP oversize-envelope
boundary) are not yet wired and are called out as left undone in that task's report.
