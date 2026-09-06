# Code intelligence and shared LSP backends

Revision: r2. Provider: Intelligence. Consumers: Assistance and Changes. Inputs: Workspace authority/snapshots/access scope, Execution admission/process endpoints, Application effective provider settings. Vocabulary: [common](common.md).

## Public domain surface

`attach_view` takes active authority, workspace incarnation, provider/profile requirements and reuse policy; returns a view handle, effective capabilities, backend generation and independent backend/view readiness, or queued/unsupported/incompatible/unavailable. A frontend instance does not imply a physical launch. `context`/`impact` take a current scope, query/target and bounded detail request; return exact snapshot-bound semantic facts, confirmed relations/tests, diagnostics and explicit coverage. Raw request_lsp stays private to this module, never a model tool.

`prepare_rename` takes a symbol target, expected snapshot and new name; returns a version-bound `RenameProposal` with exact canonical targets, byte ranges, expected bytes, replacement bytes, profile/backend generation and completeness. It has no disk effects. Incomplete, overlapping, out-of-scope, oversized or stale proposals cannot become an applicable edit. Changes alone validates/applies a returned proposal.

`diagnostic_observations` returns issues plus covered scope, source encoding/version and freshness. No message received is unknown/not-ready, never clean. An unversioned push observation is provisional unless a validated provider-specific barrier or independent current evidence proves applicability. It may inform explicitly provisional advice; it cannot establish current clean/fixed status.

`release_view` releases only the supplied logical lease. Frontend disconnect does not itself stop the coding session. Ownership loss/explicit stop is handled through the session lifecycle; shared backend shutdown requires no surviving view lease and the provider's supported lifecycle.

## Protocol and profile contract

LSP Content-Length is a byte count. Bound headers, bodies, outstanding requests and output; test fragmented/coalesced frames, EOF, malformed IDs/encoding, server-initiated calls and late responses. A frame/protocol failure invalidates that upstream generation and affected views honestly. Do not treat lsp-server as a Tokio client. Selected Rust LSP type coverage is checked early against required methods; dependency choice is not a claim of provider support.

Each profile states binary/version identity, language/protocol capabilities, toolchain/semantic configuration, readiness rule, sharing mode, maximum views, memory estimates, supported transport, owned/borrowed lifecycle and compatibility revision. Native multi-session or validated multi-workspace sharing requires real divergent-worktree tests. Otherwise use exclusive context and explicit admission/queue behavior. `require_existing` never spawns; `prefer_existing` single-flights an owned cold start when allowed. Existing means a registered supported endpoint, not arbitrary editor PID discovery.

Compatibility key includes semantic binary/protocol/profile/config/toolchain/trust-domain identity. Worktree context belongs in the key for exclusive profiles. In sharing profiles it remains an isolated view key. No live migration or retuning a backend beneath other views in the first release.

Route by backend generation, upstream connection, upstream request ID, view, authority and snapshot; never by arrival order, current cwd or active editor. Canonical URI mapping is exact, with scoped shared read-only dependencies and no cross-owner mutable buffers. Per-view document versions and synchronization ordering are independent. Unicode position encodings are negotiated and translated against exact snapshot bytes, including CRLF/astral/combining cases.

## Server requests

First release rejects workspace/applyEdit callbacks without applying any bytes; do not advertise callback-write capability. Explicit rename uses a returned plan. Workspace/configuration is answered per item/scope URI; scope-less settings must be globally compatible or fail visibly. Noninteractive prompts have no affirmative default. Unknown server commands or content are not agent instructions.

## Shared lifecycle and provider acceptance

Intelligence owns the compatibility pool, protocol generation and view lease decision. Execution owns physical ProcessInstanceId, admission and cleanup. During stop, dispose/disable the view; never leave an owned retained backend analyzing that inactive workspace. Borrowed processes are not terminated. If a provider cannot detach/quiesce one view safely, that sharing/retention profile is unsupported. A backend crash marks all its views unavailable, cancels its mappings and single-flights restart under new generations.

First real provider checks include a gopls native daemon serving two divergent worktrees with duplicate module/symbol names, config differences and stop-one/keep-peer behavior. This is an acceptance task, not an already demonstrated fact. Rust analysis starts with an exclusive rust-analyzer profile unless sharing is independently proven. Record a capability matrix for other installed/target providers; expansion follows real tests.

Contract harness: `tests/intelligence_contract.rs`; scripted wire peer: `tests/intelligence_transport.rs`; real checks: `tests/intelligence_gopls.rs`, `tests/intelligence_rust.rs`. Fake peers verify protocol/routing, never actual semantic isolation. The same applicable boundary cases run against the real provider adapters.

## Warm handoff and provider caches

A released actor's request/result mappings and feedback authority are cancelled. A frontend disconnect remains transport detach; explicit session release permits a successor only through fresh Workspace admission. Reuse compatible warm analysis without resetting it merely because actor/session identity changes. Backend or provider cache identity contains binary/protocol/profile/config/toolchain/trust inputs and a provider namespace; project/view state additionally binds canonical worktree incarnation/root and current semantic inputs. Actor/session/authority/context IDs are excluded from reusable cache keys. Root-independent immutable dependency caches may be shared only under their provider-verified content identity.

`attach_view` returns `ReuseDisposition { hot_backend | native_persistent_cache | cold | unavailable | queued, reused_components, required_refresh, reason }`. Even hot reuse requires a new owner-bound logical handle and current Workspace observations/readiness; old replies, diagnostics delivery or permissions do not become successor-owned facts. Valid compatible caches are not cleared on coder finish, stop or sequential handoff.

Retain hot state only within configured memory/process/view/TTL limits and with verified quiescence for an inactive workspace. If a provider cannot detach/quiesce safely, release the owned backend after its last lease; preserve supported owned disk cache rather than inventing a pause/export. Borrowed endpoints/caches are never killed, retuned or cleaned. Native persistent restore is a profile-specific capability with documented validity/ownership rules and actual restore evidence; generic serialized LSP-index export is not assumed. Cache priming is not evidence of disk persistence.

Application provides owned cache directories, bounded storage/provenance and safe eviction mechanics; Intelligence owns native-cache compatibility and provider-supported integrity/restore checks. Do not checksum/delete/move live opaque provider cache files indiscriminately. Corrupt/unsupported/incompatible state yields cold/unavailable with an explicit reason; it never becomes valid empty analysis. Cache pressure can evict only eligible owned unused entries and must report the lost warm capability.

Attach/restore/refresh and restart have bounded deadlines/retries and one provider-breaker owner. Failure returns degraded/unavailable with practical native fallback; it must not block ordinary agent work or create reindex loops. Test real coder→reviewer handoff on the same unchanged worktree and restart restore where the provider supports it. Measure first-ready/semantic latency, actual rebuild/cache evidence where exposed, process-tree RSS and disk usage versus cold; latency alone is not proof that indexing was avoided. Required harnesses: `tests/intelligence_handoff.rs`, `tests/intelligence_cache_restore.rs`. Unsupported disk restore is explicit capability evidence and cannot count as passing a restore test; a real compatible warm-handoff profile is mandatory before release.

Worktree lifetime is the default retention authority for compatible persistent cache. Finish/stop/actor exit/reviewer handoff do not delete it. Verified worktree deletion or explicit reset retires the owned namespace; moving/unmounting a worktree is not deletion. Content/config/toolchain changes invalidate affected records/profile compatibility, not all unrelated valid state. In-memory process eviction is separate from persistent namespace deletion. Disk pressure may evict obsolete/invalid owned entries or refuse further cache growth under declared policy; it must not silently wipe valid worktree cache and report a warm handoff. Report quota/cold capability loss honestly. A provider whose opaque cache cannot honor this policy must declare that limitation and cannot advertise persistent-worktree-cache support.
