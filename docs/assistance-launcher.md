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
      "providers": [],
      "profiles": [
        {
          "record": {"Execution-owned": "complete accepted PersistedProfileRecord"},
          "sandbox_state": {"Execution-owned": "exact supporting measured state"}
        }
      ],
      "allow_disabled_host": false
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

Each optional provider has `executable` in the same shape as `git`, a closed `settings`
value, `toolchain`, `trust`, and `cache_namespace`. `gopls_defaults` requires an absolute
Go executable as `toolchain` and absent/null `cargo_version` and `rustc_version`.
`rust_cache_priming_disabled_v1` requires a nonempty rustup toolchain selector plus accepted
nonempty `cargo_version` and `rustc_version` identities. Arbitrary settings objects and
duplicate language/settings entries are rejected.
`cache_namespace` is a bounded compatibility label, not a filesystem path or
authority grant. Assistance combines it with the verified durable worktree and
accepted provider identities to retain a private directory, then supplies only
that derived directory through the provider's cleared environment. Configuration
cannot select `HOME` or another writable host path.

Limits are explicit: 1–64 queued operations, 1–128 retained details, 1–300000 ms per
operation, and 1–1048576 retained bytes per output stream. There are at most 64 distinct
attachment mappings, two provider languages per target and two accepted Execution profile
classes. Duplicate attachment mappings, unknown fields, invalid limits, relative paths,
malformed executable digests and mismatched profile evidence are rejected.

Configuration contains private attachment and evidence values. Diagnostic formatting
redacts the configuration; it must never be rendered in model-facing tool results.
