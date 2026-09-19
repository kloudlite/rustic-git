# Operation UI handoff — O09 lifecycle and control slice

Date: 2026-09-18
Updated: 2026-09-19
Status: O09 lifecycle, accessible UI, renderer integration, and control bridge are implemented and
target-tested. **There is no live end-to-end production operation yet.** The O05 durable source and
adapter, O08 `operate` registration/executor/result-production emission, and deployment
`KL_OPERATION_CONTROL_MODULE` configuration are absent. Without a configured source, operation
control routes return typed HTTP 503 `operation_source_unavailable`. Final authoritative broad-run
verification is still open because the renderer boot broad-run environment issue remains unresolved.

Plan: [Harness operation executor](2026-09-18-harness-operation-executor.md) (O09)
Remediation plan: [Operation UI remediation](2026-09-19-operation-ui-remediation.md)
Remediation design: [Operation UI remediation and production bridge](../specs/2026-09-19-operation-ui-remediation-design.md)
Contracts: [Operation executor contracts](../../../harness/bench/src/operations/CONTRACTS.md) (O01, `v1`)
Integration map: [O08 integration and authentication bridge](2026-09-18-harness-operation-integration.md)

## Implemented boundary

O09 is a truthful projection and control surface over O01 snapshots and events. It does not create,
schedule, execute, persist, or emit production operations.

Implemented:

- Exact O01 operation and step lifecycle projection, with runtime ingress validation, non-regressing
  revisions, payload-aware duplicate handling, ordered cursor repair, guarded retries, decision
  linkage, explicit unknown outcomes, and terminal evidence/open-work checks.
- Cancellation intent remains separate from durable state. Contradictions request authoritative
  repair rather than inventing a plausible result.
- Accessible operation, step, and decision components: controlled native disclosures,
  `aria-expanded`/`aria-controls`, status/live regions, labelled additional-input form with Enter
  submission, promise-aware pending/error controls, and a modal confirmation dialog with initial
  focus, Escape, focus containment, and focus restoration.
- One renderer projection per operation ID. The store loads a snapshot first, replays all paginated
  events after its cursor, catches up on notifications/reconnect, services repair requests, scopes
  task rows to session/workspace, rejects owner mismatches, retains projections across archive, and
  disposes them with transcript/session deletion.
- An `operate` tool result mounts the same live projection under the tool call. The taskboard derives
  background-operation rows from that store and opens that same projection, while preserving legacy
  process/task rows.
- Narrow bench control routes for inspect, event replay, cancel, decision, and additional input.
  Requests and source responses are validated against O01, ownership is checked before data is
  exposed, child credentials are refused, and stale revisions retain typed details.
- Credential-free renderer transport. Renderer calls cross preload IPC; desktop main validates
  payloads and attaches its existing login bearer plus `x-kl-owner` and `x-kl-login`. The bearer is
  never exposed through preload or renderer state.
- Production online authorization checks the submitted person credential against
  `GET /v1/bench?team=...` and requires the configured bench ID, owner, team, and `access: "Full"`.
- A configured production seam loads an external module named by `KL_OPERATION_CONTROL_MODULE`.
  That module must export `createOperationControl({ owner, team, bench })` and return an
  `operationSource`. This is a seam only; no production module is supplied or deployed here.

Explicitly absent:

- O05 durable operation storage, recovery, and the production adapter implementing
  `OperationSource`.
- O08 model-facing `operate` registration, executor dispatch, and compact result plus durable
  snapshot/event production emission.
- Deployment configuration for `KL_OPERATION_CONTROL_MODULE` and a module package available to the
  bench process.

Consequently, the mounted renderer and controls are reachable code, but normal production model
traffic cannot yet produce an operation for them to display. An unconfigured bench fails closed with
the typed 503 rather than using fixtures or an in-memory production fallback.

## Exact interfaces and routes

The production source contract is `OperationSource` in
`harness/bench/src/operations/control.ts`:

```ts
interface OperationSource {
  inspect(operationId: string): Promise<unknown>;
  events(operationId: string, after?: string, limit?: number): Promise<unknown>;
  cancel(operationId: string, expectedRevision: number): Promise<unknown>;
  recordDecision(
    operationId: string,
    decisionId: string,
    intent: { stepId: string; expectedRevision: number; outcome: "granted" | "denied" },
    principal: OperationPrincipal,
  ): Promise<unknown>;
  provideInput(
    operationId: string,
    decisionId: string,
    expectedRevision: number,
    inputs: Record<string, JsonValue>,
  ): Promise<unknown>;
}
```

Bench routes, mounted by `harness/bench/src/server.ts` and implemented in
`harness/bench/src/operations/control.ts`:

| Method and route | Source call / result |
| --- | --- |
| `GET /operations/:operationId` | `inspect`; validated `OperationSnapshot` |
| `GET /operations/:operationId/events?after=:cursor&limit=:n` | `events`; validated `{ events, nextCursor?, hasMore }`, limit 1–200 |
| `POST /operations/:operationId/cancel` | `{ expectedRevision }`; validated snapshot |
| `POST /operations/:operationId/decisions/:decisionId` | `{ stepId, expectedRevision, outcome }`; validated `RecordedDecision` |
| `POST /operations/:operationId/input` | `{ decisionId, expectedRevision, inputs }`; validated snapshot; approval-shaped input keys are refused |

All routes require a person principal and verify the inspected snapshot belongs to the authenticated
actor and tenant. Missing source or authorizer returns:

```json
{"error":{"code":"operation_source_unavailable","message":"operation source unavailable"}}
```

with HTTP 503. Source contract violations return typed 502 `invalid_source_payload`; malformed
requests return 400 `invalid_request`; stale revisions map to typed 409; hidden ownership mismatches
map to 404.

Renderer transport is `OperationRendererBridge` in
`harness/src/renderer/operations/bridge.ts`:

```ts
type OperationRendererBridge = {
  loadSnapshot(operationId: string): Promise<OperationSnapshot>;
  loadEvents(operationId: string, afterSequence: number): Promise<OperationEvent[]>;
  watch?(operationId: string, onChanged: (lastSequence: number) => void): () => void;
  decide?(payload: DecisionCallbackPayload): Promise<void>;
  answer?(payload: AdditionalInputCallbackPayload): Promise<void>;
  cancel?(payload: CancelCallbackPayload): Promise<void>;
  onResyncRequested?(request: ResyncRequest): void;
  onConnection?(onConnected: (connected: boolean) => void): () => void;
};
```

Current desktop bridge uses bounded HTTP replay, not a new operation event socket. The preload
surface is `window.harness.operations.{snapshot,events,cancel,decision,input}`. IPC handlers are
`operations:snapshot`, `operations:events`, `operations:cancel`, `operations:decision`, and
`operations:input` in `harness/src/main.ts`; only main reads the login token.

## Files

| Area | Files |
| --- | --- |
| O01 projection and presentation | `harness/src/renderer/operations/{types,reduce,present,bridge}.ts` |
| Renderer store and exports | `harness/src/renderer/operations/{store,index}.ts` |
| Accessible UI | `harness/src/renderer/operations/components/{OperationPanel,StepList,DecisionPrompt}.tsx`, `harness/src/renderer/operations/styles/operations.css`, `harness/src/renderer/ui/{Confirm,Button}.tsx` |
| Production mount and taskboard | `harness/src/renderer/App.tsx`, `harness/src/renderer/components/ToolCall.tsx`, `harness/src/renderer/components/inspector/{Inspector,Tasks}.tsx`, `harness/src/renderer/components/TaskView.tsx` |
| Desktop transport | `harness/src/{preload,main,bench-client}.ts` |
| Bench controls and seam | `harness/bench/src/server.ts`, `harness/bench/src/operations/{control,production}.ts`, `harness/bench/src/main.ts` |
| Fixtures | `harness/src/renderer/operations/fixtures/scenarios.ts` |

`harness/src/renderer/App.tsx` configures the bridge. `ToolCall.tsx` accepts only a validated O01
`CompactOperationResult` from an `operate` result; it does not trust tool arguments or free-form
output to discover an operation ID. `store.ts` owns snapshot-first loading and taskboard projection.

## Lifecycle guarantees

- Snapshots/events are runtime-validated at renderer and bench ingress.
- Sequence replay is idempotent only when duplicate canonical payloads agree. A conflicting duplicate,
  gap, invalid transition, regressing revision, missing evidence, or incomplete terminal state asks
  for authoritative repair.
- Repair snapshots may advance the cursor; held newer events remain replayable.
- A step terminal event never settles its operation. A running-step cancellation requires
  `cancel_confirmed`; success requires evidence; silence is never completion.
- Retry requires the durable snapshot's `retryable`/attempt metadata and the matching dispatch retry
  count. The renderer does not infer a retry class or maximum absent from O01.
- Events still cannot carry every durable fact: dependency skips, typed retryability, decision
  content, usage, step timestamps, and reconciliation conclusions come from snapshots.
- Decision callback payloads are intents only. They cannot supply actor, tenant, session, policy
  source, record ID, digest, or expiry, and additional input cannot encode approval.

## Tests and gates

Focused coverage exists in:

- `harness/bench/test/operations-ui.test.ts`: frozen transitions, validation, sequence/revision
  repair, retries, decisions, cancellation, terminal evidence, fixtures, and presentation.
- `harness/bench/test/operations-components.test.tsx`: disclosures, live regions, labelled input,
  pending/error controls, confirmation focus/Escape/restore, responsive/evidence access.
- `harness/bench/test/operations-bridge.test.ts`: intent payload boundaries.
- `harness/bench/test/operations-integration.test.ts`: snapshot-first store, cursor replay,
  reconnect/resync, disposal/session ownership, typed errors, task-state derivation.
- `harness/bench/test/operations-integration-components.test.tsx`: `operate` mount and taskboard detail.
- `harness/bench/test/operations-control.test.ts`: real HTTP routes, validation, unavailable 503,
  stale revision, person-only and ownership refusal.
- `harness/bench/test/operations-production.test.ts`: configured module seam and online bench
  authorization, including fail-closed configuration/identity cases.

Commands for the authoritative dev-pod gate remain:

```sh
cd harness
npm ci
npm run typecheck
node --test bench/test/operations-ui.test.ts bench/test/operations-bridge.test.ts \
  bench/test/operations-control.test.ts bench/test/operations-integration.test.ts \
  bench/test/operations-production.test.ts
npm run test:components
npm run bench:test
npm run build:renderer
# run the repository renderer boot gate in its expected environment
```

The focused O09 lifecycle, component, store/integration, control-route, and production-seam tests
have been implemented and exercised. Do not infer a clean final full-suite result from those focused
runs. The renderer boot broad-run environment issue is still pending final authoritative
verification, so the full gate and O09 final verification are not claimed here.
