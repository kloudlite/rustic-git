# Operation executor contracts (O01, contract version `v1`)

Developer handoff for `harness/bench/src/operations/`. This is the frozen interface
later tasks build on: it defines what `operate` accepts, what the executor reports,
and the state/event/capability records the durable store and UI project.

## Files

| File | Role |
| --- | --- |
| `shape.ts` | One data-driven shape engine: validates a value and emits the JSON Schema from the same description. No second, approximate schema. |
| `contracts.ts` | The public interface: request validation, transitions, records, capability descriptors, budgets, provider interfaces, canonical identity. |
| `../../test/operations-contracts.test.ts` | Accepted/rejected requests, trust boundaries, bindings, transitions, budgets, fixtures. |
| `../../test/fixtures/operations/` | Concrete request/result/snapshot/event/descriptor/decision examples, all checked by the test. |

## The public call

`DEFAULT_INSTRUCTION_DESCRIPTION` is what `operate`'s description should teach. The
default request is one field: `{"instruction":"In src/config.ts, change the timeout to 30000."}`.
`OPERATE_REQUEST_SCHEMA` is emitted from the same descriptions the validator uses.

| Form | Shape | Notes |
| --- | --- | --- |
| instruction (default) | `{instruction, contextRefs?, inputs?, constraints?, expectedResults?}` | No `action` field; no patch, revision, or argument-name knowledge required. |
| `describe` | `{action:"describe", capability?, cursor?, detail?}` | Deterministic registry lookup; `detail:"schema"` is the only schema round trip, and it is optional. |
| `exact` | `{action:"exact", request:{objective, contextRefs?, calls[]}}` | Advanced version-pinned DAG; bindings live in `argsFrom`, literals in `args`. |
| `inspect` / `cancel` | `{action:"inspect", operationId, afterSequence?}` / `{action:"cancel", operationId}` | Control surface that must keep working with both providers down. |
| `resume` | `{action:"resume", operationId, decisionId, expectedRevision, resolution}` | Cites a recorded decision or supplies additional input. Never grants approval. |

Unknown fields are rejected on every branch; an instruction mixed with `action` is
`mixed_action`. Call `parseOperateRequest` to get a typed value or a `ContractViolation`
listing every rejected path.

## Trust boundary

- `TrustedActorContext` (actor, tenant, session, turn, tool call, turn revision, scope)
  comes from the authenticated bench boundary; `TRUSTED_CONTEXT_SOURCES` names each
  origin. It can never be supplied in a request — those fields are unknown fields.
- `ContextRef` / `$artifact` validation is syntax only. A well-formed reference is not a
  permission grant; the store and dispatch adapter still authorize every read.
- `RecordedDecision` is written only by the authenticated user UI or the trusted policy
  adapter, and binds actor, tenant, session, operation, step, payload digest, revision,
  policy source, and expiry (`checkResumeAgainstRecord` enforces each). The revision a
  decision and its `resume` bind to is the revision **at which the question was raised**
  (`PendingDecision.revision`, fixed at creation), never the operation's current snapshot
  revision: an unrelated sibling step settling between the prompt and the answer must not
  strand an otherwise-valid approval, and replay (`store.ts` `replayProblem`) already
  demands exactly this binding, so a store-level check that disagreed with it could accept
  a call that the next load would then refuse. What actually protects a changed payload
  from a stale approval is the payload digest, which sibling progress cannot alter.
  `resume` may cite a record; it can never carry one. `additional_input` may not contain
  approval-looking keys at any depth, and it can never resolve a
  `user_authorization`/`user_preference` decision (`decisionClassesForResolution`).
- `checkResumeAgainstRecord` returns a `ResumeVerdict`: `ok` means the citation checked
  out, not that work may run. A matching `granted` record yields `requiredAction:
  "dispatch"`; a validly recorded **denial** yields `"refuse_step"` and
  `dispatchAuthorized: false`, and O05/O06 must refuse the step.
- Expiry is mandatory. `RecordedDecision.expiresAt` must be after `recordedAt`, and
  `checkResumeAgainstRecord` refuses a record whose `expiresAt` passes the trusted
  `expiryBound` or is already past `now`, so no authorization is indefinite.
  `PendingDecision.expiresAt` is mandatory and after `createdAt` for every class,
  including additional input, so a waiting operation always has a persisted deadline.
- Policy source is checked at dispatch, by O05. Before running an approved step, compare
  `record.policySource` with the pending capability's current trusted approval policy in
  the durable operation record: refuse a `trusted_policy` record where that capability
  currently requires `user_ui` (a policy downgrade must not resurrect an automatic
  approval), and accept `trusted_policy` where automatic policy is still current. O05
  tests both cases; this contract only fixes the field, so the comparison cannot drift.
- The compact result is the only shape returned to the main model: no source, patches,
  actor, or scope. `OperationSnapshot` is internal.

## Bounds and identity

All values are JSON-safe with finite numbers. Per-request limits live in `REQUEST_LIMITS`;
`shape.ts` also counts **every string and key across the whole document** against
`REQUEST_LIMITS.requestChars` (262144 UTF-16 characters) and caps keys at 64, so many
individually-legal strings cannot add up to an unbounded payload. Defaults and ceilings
are in `DEFAULT_BUDGETS` (12 steps, 3 selection rounds, 2 generation calls, 4 concurrent
reads, 2 concurrent mutations, 10-minute deadline, 2-second handle, 64 KiB generated
payload, 1 MiB text file); `validateBudgets` lets a caller narrow but never raise them.
`deriveDeduplicationKey` hashes the tool-call tuple (a `":"` join would let identifiers
containing `":"` collide) and `requestDigest` hashes the canonical request.

## Lifecycle

`OPERATION_TRANSITIONS` and `STEP_TRANSITIONS` are the permitted transitions; the
trigger is part of the key, so `canTransitionOperation(from, to, trigger)` and
`assertOperationTransition` reject a right pair reached by the wrong cause (for example
`needs_input -> running` is only `decision_supplied`). O05 must have an explicit guard
for the actual fact behind each trigger.

- `partial` is terminal: effects remain with no pending decisions, no unknown outcomes,
  and no queued/running step. `validateOperationSnapshot` enforces that invariant.
  Every terminal state is checked the same way; `completed` needs at least one succeeded
  step and no failed/cancelled step, and `partial` needs genuinely mixed settled results.
- `reconciling` holds unknown mutation outcomes. `outcome_unknown` steps leave only
  through `reconcile_conclusive`; an uncertain write is never retried or cancelled away.
  Step terminal states are `succeeded`/`skipped`/`cancelled`; `failed` is settled but
  leaves only through `retry_allowed`, and only for a declared retry class.
- A failed step is settled once no retry remains — the deadline has passed, or its own
  retry facts (`error.retryable`, the capability's `retry.class`/`maxAttempts`, the
  step's own `attempts`) say so, `OperationStore#failedStepsAreFinal` — never by adding
  `failed` to the terminal step states, which would let a step already offered
  `retry_allowed` disappear from settlement while still eligible to run again. `expired`
  means nothing ran and nothing failed: a deadline passing with a succeeded or a failed
  step among the steps is reported through settlement (`completed`/`partial`/`failed`),
  never hidden behind `expired`. A dependency failure is recorded durably as `skipped`
  (`OperationStore#skipStep`, trigger `dependency_failed`, the one edge in
  `STEP_TRANSITIONS` that leaves `queued`) backed by a `progress` event carrying
  `decisionCode: "dependency_failed"` — `eventSupportsStepTransition` accepts exactly
  that shape for a `skipped` destination, and only from `queued`, so a bare `progress`
  event can never legitimise a skip on its own and a skip from any other state still
  needs the denied-decision path.
- A running step reaches `cancelled` only through `cancel_confirmed` (evidence that no
  effect was applied); `cancel_requested` alone is not an outcome. An operation holding
  unknown effects cannot expire — `reconciling` has no `deadline_reached` edge — so
  unresolved work stays visible until reconciliation is conclusive or a decision is made.
- Events replay monotonically by `sequence`; `validateEventSequence` refuses replays.
- `OPERATE_REQUEST_SCHEMA` is emitted from the same descriptions the validator uses and
  carries the real bounds (`minLength`/`maxLength`/`pattern`/`minimum`/`maximum`/
  `minItems`/`maxItems`/`minProperties`/`maxProperties`, `$ref` for JSON values). Exact
  plans publish the step count plus `EXACT_PLAN_RULES` — the cross-field DAG rules
  (unique keys, declared dependencies, binding sources, acyclic graph) that JSON Schema
  cannot express.

## Downstream handoff

- **O02** builds the executable registry with `CapabilityDescriptor` +
  `validateCapabilityDescriptor`, and serves `describe` through `describeCapability` /
  `buildDescriptionIndex`. Descriptors are frozen via `freezeCapabilityDescriptor`.
- **O05** persists `OperationEvent` / `OperationSnapshot`, applies transitions through
  the trigger-checked helpers, and stores `deriveDeduplicationKey` + `requestDigest`.
- **O06** validates DAGs through `validateOperateRequest` (the `exact` form and
  `callsWithUnresolvedBindings`) and resolves step args at dispatch with
  `resolveCallArgs` / `selectOutputPath`, then re-validates the resolved args against
  the capability input schema.
- **O03/O04** implement `JudgmentAdapter` / `GenerationAdapter`; generation inputs must
  already be `provider_eligible` by trusted policy, never by model claim.
- **O08** registers `operate` from `OPERATE_REQUEST_SCHEMA` +
  `DEFAULT_INSTRUCTION_DESCRIPTION` and bridges decisions through the trusted UI.

## Not in this task

No persistence, scheduler, adapter client, or UI code. No enablement decision: this
module only freezes shapes, bounds, and tables. Verification runs on the dev pod
(`node --test 'bench/test/operations-contracts.test.ts'`); it was not run in the
authoring worktree.
