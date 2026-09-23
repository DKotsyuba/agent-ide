# Trusted product launcher configuration

Set `AGENT_IDE_LAUNCHER_CONFIG` only in the daemon launch environment. The daemon reads
that file once under a 64 KiB limit. Changes take effect only after restart. It must be
maintained by the trusted launcher/operator, separately from MCP arguments and hook data.
A target path or sandbox observation is never itself Workspace authority.

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
      "codex": {
        "path": "/absolute/codex",
        "identity": "accepted-wrapper-identity",
        "blake3": "64-hex-digit-accepted-executable-digest"
      },
      "cwd_trampoline": null,
      "providers": [],
      "profiles": [
        {
          "record": {"Execution-owned": "complete accepted PersistedProfileRecord"},
          "sandbox_state": {"Execution-owned": "exact supporting measured state"}
        }
      ],
      "allow_disabled_host": false,
      "claude_profile": null
    }
  ]
}
```

The descriptive digest/profile placeholders above must be replaced by actual accepted
execution evidence; they are deliberately invalid configuration values. Configuration
loading does not fabricate D03 evidence. Complete profile records are parsed by Execution
and must match the accompanying accepted state before rebuilding its catalog. A current
invocation still needs its own exact host-correlated state and fresh binding liveness.
Each distinct configured executable fingerprint is checked once in a cancellable blocking
startup task before the worker becomes ready. Selected executable bytes must remain
immutable for that daemon boot; changes require restart and fresh verification.

`cwd_trampoline` is optional and defaults to absent. Its only accepted `path` is
`/usr/bin/env`; any other path is rejected rather than accepted as a wrapper script. Declare it
only when this target's worktree differs from the `sandboxCwd` a managed host reports — a native
child inherits its parent's `sandboxCwd`, so the utility is then run as
`env -C <worktree> <program> <args>` *inside* the unchanged `codex sandbox` argv. The replayed
sandbox state is never rewritten and no permission is widened: the wrapping is admitted only when
the observed managed profile already grants read of the whole filesystem root. Its fingerprint is
verified at startup like every other configured executable and rechecked immediately before spawn.
Absent, a differing worktree simply stays unavailable, and same-cwd argv is unchanged. The
contract is supported on macOS; other platforms report the case unavailable.

Each optional provider has `executable` in the same shape as `git`, a closed `settings`
value, `toolchain`, `trust`, and `cache_namespace`. `gopls_defaults` requires an absolute
Go executable as `toolchain` and absent/null `cargo_version` and `rustc_version`.
`rust_cache_priming_disabled_v1` requires a nonempty rustup toolchain selector plus accepted
nonempty `cargo_version` and `rustc_version` identities. Arbitrary settings objects and
duplicate language/settings entries are rejected.
`pyright_defaults_v1` supports Codex and Claude foreground helpers and requires `node` as an accepted executable object whose
identity exactly matches `toolchain`, with all Rust executable/version fields absent/null. The
configured Node program runs the accepted absolute `pyright-langserver` script directly; only
Node's parent directory and the private temporary directory are retained in its environment.
Claude reconstructs the fixed profile from only these launcher-accepted script and Node paths,
identities, and BLAKE3 digests, then rechecks the script before the inherited-process Node
recheck at spawn. This does not change the five MCP tools; helper protocol revision 3 fences
mixed binaries.
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
Its suffix is the same exact declared-bundle digest as the Codex record, while its distinct prefix
records independent host acceptance. The field may remain null for a Codex-only target; any
non-null copied or mismatched record rejects the launcher. Every member is remeasured at startup
and immediately before its one-shot child starts.
`cache_namespace` is a bounded compatibility label, not a filesystem path or
authority grant. Assistance combines it with the verified durable worktree and
accepted provider identities to retain a private directory, then supplies only
that derived directory through the provider's cleared environment. Configuration
cannot select `HOME` or another writable host path.
Claude helpers receive the same retained worktree directory after durable Start, but each
provider process is one-shot and reaped with the helper. Directory retention does not claim a
surviving backend or proven warm opaque provider index.

Limits are explicit: 1–64 queued operations, 1–128 retained details, 1–300000 ms per
operation, and 1–1048576 retained bytes per output stream. There are at most 64 distinct
attachment mappings, four provider languages per target and eight accepted Execution
profiles across the two supported profile classes. Duplicate attachment mappings, unknown
fields, invalid limits, relative paths, malformed executable digests, duplicate profile
digests, and mismatched profile evidence are rejected.

The fourth provider is the immutable `TypeScriptProviderBundleV1` in
[TYPESCRIPT-r3](contracts/intelligence-v0.2.md). It accepts only an explicit accepted Node,
bridge, TypeScript closure, and `tsserver.path`; it has no ambient npm/plugin/network discovery.
Codex and Claude require their separate exact bundle-bound compiled release records. Claude runs
the closed frame only through its claimed foreground helper; a null Claude record keeps that host
unavailable without affecting Codex. This is restart-only configuration and never enables a syntax
server or automatic typing acquisition.

For a configured TypeScript-family document, the observed ancestor `tsconfig.json` or `jsconfig.json`
must explicitly set `compilerOptions.types` to `[]` and `compilerOptions.moduleResolution` to
`node10`. JavaScript and JSX documents additionally require `compilerOptions.allowJs` to be `true`;
the closed diagnostic options `checkJs` and `noImplicitAny`, when present, must also be `true`.
The top-level `files` array is required, bounded by the existing 16-entry resolution limit, and
must list the current document exactly once relative to the config directory. Every entry must be a
normalized relative UTF-8 path without glob metacharacters, absolute paths, dot/parent components,
or backslashes; duplicates are rejected. Top-level `include` and `exclude`, compiler output options
`outDir` and `declarationDir`, and dependency graphs (`extends`, `references`, package
dependencies/workspaces, or ancestor `node_modules`) are unsupported and rejected. Membership is
taken only from this exact observed array; no glob, output-path, or dependency membership is
inferred or scanned.

Configuration contains private attachment and evidence values. Diagnostic formatting
redacts the configuration; it must never be rendered in model-facing tool results.

## Accepting a new Codex sandbox mode

A Codex host is admitted only when a live sandbox state is authorized by an operator-accepted
profile. A read-only and a workspace-write Codex sandbox are both class `managed`, so several
profiles of one class may be needed; a target may list up to eight profiles whose identities all
differ. Records are versioned by `shape_version` (T35B): an absent field is the legacy v1
layout, whose exact-digest admission is unchanged byte-for-byte and never silently upgraded, and
`shape_version: 2` carries the conservative shape-based admission: a live state is admitted when
one accepted template proves it a *narrower authority* than the accepted capture — the same
mechanism, the same positive selectors with access equal or reduced, every accepted deny
restriction still present, network equal or reduced, and the same glob expansion settings.
Everything else is refused; unknown or unsupported state shapes never fall back to a looser
comparison. Because outside write roots are preserved exactly in the shape, a new outside write
selector (for example a visualization directory a host added) always requires re-acceptance, and
a state that grants write of the whole filesystem root is refused by every workspace-write
template. The error log names the closed reason: `no_profile_for_class:<class>`,
`profile_digest_mismatch:<class>` (v1-only catalogs), `shape_unsupported:<class>` (the live state
derives no v2 shape), or `shape_not_narrower:<class>` (no accepted template proves it narrower).

When the daemon refuses a profile, it captures the raw observed state once per distinct digest
under `~/.agent-ide/rejected-profiles/<16-hex>.json` (mode 0600 at creation; the directory is
created 0700; the 16-capture limit is a soft cap — concurrent daemons can transiently exceed it).
The capture is skipped silently when the directory is a symlink, is not owned by the current
user, or carries group/other permission bits; the error-log detail then reads
`profile_digest_mismatch:managed; capture_skipped:io`. No capture is ever written for an
observed state that carries any top-level field beyond the four documented
`codex/sandbox-state-meta` fields (`permissionProfile`, `codexLinuxSandboxExe`, `sandboxCwd`,
`useLegacyLandlock`): the digest covers unknown fields, so a filtered copy would be useless and
the raw state is treated as unreviewable — the detail reads `capture_skipped:unknown_fields`.
Otherwise the detail names the capture stem (`shape_not_narrower:managed;
captured:<16-hex>`). To accept the new
sandbox mode:

1. Run the host once against this worktree so the daemon observes and refuses its sandbox state.
2. `agent-ide evidence rejected` — lists each capture as `name class sandbox-cwd mtime`.
3. Review the file `~/.agent-ide/rejected-profiles/<name>.json`. It is the exact
   `codex/sandbox-state-meta` envelope the host advertised; check its actual outside grants,
   network mode, and restrictions — those are the authority you are about to accept.
4. Run a real D03 experiment under that exact state, then mint the profile fragment:
   `agent-ide evidence record --sandbox-state <file> --profile-id <id> --revision <n>
   --provider-binary <v> --toolchain <v> --configuration <v> --trust <v> --transport <v>
   --d03-evidence <id> [--shape-version <1|2>]` (the nine flags in this fixed order; the
   optional `--shape-version` flag trails them). The default is `2`: the shape-based record for
   supported managed captures. A capture whose state cannot support v2 (a `disabled` or
   unrecognized state) is refused rather than downgraded; pass `--shape-version 1` explicitly to
   mint the legacy exact-digest record.
5. Append the printed `{record, sandbox_state}` object to the target's `profiles` array and
   restart the daemon, or validate first with `agent-ide launcher check <config-file>`. Existing
   v1 records keep validating unchanged.

Known limitation (updated by T36B): shape v2 changes admission, and native reads are no longer
all-or-nothing. Since the T36B per-path read proof, native context/diff reads and cached result
delivery also work on deny-bearing states — including credential-glob captures admitted for
managed execution — for every path the live cwd-bound shape proves: `observe` proves its exact
source, diff proves each tracked path, reread, and untracked inspection before reading, and
cached delivery proves every represented path before disclosing anything. Paths a deny can
reach (or that the conservative matcher cannot reason about) still refuse with
`read_scope:path_unproven`, and whole-tree scope keeps the restrictive deny-free behavior.
D03's current fixed acceptance expectations describe the legacy workspace-write profile.

## Confined project checks (EYES-r2)

Two optional top-level fields extend the configuration for the v0.3 project problem feed. They
follow the same rules as every other field: restart-only, validated at load, and the whole
configuration is rejected on any malformed value.

- `allowed_roots`: 0..=16 absolute, normalized directory roots (no `..`, no trailing `/`); the
  product default is an empty list. A worktree is admitted when its canonical path is equal to or
  below a canonicalized root; otherwise every language state is `outside allowed roots`.
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
    while the schema on the current base accepts only `toolchain_dir` and rejects unknown fields.
  - `python`: `node` and `pyright_cli` (both required, absolute, normalized), the accepted Node
    executable and the Pyright CLI entry module it runs. The project's own interpreter is located
    inside the worktree; a missing environment reports `environment not found` rather than the
    flood of unresolved imports it would produce.
  - A language subsection absent means that language is never checked and never appears in the
    `<agent-ide>` block.

Every check process runs confined through one `sandbox-exec` profile: default-deny, no network,
read-only access to the admitted worktree, reads limited to the declared toolchain directories and
system paths, and write access only to the check's private cache directory and private temporary
directory. The environment is rebuilt from an allowlist (toolchain binaries, `HOME`, the private
temp directory, `CARGO_TARGET_DIR`, `CARGO_NET_OFFLINE=true`); ambient credentials are not passed.
For Rust, T06B adds `CC`/`CXX`/`SDKROOT` plus `CARGO_TARGET_<TRIPLE>_LINKER` (or `RUSTFLAGS`)
pointing at the resolved developer directory's own `clang`, bypassing the `/usr/bin/cc` `xcrun`
shim that a build script's link step cannot run under this profile; see EYES-r2 §3 for the exact
resolution.
Each check owns one process group, killed whole on cancel, timeout (`check_timeout_s`), or daemon
shutdown; no pattern-based kill touches processes the daemon did not start.

Check caches live outside every runtime directory, under
`$HOME/.agent-ide/checks/<16 hex blake3(repository key)>/<16 hex blake3(canonical worktree)>/<language>`
(mode `0700`), so they survive daemon idle stops and crashes; at daemon start, cache directories
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

Scheduling: triggers are a successful `ide.start`, Claude post hooks for `Edit`, `Write`,
`MultiEdit`, `NotebookEdit`, and `Bash`, and a completed `ide.edit`. Per `(worktree, language)`,
runs are debounced by `debounce_ms`, at most one runs at a time, and at most two run concurrently
across the daemon.

Results: one bounded snapshot per `(worktree, language)` carries state, error/warning counts, and
a bounded problem list. The compact `<agent-ide>` block (at most 256 bytes, counts and fixed state
text only, never paths or messages) is emitted to the owning Claude actor only when its items
change; unavailable states render fixed text (`checks disabled`, `outside allowed roots`,
`tool not found`, `environment not found`, `check failed`, `check timed out`). `ide.context` with
`{"kind":"problems","language"?,"offset"?}` returns counts and up to 20 problems per call with
`next_offset`. One `ProjectCheckCompleted` telemetry event (bucketed counts, no paths or messages)
extends the TELEMETRY-r1 event scope.

## Optional strict Claude operator profile

A target may declare `claude_profile`. Omit it, or set it to `null`, for a Codex-only target: the
existing configuration and behaviour are unchanged, and Claude execution simply remains
unavailable for that target.

```json
"claude_profile": {
  "enabled": true,
  "fail_if_unavailable": true,
  "allow_unsandboxed_commands": false,
  "no_matching_excluded_commands": true,
  "scope_declared": true,
  "platform": "mac_os"
}
```

Every field is an operator statement about the host configuration the daemon is trusted to assume;
none is measured, inferred or mutated by the daemon. A declared profile must be complete and
strict — `enabled` and `fail_if_unavailable` true, `allow_unsandboxed_commands` false,
`no_matching_excluded_commands` and `scope_declared` true, and `platform` `mac_os`. A
declared-but-weakened profile is rejected at load rather than silently downgraded, so an operator
never believes a partially strict configuration was accepted. `linux` is accepted by the schema but
never satisfies validation; see `assistance-claude-worker.md`.

Project checks use this accepted profile as their read authority. Optional `read_roots` is an
array of absolute directory grants; when absent, the existing `scope_declared` assertion grants
the project tree. `read_denies` defaults to an empty array of absolute path or glob exclusions.
An explicit grant must contain the whole worktree, and an unresolved grant cannot establish
coverage. Any `read_denies` entry makes checks `unavailable: read_restricted`, even when it names
a path outside the worktree: Rust and Python checks also read toolchains and package caches.
Declare the host's actual read exclusions here, including hidden-file globs. With no exclusions,
the existing project-check behavior remains available.

On macOS, `scope_declared` also requires Claude's sandbox configuration to allow the exact
`/private/tmp/ai-r-<repository-key-digest>/claude-helper.sock` path through
`sandbox.network.allowUnixSockets`. Per EYES-r1 §2, this path is now keyed by the repository (its
canonical git common directory, or the project root itself outside a git repository), so it is
shared by every worktree of one repository rather than distinct per worktree. The pending helper
command exposes the exact runtime directory without disclosing the attachment. Current Claude Code
does not expand a wildcard in this allowlist, so the operator must add the exact path to
project-local settings before the next session.
`allowAllUnixSockets` is outside this strict profile because it grants access to unrelated host
sockets.

To compute both exact paths for a repository in advance, run
`agent-ide claude-rendezvous /absolute/path/to/project`: it prints the shared `runtime_dir=` and
`helper_socket=` paths using the same derivation as the managed MCP server and hook, and creates
no runtime state.
