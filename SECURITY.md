# Security boundary

`agent-ide` is a resident stdio MCP server for Codex and Claude Code. It reads
and edits source files in Git worktrees the launcher configuration names, runs
bounded language servers and project checks as the local user, and keeps local
state under `~/.agent-ide`. It is a local tool: no network client of its own,
no telemetry leaves the machine, and it never handles credentials, tokens or
secrets — host metadata that reaches it (for example the opaque
`AGENT_IDE_HOST_ATTACHMENT` transport handle) is treated as attribution and
routing hints, never as authentication, and is redacted from diagnostics.

## Trust boundary

- The only filesystem policy is the trusted launcher configuration's
  `allowed_roots` list (`~/.config/agent-ide/launcher.json`, restart-only via
  `AGENT_IDE_LAUNCHER_CONFIG`): `ide.start` is admitted when the working
  directory — and, when Git discovers one, the worktree and its Git directory —
  lies inside a configured root; everything else refuses
  `outside_allowed_roots`. Tool arguments are bounded and validated
  (relative paths only, no `..`, byte/entry ceilings, closed vocabularies)
  before they reach the filesystem, a language server or a subprocess.
- Project checks run confined under `sandbox-exec`: no network, a read-only
  worktree, reads limited to the declared toolchains, a private cache and
  temp, and an allowlisted environment. Their process groups are killed on
  cancel or timeout.
- Host adapter files (`.claude-plugin/`, `.codex-plugin/`, `hooks/`) are
  pointers to the installed binary; the installed `claude-hook.sh` accepts
  only an absolute `AGENT_IDE_BIN` and never searches `PATH`, so an update
  cannot split MCP and hook versions.

## Shared rendezvous directories

The resident daemon and hooks rendezvous in `/private/tmp/ai-r-<hash>`
(keyed by the canonical Git common directory, shared by every worktree of one
repository); MCP leaves its hook key in `/private/tmp/ai-k-<hash>` so hooks
never run `git`. A rendezvous directory is used only when it is a real
directory, owned by the effective user, with no group/other permission bits
(`0o700` at creation); anything else — a symlink, a foreign owner, loose
modes — is refused as unsafe. Every accepted IPC connection re-checks the
peer's UID through the socket credentials and drops mismatches. A daemon lock
is a `0o600` flock'd file owned by the effective user; a leftover socket is
retired only while that exclusive lock is held and only after a connect probe
proves nothing listens on it. `agent-ide doctor` reports runtime entries
older than 24 hours whose socket answers no probe as stale. An MCP exit never
stops a healthy shared daemon; it idles out after `idle_timeout_s` and
removes only its own runtime directory.

## Refusal and honesty rules

Business refusals are `isError` replies with closed reason codes, never
fabricated success; an effect that could not be confirmed is reported as
`outcome_unknown` with `isError=false`. Edits are idempotent per
`operation_id` and validated against the exact source version they name;
nothing is written when any address in a batch fails to resolve. stdout of
`mcp` is protocol-only.

## Reporting

Report vulnerabilities privately through GitHub security advisories for
`DKotsyuba/agent-ide`, or contact the maintainer directly. Do not open a
public issue with exploit details.
