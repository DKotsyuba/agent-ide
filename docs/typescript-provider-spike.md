# TypeScript provider admission spike

Status: blocked by the v0.2 descendant-settlement gate. This is an evidence
checkpoint, not a provider-support claim.

## Verified exchange

The local accepted Node 24.4.0 executed the installed TypeScript Language Server
6.0.0 bridge in stdio mode. The bridge loaded TypeScript 5.9.3 from the explicitly
configured `tsserver.path`. Initialization fixed automatic typing acquisition off,
tsserver syntax mode to `never`, and tracing/logging off. The bridge accepted
`initialize`, `initialized`, `textDocument/didOpen`, `textDocument/definition`,
`textDocument/references`, `shutdown`, and `exit`.

A `.ts` document opened as `typescript` returned its declaration and both
references. The exchange observed `$/typescriptVersion` and a versioned
`textDocument/publishDiagnostics` notification. The installed bridge source maps
the closed extension set to the LSP IDs `javascript`, `javascriptreact`,
`typescript`, and `typescriptreact` for `.js`, `.jsx`, `.ts`, and `.tsx`
respectively.

## Topology and gate result

The stdio bridge is a Node direct child and starts `tsserver.js` as its Node
descendant in the same process group. Its observed arguments included the fixed
automatic-typing flag and no syntax-server or logging flags. This proves that a
direct-child-only settlement result is insufficient: the current Execution API
always returns `DescendantEvidence::Unverified` after a protocol-child reap.

Because v0.2 requires proved direct-child **and descendant** settlement, the
real assertion fails. The profile therefore remains `resolution_unverified` and
unavailable. No provider bundle, cache profile, launcher target, Codex path, or
Claude helper path may be enabled until Execution can provide a bounded,
positive descendant-settlement proof for this topology.

## Scope retained for a future retry

The accepted dependency closure to remeasure is the Node executable, the bridge
package, the TypeScript package, and every bridge-loaded runtime dependency.
The retry must re-run the real stdio exchange, confirm fixed initialization and
the four-language-ID table, exercise diagnostics and write refusal, and prove
that bridge and `tsserver.js` are both settled before admitting the profile.
