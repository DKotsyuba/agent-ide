# TypeScript provider release evidence

Status: `real_pass` for the separate Codex and Claude macOS host cells on 2026-09-14.
This is release evidence for the pinned bundle only. Runtime
`DescendantEvidence` remains `Unverified`.

## Pinned exchange

On macOS 26.6.2, the accepted Node 24.4.0 executed TypeScript Language Server
6.0.0 in stdio mode with TypeScript 5.9.3 selected from the explicit
`tsserver.path` (`source=user-setting`). Initialization fixed automatic typing
acquisition off, syntax-server mode to `never`, and tracing and logging off. The
exchange completed `initialize`, `initialized`, `textDocument/didOpen`,
`textDocument/definition`, `textDocument/references`, `shutdown`, and `exit`.

A `.ts` document opened as `typescript` returned one definition, two references,
and two diagnostics. The bridge emitted `$/typescriptVersion` and
`textDocument/publishDiagnostics`; its diagnostic notification omitted a document
version even when the client advertised version support, so the runtime must not
invent version correlation. The server made one bounded `workspace/configuration`
request. The installed bridge source maps the closed extension set to the LSP IDs
`javascript`, `javascriptreact`, `typescript`, and `typescriptreact` for `.js`,
`.jsx`, `.ts`, and `.tsx` respectively.

## Normal-shutdown topology

Before shutdown, the process observations were:

| Role | PID | PPID | PGID | Darwin start time |
|---|---:|---:|---:|---|
| stdio bridge | 67421 | 67420 | 67421 | Mon Sep 14 13:46:10 2026 |
| `tsserver.js` | 67422 | 67421 | 67421 | Mon Sep 14 13:46:10 2026 |

The observed TypeScript-server arguments included
`--disableAutomaticTypingAcquisition` and contained no syntax-server or logging
flag. After the successful shutdown response, the client sent `exit`, observed
protocol EOF, observed bridge exit code zero with no signal, and reaped the direct
bridge child. The client requested no TERM or KILL. Fresh process queries for both
captured PID/start-time identities were empty after ordinary shutdown.

These observations establish normal-shutdown conformance for this exact release
cell. The configured host records append the stable digest of the exact accepted
Node/bridge/`tsserver.js`/closure declaration; changing any declared identity rejects a copied
record, and the distinct Codex and Claude prefixes cannot substitute for each other. They neither
prove arbitrary descendant settlement at runtime nor claim that a process group contains every
descendant.

## Claude foreground-host spike

Claude Code 2.1.267 ran the same pinned stdio bridge in a foreground Bash operation under its
normal strict macOS sandbox and normal authenticated profile. The exchange returned a semantic
definition, two references, and a nonempty diagnostic publication, then completed `shutdown`,
`exit`, EOF, and bridge exit zero. An observer outside the sandbox captured the bridge and direct
`tsserver.js` PID plus Darwin start-time identities before shutdown and confirmed both exact
identities absent afterward. The observer did not widen the sandbox or grant process-table access
inside it. The production helper gate separately reconstructed the bundle from the accepted Claude
launcher frame, returned semantic TypeScript Context, and reported every direct child reaped.

## Shipping product fixture

The prior r2 evidence is superseded by this r3 record. The r3 closure requires an observed
configured project: JavaScript and JSX documents require `compilerOptions.allowJs=true`, while
TypeScript and TSX documents do not invent that requirement. The top-level `files` array lists all
four fixture documents exactly, so configured membership is observed directly; `include` and
`exclude` remain refused.

The shipping MCP and daemon then loaded the same accepted bundle through the fourth
`typescript_defaults_v1` launcher profile. Real `.js`, `.jsx`, `.ts`, and `.tsx`
files under one exact `tsconfig.json` containing `types=[]`, `moduleResolution=node10`, and
`allowJs=true` each returned semantic definitions and references through separate exclusive
one-shot sessions. Every operation completed the strict graceful session result before the direct
bridge child was released. A separate `.ts` fixture completed the same semantic Context and
normal-shutdown path inside the Claude foreground helper. This covers the TypeScript Context and
normal-shutdown subset only; it does not complete the broader v0.2 edit, telemetry, fallback, or
multi-worktree acceptance matrix.
