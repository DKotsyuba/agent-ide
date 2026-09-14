# Workspace v0.2 edit boundary

Revision: EDIT-r1 (proposed; extends, and does not duplicate, [Workspace v0.1](workspace-v0.1.md)). Provider: Workspace. Direct consumers: Changes and Assistance. Vocabulary: [common](common.md).

## One-file target authority

For `ide.edit`, Workspace owns descriptor-safe resolution of the current target and its metadata, one-use effect permit, confined replacement, and post-write read. It consumes current Workspace authority and a relative UTF-8 path. It rejects empty, absolute, dot, parent, NUL-containing, or otherwise escaping paths; it walks the already verified worktree root with no-follow descriptor operations and never authorizes a string path after resolution.

Workspace returns `CurrentEditTarget { path, source_ref, metadata }` only after reading a regular UTF-8 file through that descriptor. `source_ref` is the completed-context binding for the exact pre-write bytes: it includes the current worktree incarnation, source digest/length, observation revision, and authority epoch. A source reference from another worktree, epoch, path, incomplete context, or older source is stale. Workspace retains the exact resolved descriptor until settlement; it does not trust a re-open by name.

Create is conservative: only a missing final component under an existing descriptor-safe directory is eligible; all parent directories must already exist, be real directories, and be inside the current worktree. Creation uses one new regular file with owner-only initial permissions and no executable bits. Replacement preserves the existing regular file's permitted metadata only when it can be read descriptor-safely and remains regular. Symlinks, hard-link-sensitive targets, special files, directories, non-UTF-8 names/content, unsupported ACL/xattr/ownership/permission metadata, or any metadata that cannot be preserved safely are `unsafe_target` before a write. Workspace never creates parents, changes ownership, grants execute bits, follows links, or applies caller-selected metadata.

`replace_if_current(permit, target, source_ref, content)` rechecks authority, descriptor identity, metadata policy, and exact source binding immediately before one confined replacement. It then post-reads the final descriptor, returns its new source observation, and consumes the permit whether settled or uncertain. It does not journal, compose patches, rename, write multiple files, run a shell, or run checks.

## Outcomes and examples

The Workspace-local result is `created`, `replaced`, `unchanged`, `stale_source`, `unsafe_target`, `cancelled_no_effect`, `deadline_no_effect`, `capacity_no_effect`, or `outcome_unknown`. `created` and `replaced` include the post-read `source_ref`; `unchanged` proves the requested bytes already equal the current target. Any possible write combined with lost/ambiguous completion is `outcome_unknown(operation_id, path)`, even if a later watcher hint looks favorable.

Success: a current `source_ref` for `src/a.ts` and new UTF-8 bytes changes that regular target and yields `replaced` plus the post-read reference.

Error: a completed-context reference for the old bytes after a native edit yields `stale_source` and zero writes. A requested `new/child.ts` when `new` is missing yields `unsafe_target`, not implicit directory creation.

## Gates

A substitute gate drives descriptor/metadata fixtures, a one-use permit, and injected pre-write cancellation to prove zero-write outcomes and one-file confinement. A real integration gate races native replacement, symlink substitution, and restart settlement against a temporary worktree, proving descriptor-safe refusal, post-read binding, and no write for stale input. These gates do not add a public filesystem API.
