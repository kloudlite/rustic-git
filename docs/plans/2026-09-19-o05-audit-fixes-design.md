# O05 Audit Fixes Design

## Scope

Implement every valid O05-owned finding from the 2026-09-19 comprehensive audit. O02 remains responsible for the executable capability registry, O06 for scheduling and execution, and O09 for UI/event transport. O05 must expose durable, enforceable primitives for those stages without implementing their engines.

## Durable storage and replay

Operation logs move to a versioned v2 frame format. Each frame carries an explicit byte length, a canonical payload digest, and one commit payload. Replay may truncate only a physically incomplete final frame. A complete frame with a bad digest, invalid schema, or illegal lifecycle transition is corruption.

Legacy newline-delimited v1 logs remain readable under strict validation. The first mutation or explicit compaction rewrites them atomically to v2 through a same-directory temporary file, file fsync, rename, and mandatory directory fsync.

Terminal retention writes a durable deduplication tombstone before reclaiming history. The tombstone retains dedupe key, operation ID, and request digest, so redelivery resolves to the original identity and a changed body remains a conflict. Terminal compaction retains a resync snapshot and an earliest replayable event sequence. Inspection reports replay, caught-up, cursor-ahead, or snapshot-required status for O09.

Operation files are opened without following symlinks where the platform supports it and are verified as regular files owned by the process with restrictive permissions. Unsupported directory fsync is a durability capability failure, not an acknowledged success.

## Lifecycle and trust boundaries

The store consumes trusted capability metadata. Callers identify a capability and target/dependencies; they do not assert effect, resource keys, approval policy, or retry policy. The store persists those security facts from the trusted source. This is the O05 seam that O02's registry will implement.

Decision recording rejects expired, future-dated, pre-consumed, session-mismatched, and policy-bound violations. Resume checks current session and turn watermark. Additional input cannot suspend running work, must still be unexpired, and records actor/session/revision/payload digest with its resolution.

Replay compares each commit with its predecessor. Revisions increase exactly once; identity, request, context, and budgets are immutable; operation and step state changes follow O01 transition tables; event revision, phase, timestamp, and operation/step identity agree with the resulting snapshot; decisions and resolutions remain internally consistent.

A post-dispatch transition records a backend operation ID once assigned. Backend IDs are immutable. Cancellation persists abort intent for running work, and recovery emits a resume-abort action rather than treating cancellation as ordinary reconciliation.

Recovery uses store-owned retry policy. Reconciliation and abort actions form an operation-wide barrier: no queued or retry work is emitted in the same plan. Waiting, reconciling, cancellation, and expired states also block dispatch. Running reads are retried only when trusted metadata explicitly permits it. Pending reconciliation includes running and outcome-unknown steps.

Retry attempts receive a fresh start timestamp. Settlement summaries count skipped work consistently.

## Testing and verification

Tests cover framed tails and checksums, legal replay transitions, immutable fields, symlink and file-mode attacks, strict directory fsync, v1 migration, tombstones, compaction cursor states, trusted descriptors, decisions and sessions, backend identity, cancellation recovery, reconciliation barriers, retry timestamps, settlement summaries, and fault points around rewrite/rename/fsync.

O05 fixtures gain a completeness manifest. Authoritative verification runs against a hash-matched temporary copy in the `dev` pod: targeted operation tests, full `npm run bench:test`, and `npm run typecheck`. The copy is removed afterward. Platform-only failures are reported separately and never described as passes.
