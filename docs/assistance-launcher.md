# Trusted product launcher configuration

Set `AGENT_IDE_LAUNCHER_CONFIG` only in the daemon launch environment. The daemon reads
that file once under a 64 KiB limit. Changes take effect only after restart. It must be
maintained by the trusted launcher/operator, separately from MCP arguments and hook data.
A target path is never itself Workspace authority.

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
