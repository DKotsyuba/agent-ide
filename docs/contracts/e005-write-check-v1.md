# e005-write-check-v1 revision 1

Provider: M-035 synchronous write receipts. Consumer: M-037 accepted check completion.
Authority: owner-adopted DOC-003 T2/T5; preserve DOC-002 public edit and Bridge contracts.

After existing admission, freshness, formatting and syntax validation, the provider returns
`PeerReply::Edit` or `PeerReply::EditBatch` from the actual durable write outcome, independently
of provider/check readiness. The existing operation_id, per-file outcome and source/result
reference identify that receipt. A refused or outcome_unknown write never claims application.
A planning-only or zero-commit edit_progress reply is unresolved progress, not a receipt.

The consumer receives the existing `JobStage::EditAwaitingCheck` or
`JobStage::EditBatchAwaitingCheck(BatchCheck)` state. It retains the operation/result, binding,
authority, post-write source observations/fences, affected paths/languages, accepted generation
and deadline. Existing result/check linkage uses the retained job reference; no new wire type,
carrier or second execution of the write is introduced. Pending diagnostics may name that
reference, but recovering diagnostics is not required to know whether the write committed.

Known-disabled/unavailable checks are classified with their reason in the receipt before
acceptance and owe no empty wake. An accepted check owes one terminal edit-checked result
through qualified Bridge, including unavailable/cancelled/superseded; the consumer owns
settlement, diagnostics identity/freshness and deduplication. No reply piggyback is added.
Lost/delayed write replies remain recoverable with the same operation_id without reapplying.

Source boundaries: assistance/worker.rs::edit_with_source and JobStage;
assistance/batch_edit.rs::schedule_batch_check and BatchCheck;
assistance/delivery.rs::reply_edit_early and accepted-check settlement.
Compatibility proof: held/unavailable provider and check do not delay committed receipts;
receipt identity remains usable while a check is pending; lost-reply retry writes once;
known-disabled versus accepted-check terminal cases preserve all existing authority fences.
