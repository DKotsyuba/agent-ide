# Trusted product launcher configuration

Set `AGENT_IDE_LAUNCHER_CONFIG` only in the daemon launch environment. The daemon reads
that file once under a 64 KiB limit. Changes take effect only after restart. It must be
maintained by the trusted launcher/operator, separately from MCP arguments and hook data.
A target path is never itself Workspace authority.

An optional top-level `bridge` object selects a protected local Bridge deployment:

```json
{
  "registration_socket": "/absolute/private-bridge/adapter.sock",
  "core_socket": "/absolute/private-bridge/core.sock",
  "credential_file": "/absolute/private-bridge/producer.token",
  "native_stores": [{"harness":"codex","host":"local","store":"owned-codex"}]
}
```

Paths must be normalized absolute paths with real owner-only current-user parents. The
credential is a no-follow owner-only regular file, at most 16 KiB. Each process reads it
once; there is no ambient credential discovery. SDK calls use the standalone client's finite
five-second exchange baseline; Bridge owns the separate native handoff deadline after custody.
`native_stores` accepts at most two unique harnesses (`codex`, `claude_code`), each with
an explicit Bridge host/store namespace. The actor comes only from the independently
validated IDE binding, never model arguments, cwd or parent-session guesses.

A nonempty mapping selects Bridge-only delivery: IDE has no fallback carrier, wake or Stop hold.
Graph always accepts a job, including cached results; tests return their run id without
waiting for the child. Useful result/error bodies go to Bridge with Auto/KeepAll. Admission
requires explicit direct-address, Auto, result-kind, size and delivery-scope capabilities.
These capabilities are not a claim of an idle receiver's native wake support: that requires
separate qualification for the actual executable/home/launch. Current Bridge stock Codex
CLI queue acceptance does not prove passive model resumption. Do not enable a production
cutover from a declared profile or custody receipt alone.

Before possible SDK writes IDE commits immutable destination/body/sender-owner intent. Test
facts deduplicate by run; independent graph jobs include their result reference. Unknown
outcomes never create another send or carrier. Stop/displacement retires the boot/binding
scope; restart retires only retained intents belonging to the same service/credential owner.
A mismatched owner disables recovery instead of silently rehoming possible effects. Custody
is not model observation and does not consume the saved domain result.

Source-bearing bodies recheck current workspace/binding, no-follow source bytes and recorded
path disclosure before publication; superseded results carry an explicit terminal outcome.
Bridge does not grant IDE source access. Results describe the captured source revision, not
an assurance that source will remain current until eventual receipt.

Empty/omitted `native_stores` configures no Bridge delivery: IDE has no fallback carrier, so
results of deferred work stay retained under their `detail_ref` and are read explicitly with
`ide.inspect`.
No global profile, installed service or operator grant is changed by building the candidate.

The IDE's only path policy is the top-level `allowed_roots` list: `ide.start` admits its working
directory (the optional `root` parameter, else the target `candidate`), the discovered Git
worktree root and the Git common directory only when each lies below a configured root, and
refuses with `outside_allowed_roots` otherwise (an empty list refuses every activation). Every
absolute path a provider resolves outside the worktree passes the same containment check. The
IDE replays no host sandbox and keeps no list of accepted sandbox profiles; its own child
processes (language servers, Git, project checks) run as ordinary processes of the daemon's user.

The closed version-one JSON shape is:

```json
{
  "version": 1,
  "limits": {
    "queued": 16,
    "details": 64,
    "operation_ms": 120000,
    "output_bytes": 65536
  },
  "targets": [
    {
      "attachment": "opaque-launcher-channel",
      "candidate": "/absolute/candidate-worktree",
      "git": {
        "path": "/absolute/git",
        "identity": "accepted-binary-identity",
        "blake3": "64-hex-digit-accepted-executable-digest"
      },
      "providers": []
    }
  ],
  "allowed_roots": ["/absolute/projects"]
}
```

The digest placeholder above must be replaced by the actual measured executable (`agent-ide
evidence executable --identity <id> <path>`); it is a deliberately invalid configuration value.
Each distinct configured executable fingerprint is checked once in a cancellable blocking
startup task before the worker becomes ready. Selected executable bytes must remain
immutable for that daemon boot; changes require restart and fresh verification.

Configurations written for 0.3.16 and earlier may still carry `codex`, `cwd_trampoline`,
`profiles`, and `allow_disabled_host` — and, from the retired Claude helper, `claude_profile` —
on a target; they are accepted and ignored, because no
host sandbox is replayed any more. New configurations should omit them.

Each optional provider has `executable` in the same shape as `git`, a closed `settings`
value, `toolchain`, `trust`, and `cache_namespace`. A provider entry with the retired `gopls_defaults` settings (Go support was removed in 0.10.8) is
ignored before validation: the daemon starts normally, journals one `provider_settings_retired:gopls_defaults` line per
start, and `agent-ide doctor` prints one `retired_provider` line; any other unknown settings value is still invalid.
`rust_cache_priming_disabled_v1` requires a nonempty rustup toolchain selector plus accepted
nonempty `cargo_version` and `rustc_version` identities. Arbitrary settings objects and
duplicate language/settings entries are rejected.
`pyright_defaults_v1` supports Codex and Claude and requires `node` as an accepted executable object whose
identity exactly matches `toolchain`, with all Rust executable/version fields absent/null. The
configured Node program runs the accepted absolute `pyright-langserver` script directly; only
Node's parent directory and the private temporary directory are retained in its environment.
The daemon rechecks accepted script and Node identities at spawn.
`typescript_defaults_v1` requires accepted `node` and bridge executable objects plus a
`typescript` object containing `bridge_bytes`, the exact bridge and TypeScript versions, an
accepted `tsserver` file (`path`, `blake3`, and `bytes`), a sorted nonempty `closure` of files in
that same shape, and separate host evidence fields. The compiled Codex release cell accepts only
`macos-26.6.2-node-24.4.0-tls-6.0.0-ts-5.9.3-codex-r3-2026-09-14:<bundle-digest>`: the suffix is
derived from the exact declared Node, bridge, `tsserver.js`, and closure paths, digests, lengths,
and identities, so copying the record to another bundle is rejected. The three public versions
must also be exactly 24.4.0, 6.0.0, and 5.9.3, and the compiled record fixes the accepted Node,
bridge, `tsserver.js`, and complete closure byte identities rather than trusting those labels alone.
The separate Claude cell accepts only
`macos-26.6.2-node-24.4.0-tls-6.0.0-ts-5.9.3-claude-r3-2026-09-14:<bundle-digest>`.
Its suffix is the same exact declared-bundle digest as the Codex record, while its distinct prefix records independent host acceptance. Every member is remeasured at startup
and immediately before its one-shot child starts.
`cache_namespace` is a bounded compatibility label, not a filesystem path or
authority grant. Assistance combines it with the verified durable worktree and
accepted provider identities to retain a private directory, then supplies only
that derived directory through the provider's cleared environment. Configuration
cannot select `HOME` or another writable host path.
Claude provider processes run one-shot in the daemon and are reaped there. Directory retention does not claim a
surviving backend or proven warm opaque provider index.

Limits are explicit: 1–64 queued operations, 1–128 retained details, 1–300000 ms per
operation, and 1–1048576 retained bytes per output stream. There are at most 64 distinct
attachment mappings and four provider languages per target. Duplicate attachment mappings,
unknown fields, invalid limits, relative paths, and malformed executable digests are rejected.

The fourth provider is the immutable `TypeScriptProviderBundleV1` in
[TYPESCRIPT-r3](contracts/intelligence-v0.2.md). It accepts only an explicit accepted Node,
bridge, TypeScript closure, and `tsserver.path`; it has no ambient npm/plugin/network discovery.
Codex and Claude require their separate exact bundle-bound compiled release records. Claude runs
the closed frame only through the daemon; a null Claude record keeps that host unavailable without affecting Codex. This is restart-only configuration and never enables a syntax
server or automatic typing acquisition.

For a configured TypeScript-family document, the observed ancestor `tsconfig.json` or `jsconfig.json`
must explicitly admit the current document through a bounded top-level `files` entry or a literal
`include` file/directory entry (wildcard `include` and `exclude` are refused), and must set
`compilerOptions.types` to `[]` or exactly `["vite/client"]` and `compilerOptions.moduleResolution`
to `node10` or `bundler` (a `module` of `node16`/`nodenext`/`preserve` is refused). JavaScript and
JSX documents additionally require `compilerOptions.allowJs` to be `true`; the closed diagnostic
options `checkJs` and `noImplicitAny`, when present, must also be `true`. Every `files`/`include`
entry must be a normalized relative UTF-8 path without glob metacharacters, absolute paths,
dot/parent components, or backslashes; duplicates are rejected, the observed input set is bounded
by the existing 16-entry resolution limit, and at least one config is required (inferred projects
are unsupported). A config may instead declare only `references` (with an empty `files` and no
other options) and route to its sibling projects one level deep; nested `references` and
`extends`/`typeAcquisition` are unsupported. Compiler output options `outDir` and `declarationDir`
are rejected. `package.json` may declare dependencies but not `imports` or `workspaces`; package
resolution is confined to real `node_modules` directories below the worktree, and ambient ancestor
`node_modules` entries are refused. Membership is taken only from these exact observed entries; no
glob, output-path, or inferred membership is scanned.

Configuration contains private attachment and evidence values. Diagnostic formatting
redacts the configuration; it must never be rendered in model-facing tool results.

## Allowed roots (the only path policy)

`allowed_roots` is the complete permission model of the IDE. It is host-neutral: Codex in any
sandbox mode, Codex through agent-run or the desktop application, and Claude Code all pass the
same check, and a host upgrade or a changed sandbox mode changes nothing. The daemon never reads
`codex/sandbox-state-meta`, and there is nothing to accept, capture, or re-mint.

- `ide.start` resolves its working directory — the optional absolute `root` argument, else the
  target `candidate` — with `std::fs::canonicalize` and requires it to be equal to or below one
  canonicalized configured root. The canonical spelling is what Git discovery then runs in.
- After discovery, the Git worktree root (Git may resolve a candidate to a repository above it)
  and the Git common directory (a linked worktree's `.git` may point elsewhere) must both lie
  below a configured root; neither implicitly authorizes the other location.
- Provider resolution inputs outside the worktree (ancestor `tsconfig.json`, `node_modules`,
  dependencies) are admitted with `admit_path`: a path that does not exist yet is judged by its
  deepest existing ancestor plus the remaining components; relative paths and `.`/`..` segments
  are refused.
- Any failure, and an empty `allowed_roots`, answers `error: outside_allowed_roots` with a stage
  tag and a hint to start in an allowed directory or extend the list. The error log records the
  same `outside_allowed_roots` reason.

What the list does not do: it does not confine the language servers' or Git's own reads, and it
does not stop a child from writing where the daemon's user may write. That is deliberate — the
agent already has its own shell, so a second sandbox around the IDE protected nothing and only
made the IDE refuse.

## Confined project checks (EYES-r2)

Two optional top-level fields extend the configuration for the v0.3 project problem feed. They
follow the same rules as every other field: restart-only, validated at load, and the whole
configuration is rejected on any malformed value.

- `allowed_roots`: 0..=16 absolute, normalized directory roots (no `..`, no trailing `/`); the
  same list gates activation (above). A worktree is admitted for checks when its canonical path is
  equal to or below a canonicalized root; otherwise every language state is `outside allowed roots`.
- `project_checks`: optional timing and language declarations. Presence together with a nonempty
  `allowed_roots` enables the feed; absence, or empty roots, keeps v0.2 behaviour unchanged.
  - `debounce_ms` 100..=10000 (default 1500), `idle_timeout_s` 30..=3600 (default 300),
    `check_timeout_s` 10..=900 (default 300).
  - `rust`: `toolchain_dir` (required, absolute, normalized), the rustup toolchain that runs
    `cargo check --workspace --all-targets --message-format=json --offline --locked`. EYES-r2 also
    defines the optional `rust.cargo_home` override, which defaults to `$HOME/.cargo` (the rustup
    home is derived from `toolchain_dir`, never configured); T05B adds the optional
    `rust.developer_dir` override for the Apple developer directory a native build script's
    `cc`/`xcrun` invocation needs, which defaults to the `/usr/bin/xcode-select -p` resolution
    (falling back to `/Applications/Xcode.app/Contents/Developer` then
    `/Library/Developer/CommandLineTools`); the shipped example fragment
    [docs/examples/launcher-eyes.json](examples/launcher-eyes.json) omits both optional fields
    while the schema accepts exactly `toolchain_dir`, `cargo_home`, and `developer_dir` and
    rejects unknown fields.
  - `python`: `node` and `pyright_cli` (both required, absolute, normalized), the accepted Node
    executable and the Pyright CLI entry module it runs. The project's own interpreter is located
    inside the worktree; checks and the Pyright language server share the same interpreter resolver,
    and the server receives that interpreter as `python.pythonPath`. A root `.venv` applies to
    nested Python files even when the repository has no root Python manifest. With no resolved
    interpreter, the server keeps its existing defaults; a missing check environment reports
    `environment not found` rather than the flood of unresolved imports it would produce.
  - A language subsection absent means that language is never checked and never appears in the
    `<agent-ide>` block.

### Matching the project compiler and runtime

A wrapper's inherited PATH does not select the background TypeScript compiler. Declare
`project_checks.typescript.node` and `tsc_cli` explicitly; the latter should name the prepared
project's own `node_modules/typescript/lib/tsc.js`, not an unrelated global installation.
Both paths must be absolute and normalized. Run native checks from the same worktree with
that Node and compiler, the same root tsconfig and the same dependency snapshot. Resolve
package-manager shims before comparing the actual tools; a global version recorded in a
manifest does not prove what a project-first PATH executes.

The current confined adapter supports TypeScript5.9.3 and the semantic provider has its own
separate release-qualified5.9.3 bundle. Pointing a check at6.x does not qualify that provider:
an unsupported compiler is unavailable with an explicit version-mismatch cause, never a
clean zero and never a silent fallback. Changing supported versions requires separate adapter
and provider qualification. For a project requiring `^5.9.2`, a prepared5.9.3 dependency is
compatible; a6.x seed from another checkout is not the same project environment.

For reproducible launches, prepare dependencies from the intended project's lock or qualified
snapshot; compare native/project/IDE compiler version and library bytes, the actual Node,
working directory and configuration before admitting model work. Keep the selected identities
in the run evidence. A mismatch must stop preparation rather than ask the agent to change
project configuration to satisfy a different compiler.

Checks rebuild a safe allowlisted environment and retain their read-only/no-network profile.
They intentionally do not copy ambient credentials, writable HOME, arbitrary shell hooks,
package-manager auto-installation or another process's complete environment. Explicit
`ide.test command` has its own cwd/env; use the project's exact prepared command there.
Matching semantic inputs does not mean removing confinement or inheriting every shell value.

Every check process runs confined through one `sandbox-exec` profile: default-deny, no network,
read-only access to the admitted worktree, reads limited to the declared toolchain directories and
system paths, and write access only to the check's private cache directory and private temporary
directory. The environment is rebuilt from an allowlist (toolchain binaries, `HOME`, the private
temp directory, `CARGO_TARGET_DIR`, `CARGO_NET_OFFLINE=true`); ambient credentials are not passed.
For Rust, T06B adds `CC`/`CXX`/`SDKROOT` plus `CARGO_TARGET_<TRIPLE>_LINKER` (or `RUSTFLAGS`)
pointing at the resolved developer directory's own `clang`, bypassing the `/usr/bin/cc` `xcrun`
shim that a build script's link step cannot run under this profile; see EYES-r2 §3 for the exact
resolution.
One environment limit is handled, not hidden: when the daemon itself is already confined by the
host (an agent's own sandboxed session), macOS refuses to apply a nested profile —
`sandbox-exec: sandbox_apply: Operation not permitted`, a non-zero exit with no checker output.
The runner detects that refusal, runs the same check once without our profile — so such a
session still gets its checks — and remembers the refusal for the daemon's lifetime so later
checks never retry the doomed wrapper. Those checks run under the host's confinement only: the
product's read-only, no-network, private-cache profile does not apply to them, and whatever the
host allows (project writes, network) the check and its `build.rs` may do. A check that fails for any other reason
keeps its own cause: the snapshot detail carries the first `error:` line of stderr, else its
first non-empty line, else `exit <status>`, so a failed check always says why.
Each check owns one process group, killed whole on cancel, timeout (`check_timeout_s`), or daemon
shutdown; no pattern-based kill touches processes the daemon did not start.

Check caches live outside every runtime directory, under
`$HOME/.agent-ide/checks/<16 hex blake3(repository key)>/<16 hex blake3(canonical worktree)>/<policy digest>/<language>`
(mode `0700`), so they survive daemon idle stops and crashes and caches from different policies
never collide; at daemon start, cache directories
whose recorded worktree no longer exists are removed. A new worktree's Rust cache is cloned
copy-on-write from the most recently completed sibling worktree of the same repository when
possible.

Lifecycle: the first MCP server that finds no live daemon spawns one shared per-repository daemon
(runtime directory `/private/tmp/ai-r-<16 hex of the rendezvous key digest>`); later MCP servers of
the same repository adopt it after validation, and an MCP exit never stops it. The hook path reads
the repository key from the MCP-cached hint `/private/tmp/ai-k-<16 hex of the worktree path
digest>` and never runs `git` itself. Each MCP startup atomically refreshes that hint before
serving calls, so a stale hint cannot send the first hook to an earlier daemon generation.
Unresolved managed hooks stay silent to Claude and record a closed, path-free error-log detail.
With zero open leases and no running check the daemon stops after `idle_timeout_s`, closes its
socket, and removes its runtime directory; the caches remain.

`AGENT_IDE_CLAUDE_RENDEZVOUS_ROOT` can replace the default temporary parent for the shared runtime,
key hints and manual or automatic hint collection. Supply the same existing absolute normalized
directory to MCP servers, hooks and daemons. Invalid relative, traversing or unresolved overrides
refuse runtime resolution and manual collection; automatic collection skips them without using
the default parent. Unset or empty overrides retain `/private/tmp`. Keep the parent short enough
for Unix socket paths. This operator setting does not change private child ownership, no-follow
checks, identity fencing or locking.

Scheduling: triggers are a successful `ide.start`, Claude post hooks for `Edit`, `Write`,
`MultiEdit`, `NotebookEdit`, and `Bash`, and a completed `ide.edit`. A trigger that names the
changed file — a writer post's retained `tool_input.file_path` (`notebook_path` for
`NotebookEdit`), the edited path of `ide.edit` — makes that file's language the forced one (an
edit reply waits only for it) while every other configured language keeps the ordinary
fingerprint-gated re-arm, so a `.py` edit never forces a cargo check and a cross-language input
still re-checks its consumer once the worktree fingerprint changed; a path no
registered language owns (`Cargo.toml`, `README.md`) and a path-less trigger (`Bash`, `ide.start`)
re-arm every configured language. Per `(worktree, language)`,
runs are debounced by `debounce_ms`, at most one runs at a time, and at most two run concurrently
across the daemon.

Results: one bounded snapshot per `(worktree, language)` carries state, error/warning counts, and
a bounded problem list. The compact `<agent-ide>` block (at most 256 bytes) is emitted to the
owning Claude actor only when its items change; unavailable states use fixed phrases, and failed
checks add a bounded reason when the checker reported one (or say no reason was reported), e.g.
`check failed (cannot start compiler)`. A running check renders
`checking (first check)` or `checking (files changed; last result: N errors, M warnings)`.
`ide.context` with
`{"kind":"problems","language"?,"offset"?}` returns counts and up to 20 problems per call with
`next_offset`. One `ProjectCheckCompleted` telemetry event (bucketed counts, no paths or messages)
extends the TELEMETRY-r1 event scope.

## Experimental Jev support pilot

Build with `--features jev-pilot` to include the optional client. The same executable
supports restart-only `jev_pilot.mode` values `off` (default) and `jev`. Existing
checks and the ten public tools are unchanged. A malformed or incomplete helper
section disables only the helper and records a fixed safe operator reason.

```json
"jev_pilot": {
  "mode": "jev",
  "route": "opencode",
  "task": "Review the changed behavior using observed project evidence.",
  "credential_file": "/absolute/private/key-file",
  "egress_allowed": true,
  "initial_scope": ["src/example.ts"],
  "trace_dir": "/absolute/private/existing-trace-directory"
}
```

`route` selects exactly `opencode` (default: OpenCode System One,
`jev-1.13-free`) or `typesafe` (`https://api.typesafe.ai/v1/systemone`,
`jev-1.13.0`). There are no custom endpoints, models, retries or provider fallbacks.
The key file must be canonical, regular, owned by the current user, inaccessible
to group/world and at most 4096 bytes. The existing trace directory must be
canonical, owner-only and private. OFF never opens a key, constructs HTTP or sends.
The task is nonempty and at most 1024 UTF-8 bytes. Scope paths must be relative;
only current writer authority can admit their bounded private source reads.
Those reads never become main-agent source receipts.

One real confirmed edit starts a 250 ms coalesced optional episode. Native changes
qualify only after normal MCP authorization/reconciliation. The episode binds
exact task, activation, binding generation, source versions and change epoch.
Verified edit geometry supplies a bounded window around the actual changed byte;
missing geometry is explicitly unavailable. Scope alternatives remain selectable.
Only previously observed or explicitly scoped files are read: at most eight unique
files and 2 MiB aggregate bytes including freshness rereads, under the existing
1 MiB per-file reader ceiling.
Model state is at most 8 KiB; options at most twelve; ready graph borrowing is
currently unavailable (zero graph requests). Source and relationship coverage is
explicitly partial. No new provider warming or source index is introduced.

At most two decisions, including test choice, run within five seconds of compute;
each HTTP exchange has a 1.5-second total deadline. The helper may inspect, expand
or reopen existing snapshots, suggest a next-natural-reply review, remain inactive,
or choose one existing core-admitted test. The run option names the actual observed
test target; source-to-test relation is unverified. Without an admitted recipe no
automatic test is offered. Choice confidence must be at least 0.5 and an independent
usefulness Noul at least 0.8. A test uses existing admission, output/replay retention
and canonical completion delivery, within min(recipe timeout, 30 seconds). Same-root
busy is exclusion only; foreign output is never reused or cancelled. Explicit main
tests cancel only their exact binding's optional work and wait for actual cleanup;
a delay above two seconds is recorded as a violation. Stop/shutdown cancel and join
owned optional work. Started results remain historical when source changes.

Private JSONL records retain exact state, ordered options and requests without auth,
bounded redacted responses, transport timing/status/bytes, reported usage, per-field
token coverage and dated tariff estimates. Missing/interrupted usage and actual
provider charges remain unknown. A durable exclusive task/worktree claim and a
flushed pre-send pending record prevent restart replay from hiding attempted work.
Changing the task text creates a new task identity. Trace files contain private
project context and are not sent to the main agent or ordinary logs.
