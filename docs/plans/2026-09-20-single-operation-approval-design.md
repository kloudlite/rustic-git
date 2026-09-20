# Single Operation Approval Design

## Decision

The operation executor and durable store own approval. Capability registry dispatch must not begin until the executor has obtained, recorded, and consumed the one approval decision.

## Flow

For an approval-required mutation, the registry first prepares a canonical approval request by performing its existing capability lookup, version check, argument validation, handler availability check, prompt construction, and payload digest calculation. Preparation cannot invoke either the approval bridge or the capability handler.

The executor submits that request to the configured approval bridge exactly once. It creates the pending durable decision, records the returned decision, and resumes through the store. A denial settles the step without dispatch. A grant records dispatch intent and calls registry dispatch with the recorded decision as proof.

Registry dispatch validates that proof against the same canonical expectation and allowed policy sources before invoking the handler. It never requests approval. Reads and capabilities whose approval requirement is `none` retain their direct dispatch path.

## Replay Timestamps

Same-revision commits remain restricted to durable decision evidence. Only a commit made solely of `decision_recorded` events may have a commit timestamp later than the unchanged snapshot `updatedAt`. Every event timestamp must equal its enclosing commit timestamp, and neither may regress behind the snapshot timestamp.

## Tests

- Approval is called exactly once.
- Registry dispatch does not begin before the durable decision has been recorded and consumed.
- Denial never reaches registry dispatch or the adapter.
- A forged or mismatched proof never reaches the adapter.
- Non-decision same-revision timestamp drift is rejected.
- Delayed decision evidence preserves snapshot `updatedAt` while retaining its later event timestamp.
