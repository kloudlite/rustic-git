# O08 integration map and authentication bridge

Planning assessment; no implementation or tests. Baseline inspected: `0fe12268ed6a157db60fddef75716da195d0921a`, `/Volumes/kdisk/rustic-git-wt/operation-executor`. Paths below are relative to that checkout. Preserve the accepted minimal instruction interface; no actor, credential, or approval-grant fields are added to model arguments.

## Integration map

| Boundary | Existing locations | Required integration |
| --- | --- | --- |
| Registration | `harness/pi/kloudlite.ts:1334` default extension, `:1124` tools, `:334` makeReg; `harness/pi/workspace-tools.ts:534` registration | Opted-in main sessions register operate before legacy registration. Preserve worker, ephemeral, info-fork and no-tool-fork roles. |
| Every current activation | `kloudlite.ts:618` plan toggle, `:661` search activation, `:1306` session start | One mode/role allowlist governs all three. Plan exit currently restores ALWAYS_ON. |
| Remembered discovery | `kloudlite.ts:1314` foundHere, `:1322` rememberFound; `server.ts:149` found endpoint | Old discoveries cannot reactivate legacy tools in single-tool mode. Filter both persistence responses and restoration. |
| Startup and reported surface | `harness/bench/src/rpc-child.ts:78-93` args, `:102` tools, `:122` hands | Preserve no-builtin-tools; use authoritative allowlist for startup and listings. Inspect real runtime registration and dispatch, not only computed tools(). |
| Dispatch | `server.ts:548` RPC forwarding to `bench.ts:1618`; extension execute wrappers | Block forbidden public activation/invocation; internal capability dispatch is separate from model registration. |
| Scope injection | `bench.ts:332` child options; `rpc-child.ts:154-158` session/workspace/tree/fork environment | Bind trusted context from bench records. Existing KL_SESSION is a name, not authentication. |
| Policy | `kloudlite.ts:340-347` gated propose, `:411-413` widget/wait; `workspace-tools.ts:542-551` preflight and mutation approval | O02 exposes a shared policy-bearing adapter, including awaited decision handling. Exact calls use it too. |
| Proposal UI | `bench.ts:509-540` proposal receipt, `:1360` answerProposal; `server.ts:194-220` routes; `renderer/live.ts:295,780` answers/auto-approval | Project durable operation decisions into familiar cards; do not reuse legacy in-memory proposal answers as authority. Preserve accept-edits by actual capability, never by outer operate name. |
| Events | `bench.ts:217-221`, `server.ts:452`, `harness/src/bench-client.ts:188-206` reconnect | Add durable event cursor replay and snapshot resync. Broadcast alone is not replay. |
| Idle and ownership | `server.ts:54`, `bench.ts:1537` busy, `:1546` writable/read-only refusal | Include executing/reconciling operations; persist waits without holding workers. Recovery requires writable ownership. |
| Archive | `bench.ts:1696` archive, `:1719` settleOpen, `:1730` legacy proposal denial; `:1622` archived RPC refusal | Preserve durable operation ownership and owner controls. Archived child cannot resume. Do not erase durable decisions through legacy cleanup. |
| Disable | New wiring required | Control API remains available independently of model mode/providers. Change public surfaces on safe session transition; do not duplicate delivery. |

## Selected smallest authentication bridge

Use the desktop's existing login credential for authenticated UI operation-control requests, with online validation against the existing person-only bench endpoint. Issue a separate random scoped credential for each child incarnation. Do not add platform routes, gateway protocol, shared JWT signing secrets, or a new UI-token lifecycle in this first implementation.

### Why existing nonce/session names are insufficient

`harness/src/connect/tunnel.ts:16-21,31-44` checks a random per-launch x-kl-tunnel nonce only at the desktop loopback tunnel; it forwards the request unchanged. The bench does not know this nonce. Header presence cannot authenticate a bench request. Keep this local protection intact.

The gateway authenticates a single-use bench ticket (`bins/gateway/src/tunnel.rs:202-222`); the ticket is consumed for the tunnel and is not an HTTP user identity forwarded to the bench. Do not reuse it as a control API bearer.

`KL_TOOL_TOKEN_FILE` is deliberately available to children (`harness/pi/kloudlite.ts:18-36`). It represents permitted platform tool access and must never authorize user decisions. Deployment scope comes from `crates/workspaces/src/k8s/bench.rs:47-55` (KL_OWNER/KL_TEAM/KL_BENCH), not caller JSON.

### UI authentication

1. Desktop main process injects its existing login credential as Authorization bearer on operation control requests and operation-specific event retrieval. The renderer never receives it. Retain x-kl-tunnel independently.
2. Bench treats this as a candidate UI credential, validates length/format, and forwards it only to the configured trusted platform base: `GET /v1/bench?team=<encoded deployment KL_TEAM>`, `redirect: error`, bounded timeout. No caller-controlled validation URL, team, or owner. No token in URL, body, logs, traces, cache, operation records, or model input.
3. Accept only successful valid response whose bench ID and owner equal deployment KL_BENCH/KL_OWNER and whose access permits the requested control. Bind tenant to deployment KL_TEAM; verify returned normalized team consistently with the endpoint's representation. A missing/unconfigured deployment identity fails closed.
4. This endpoint is an existing identity/admission check: `crates/workspaces/src/api/bench.rs:122-142` resolves verified caller, scope, owner's bench, and membership. Its response includes ID/owner (`:77-78`). `/v1/bench` is expressly excluded from bench-tool routes (`crates/workspaces/src/api/mod.rs:225-245`, particularly :238). Add negative integration evidence that the actual child bench-tool credential is rejected; do not merely decode JWT claims.
5. Authenticate every control request, with no positive auth cache in the first version. Discard the credential after validation. Platform denial denies access; platform unavailability returns service-unavailable, never silently falls back to session naming or a cached grant. Preserve existing read-only/revocation policy rather than treating a successful metadata response alone as write authorization.
6. Prefer authenticated bounded HTTP `GET events?after=...` for operation replay in this first bridge; it avoids a new long-lived credential/revalidation problem. Existing general /events may carry only an operation-changed notification; fetch sensitive operation details through authenticated controls. Do not broadcast approval records or private arguments through the unauthenticated general event path.

No new UI credential is necessary for this minimal bridge. The broad desktop credential crosses into the already trusted bench process for online validation, so handling must be tightly scoped. A future platform-minted operation-control token can reduce that exposure, but is not required for this integration. Do not read a desktop credential from bench configuration or inherit it into children.

### Child authentication

Bench mints at least 32 random bytes per child incarnation; keep only a verifier and binding in a private in-memory registry. Inject the credential at spawn solely into that child's extension transport. Binding: boot epoch, session ID, incarnation, role (main/worker/info/fork), trusted workspace/tree scope, permitted public operation actions, expiry. No mint route callable by a model. Strip any inherited child credential before spawning another child; each receives only its own.

Verify every operation model request against this registry and current session record. Resolve session/owner/team from the binding, never the body. Derive deduplication identity from bound session plus accepted turn/tool-call identity. Turn revision comes from the bench's authoritative user-input path, and must be checked against the revision captured when the call originated, not relabelled with a later revision on arrival. O08 needs a trusted per-turn context handoff or registered call identity; session credential alone is not turn freshness.

Revoke on child exit/replacement, archive/removal, expiry, and incompatible mode transition. Bench restart invalidates every old child credential; resumed children receive new ones. Durable operations are independent of these credentials. Scope is not a sandbox against arbitrary code execution in the trusted bench process; credential isolation prevents model arguments and another session's handle from claiming authority.

### Endpoint roles

Keep frozen O01 endpoint names; apply these role rules rather than changing model fields:

| Action | Authenticated child | Authenticated owner UI | Trusted in-process adapter |
| --- | --- | --- | --- |
| Start / exact / describe | Eligible live main session, current mode/scope/revision | Not required by this bridge | Coordinator |
| Inspect / bounded events | Own bound session's operations | All owned operations in this bench, including archived-session operations | Coordinator |
| Resume additional input | Own live session; revision/provenance checks; cannot grant approval | Only if explicitly supported by frozen interface | Coordinator |
| Cancel | Own live session's operations | Owned operation even after archive/disable | Coordinator |
| Record user approval/preference | Never | Current owner authentication plus immutable decision binding | None impersonating a user |
| Record automatic policy decision | Never | UI configuration can be conveyed through authenticated bridge, not as model claims | Trusted policy adapter records source/version/binding |

Legacy `/proposals/:id` must not resolve an operation decision. Use separate durable decision identity/namespace and reject child bearer on user-only decision routes. Additional-input text containing yes, a forged record ID, or a boolean grants nothing. Decisions bind actor, tenant, original session, operation/step, payload digest, operation/request revision, expiry, source and consume/replay state.

### Restart, archive and feature mode

Persist operation ownership and decision evidence, never bearer credentials. Owner reauthenticates with the desktop credential after restart; controls do not depend on a live child, TypeSafe, Flash, or feature enablement. Archive revokes child authority, not owner controls. Do not auto-transfer an archived operation to a new session. Safe disable stops new routing and leaves inspect/events/cancel/decision endpoints mounted. An owner decision may be recorded for an archived operation, but does not itself re-enable archived model resume or initiate new dispatch; resumption must use an explicitly permitted recovery path.

Membership/login revocation follows the existing person-only bench admission check. If that check denies access, authenticated operator recovery is separate; do not invent a bypass. Both model-provider outages remain recoverable; platform authentication outage is a distinct limitation and returns a clear control-plane error.

## Immediate O02 / O05 contract boundary

O02: policy-bearing dispatch takes a server-created TrustedExecutionContext (actor/tenant/bench/session/incarnation/role/workspace/tree, request and turn revisions, mode/capability gates, abort/deadline) plus validated arguments. A DecisionBridge persists pending immutable decisions and returns pending/denied/granted-reference state; it must work without ExtensionContext widgets. No raw handler entrypoint is exported to executor. Revalidate exact payload digest and current authorization before dispatch.

O05: persist stable owner/tenant/session identity, accepted turn/request revision and call identity, durable decisions and consumption state, original approved digest and evidence, budgets, unknown outcomes, and event sequence. Never persist credentials or derive owner solely from a live SessionRow. Credential issuance/verification belongs to O08; O05 consumes a verified principal, never raw headers. Durable control authorization and operation storage survive feature disable and session archive. UI projections cannot manufacture grants.

O08 must cover child registration, RPC/user-turn ingress, desktop main-process transport, operation-specific HTTP routes, and decision/event UI bridge. Existing-file ownership for `harness/src/main.ts`, `bench-client.ts`, and renderer policy handoff must be coordinated with O09 before coding. This is necessary integration scope, not a change to the approved instruction interface.

## Trust limitations and required evidence

The bench, installed extension, desktop main process, platform identity endpoint and deployment configuration are trusted. This is not isolation from same-UID compromise or malicious extension code. Existing broad unauthenticated bench routes remain a wider legacy perimeter; they must not expose new operation records or alias operation decision APIs. If arbitrary legacy routes can directly recover the new private credential registry or write its authority, the bridge is invalid.

Required cases: child tool token cannot authenticate UI controls; forged tunnel header/session ID fails; wrong owner/team/bench fails; child from another incarnation/session fails; missing current-turn binding fails; model cannot record decision; legacy proposal answer cannot resolve operation decision; archived child resume fails while owner inspect/cancel works; restart invalidates child secrets and preserves durable records; disabled mode plus unavailable providers still permits owner controls; revocation/platform-check failure never becomes approval; credentials absent from logs/events/model payloads.
