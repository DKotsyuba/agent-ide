# TypeScript provider release evidence

Status: `real_pass` for the Codex macOS host cell on 2026-09-14; `not_tested` for
Claude. This is release evidence for the pinned bundle only. Runtime
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
cell. They neither prove arbitrary descendant settlement at runtime nor claim that
a process group contains every descendant. Claude requires its own real macOS
foreground-helper record before this bundle may be enabled for Claude.
