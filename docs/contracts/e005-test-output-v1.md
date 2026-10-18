# e005-test-output-v1

Revision: 1. Provider: M-037. Consumer: M-038. Basis: DOC-003 T4.

`ide.test {"output":"<existing-ref>/output/N"}` explicitly restores one retained
test-output page, including a page already delivered before context loss. `N`
is a positive one-based decimal page number. `output` is a nonempty bounded
string using the existing detail-reference limit. It is an exclusive test
selector: no command, targets, result/status, history, cancel, detail_ref,
budget_s, cwd or env may accompany it. This is not a `force` flag or an inspect
operation. Existing `detail_ref` output-page reads keep compact acknowledgements
on repeats and in-order delivery.

M-038 owns the Test catalog, allowed keys, validation, nonmutating classification,
normalization and generated host snapshots. M-037 owns dispatch from
`WorkerHandle::submit`, the retained `ContextPageState` and `serve_inspection`
execution path. A valid explicit output query uses the same Test tool identity,
owning binding and original retained reference; it neither enqueues a test nor
requires new async admission. It returns the existing bounded `PeerReply::Complete`
with `ResultKind::Test`. Its `detail_ref` names the next numbered page when
`continuation` is true; explicit recovery continues with `output` again.

Explicit recovery leaves the ordinary delivery cursor unchanged. Pages are
deterministic slices of the original captured log; no runner is reexecuted.
Retention remains bounded by the existing 256 KiB test-output capture and the
owning session detail lifetime/ledger eviction. Final ordinary-page delivery
does not delete those bytes before that lifetime ends. Expired, foreign,
wrong-tool or non-test-output references, malformed page suffixes and page
numbers beyond the captured log refuse through existing invalid-detail/error
semantics; no fabricated empty success. Existing active binding, authority and
source checks apply before disclosure. Upstream truncation remains explicit.

Boundary proof: ordinary duplicate first/final pages acknowledge briefly;
`output` restores their exact bodies before and after ordinary completion,
without moving its cursor, changing the saved result or rerunning a side effect.
Foreign/wrong-tool/expired/malformed/beyond-end requests refuse. Direct worker
tests prove the mechanism; assembled MCP tests prove catalog and validation
composition on the actual build. A mutation deleting retained log bytes must
make the same recovery test fail, then restoration must pass.
