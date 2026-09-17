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
attachment mappings, four provider languages per target and two accepted Execution profile
classes. Duplicate attachment mappings, unknown fields, invalid limits, relative paths,
malformed executable digests and mismatched profile evidence are rejected.

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
