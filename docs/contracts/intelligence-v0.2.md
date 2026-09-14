# Intelligence v0.2 TypeScript contract

Revision: TYPESCRIPT-r1 (proposed; extends [Intelligence v0.1](intelligence-v0.1.md)). Provider: Intelligence. Direct consumers: Execution, Workspace, Assistance, and the launcher configuration. Vocabulary: [common](common.md).

## Closed provider profile

The fourth closed provider is TypeScript. It starts `typescript-language-server --stdio` only through an accepted Node executable and immutable `TypeScriptProviderBundleV1`. The bundle names and fingerprints the bridge, TypeScript, and their loaded dependency closure. It accepts an explicit `tsserver.path` and fixes `disableAutomaticTypingAcquisition=true`; logs are off, syntax server is never enabled, and npm, plugins, ambient discovery, and network access are forbidden. A missing or changed accepted dependency, bridge, TypeScript, Node, or closure is unavailable, not a fallback to ambient resolution.

The profile is exclusive per worktree. It admits only `.js`, `.jsx`, `.ts`, and `.tsx` after a real spike verifies the exact LSP language IDs used for each extension; until then the profile is `resolution_unverified`. It has no shared backend, multi-root reuse, or inferred project discovery.

`ProjectResolutionInputsV1` is a bounded, canonical set of already observed project-resolution inputs (root identity, supported config/package lock identities, accepted bundle identity, and document language ID). Its fixed path/count/byte limits are part of the profile declaration. If a needed input is absent, unsupported, oversized, or changes, Intelligence returns `resolution_unverified` rather than guessing resolution. This type neither reads files by ambient search nor authorizes a process.

## Spike and outcomes

The real spike is the admission gate for Claude support and must prove the full immutable dependency closure, server requests and notifications, diagnostics, process topology, and descendant cleanup under the selected execution profile. It records only public versions, profile identity, and outcomes; it does not publish source, paths, prompts, commands, or private supervisor data.

Success: a `.tsx` completed context with verified bundle and bounded resolution inputs receives a same-worktree exclusive provider view and generation-matched diagnostics.

Error: a `.ts` request whose accepted `tsserver.path` changes before spawn returns `resolution_unverified`; no server starts. A server request to apply edits is rejected under the existing Intelligence write prohibition.

## Gates

A substitute gate proves bundle closure comparison, fixed settings, extension/language-ID table closure, resolution bounds, and exclusive admission. The real macOS spike proves actual stdio exchange, required notifications/requests, diagnostics, no syntax server/logs/automatic typing acquisition, process topology, and direct-child plus descendant cleanup. Until every named real assertion passes, TypeScript—including Claude support—is unavailable.
