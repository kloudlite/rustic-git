# O06 Final Review Fixes Design

## Scope

Close the final scheduler and executor review findings without expanding O05 durable storage. Recovery must not dispatch work unless it can prove the exact validated payload and preserve scheduler conflict guarantees.

## Recovery

`OperationExecutor.recover` executes only ownership-checked reconciliation and expiry actions. It leaves `dispatch_step`, `retry_candidate`, `resume_abort`, and `await_decision` pending. Normal capability dispatch is never used as an abort adapter, and restart recovery never reconstructs arguments from literals while authorizing them with an earlier digest.

This deliberately defers automatic queued and retry recovery until a later design persists an exact execution envelope or durable outputs sufficient to reconstruct and verify bindings. Because recovery dispatches nothing, it cannot bypass scheduler conflict lanes or dependency ordering.

## Live Execution

Idempotent retries stop at the descriptor's `maxAttempts`. The executor records the final retryable failure and settles it instead of asking O05 to authorize an attempt beyond the trusted ceiling.

Caller cancellation and durable deadlines feed one executor-owned abort controller. When the deadline fires, the executor first records durable cancellation intent, then aborts scheduler work. An already-expired deadline remains an explicit scheduler rejection, with operation expiry persisted before the rejection escapes.

Cancellation evidence is authoritative. A read is marked cancelled only when its adapter returns the `cancelled` error code. If the signal races with an ordinary provider failure, that failure remains a failure. The executor does not synthesize no-effect evidence from signal state.

## Binding Validation

A binding may select an output named explicitly in `outputSchema.properties` or admitted by schema-valued `outputSchema.additionalProperties`. Selection and assignability checks continue from that resolved output schema. Boolean open objects do not provide enough type information and remain invalid binding sources unless the output is declared explicitly.

## Tests

Regression tests prove:

- recovery dispatches no queued, retry, or abort action;
- reconciliation and expiry recovery still execute;
- retry exhaustion settles the final durable failure;
- a deadline firing during dispatch records cancellation intent;
- a read failure racing with abort remains failed without fabricated evidence;
- schema-valued open-object outputs can satisfy compatible bindings;
- incompatible open-object bindings are rejected.

Affected executor, scheduler, and store suites plus strict operations typecheck remain the completion gate. The full harness suite may still require an X server for its Electron boot test.
