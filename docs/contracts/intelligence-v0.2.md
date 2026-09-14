# Intelligence v0.2 TypeScript contract

Revision: TYPESCRIPT-r2 (accepted; extends [Intelligence v0.1](intelligence-v0.1.md)). Provider: Intelligence. Direct consumers: Execution, Workspace, Assistance, and the launcher configuration. Vocabulary: [common](common.md).

## Closed provider profile

The fourth closed provider is TypeScript. It starts `typescript-language-server --stdio` only through an accepted Node executable and immutable `TypeScriptProviderBundleV1`. The bundle names and fingerprints the bridge, TypeScript, and their loaded dependency closure. It accepts an explicit `tsserver.path` and fixes `disableAutomaticTypingAcquisition=true`; logs are off, syntax server is never enabled, and npm, plugins, ambient discovery, and network access are forbidden. A missing or changed accepted dependency, bridge, TypeScript, Node, or closure is unavailable, not a fallback to ambient resolution.

The profile is exclusive per worktree. It admits only `.js`, `.jsx`, `.ts`, and `.tsx` after a real spike verifies the exact LSP language IDs used for each extension; until then the profile is `resolution_unverified`. It has no shared backend, multi-root reuse, or inferred project discovery.

`ProjectResolutionInputsV1` is a bounded, canonical set of already observed project-resolution inputs (root identity, supported config/package lock identities, accepted bundle identity, and document language ID). Its fixed path/count/byte limits are part of the profile declaration. If a needed input is absent, unsupported, oversized, or changes, Intelligence returns `resolution_unverified` rather than guessing resolution. This type neither reads files by ambient search nor authorizes a process.

## Spike and outcomes

TypeScript support is a release-pinned normal-shutdown conformance claim for one exact immutable `TypeScriptProviderBundleV1`, not per-invocation proof that arbitrary descendants settled. Runtime `DescendantEvidence` remains `Unverified`. Codex and Claude require separate real macOS acceptance records for the same bundle and profile; neither host's record admits the other.

The real acceptance spike must prove the full immutable dependency closure, server requests and notifications, diagnostics, and observed bridge/TypeScript-server topology under the selected execution profile. A successful operation requires the LSP `shutdown` request to succeed, `exit` to be sent, protocol EOF to be observed, the bridge to exit successfully within the deadline, and its direct child to be reaped. This normal-success path sends no `TERM` or `KILL`. The record captures bridge and TypeScript-server PID plus Darwin start time before shutdown and confirms both identities are absent afterward. These observations are release evidence for the pinned bundle, not runtime descendant evidence or a claim that a process group contains arbitrary descendants. Records contain only public versions, profile identity, and outcomes; they do not publish source, paths, prompts, commands, or private supervisor data.

Any shutdown rejection or timeout, missing EOF, nonzero bridge exit, cancellation, forced signal, direct-child reap timeout, or lost helper returns `unavailable` or `deadline` and quarantines the exact TypeScript profile for the owner lifetime. No later operation owned by that daemon or helper may reuse the quarantined profile.

Success: a `.tsx` completed context with verified bundle and bounded resolution inputs receives a same-worktree exclusive provider view and generation-matched diagnostics.

Error: a `.ts` request whose accepted `tsserver.path` changes before spawn returns `resolution_unverified`; no server starts. A server request to apply edits is rejected under the existing Intelligence write prohibition.

## Gates

A substitute gate proves bundle closure comparison, fixed settings, extension/language-ID table closure, resolution bounds, exclusive admission, normal-success criteria, owner-lifetime quarantine, and abnormal-cleanup ordering. Separate real Codex and Claude macOS spikes prove the actual stdio exchange, required notifications/requests, diagnostics, no syntax server/logs/automatic typing acquisition, and pinned-bundle topology observations. TypeScript remains unavailable for a host until every named substitute assertion and that host's real acceptance record pass.
