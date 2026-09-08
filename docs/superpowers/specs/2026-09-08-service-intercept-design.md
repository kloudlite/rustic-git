# Intercepting an environment's service with an attached workspace

Status: draft for review, 2026-09-08.

## Why

A workspace attached to an environment can already CALL the environment's services by bare name
(`mongodb://mongodb:27017`). The other direction is missing: you cannot put the code you are
editing in the path of the environment's own traffic. Today, to see your change answer a sibling
service you have to build an image, push it, and restart the environment's StatefulSet — a loop of
minutes, on every edit, for a change you already have running in your workspace.

Interception closes that loop. While `api` is intercepted by a workspace, every connection an
environment pod makes to `api:8080` lands on the workspace's own process instead of the
StatefulSet's. Nothing is rebuilt and nothing is redeployed; you edit, and the next request is
yours.

## What it is not

A **global** intercept: while it is on, ALL of that service's traffic goes to the workspace. There
is no header-based or per-developer split (Telepresence's "personal intercept"), which needs a
request-aware proxy in the path and is a much larger thing. Two people cannot intercept the same
service at once, and the second is refused rather than queued.

## Decisions taken

| Question | Answer |
| --- | --- |
| What is intercepted | one SERVICE of the environment, named. Not the whole environment. |
| Where the wish lives | `Workspace.spec.intercepts: [service names]`, exactly where `attachedEnvironment` lives, written only by `/v1` |
| Who renders it | the ENVIRONMENT's controller, which already owns that Service and reads its attached workspaces by the `ATTACHED_ENV_LABEL` selector |
| The mechanism | while intercepted, the service's `Service` is written with NO selector and the controller writes the `EndpointSlice` itself, naming the workspace pod's IP. A selector-ful Service has its endpoints managed by Kubernetes and cannot point across namespaces; a selector-less one is the supported way to say "these exact addresses". |
| The real StatefulSet | keeps running, untouched. Releasing an intercept is then one Service rewrite, seconds, with no cold start — and the pod's own data and connections are never disturbed. |
| The workspace pod is not running | the intercept holds and the `EndpointSlice` is written EMPTY: connections fail fast with "no endpoints" rather than silently reaching the real service. An intercept that quietly falls back is worse than one that is visibly down — the whole point is that your code is in the path. |
| Ports | the service's declared ports, unchanged, to the same port numbers on the workspace. No remapping. |
| Who may intercept | the caller must be able to act on the workspace AND the workspace must be attached to that environment. The environment's own owner is not enough: an intercept redirects traffic INTO someone's workspace. |
| Two workspaces, one service | refused at `/v1` with 409 naming the holder. If the controller ever sees two (a write race), it picks the lexicographically first workspace id and says so in the condition — deterministic beats arbitrary. |
| When it ends | an explicit release, a detach, or the workspace being deleted. `/v1` clears it on detach and delete; the controller ALSO drops an intercept whose workspace is gone or no longer attached, and restores the real endpoints. Keep-biased in the direction of the service working. |
| A stopped workspace | the intercept holds, empty. Starting the workspace restores it without re-asking. |

## Design

### 1. The wish (`crates/workspaces/src/crd/mod.rs`)

`WorkspaceSpec.intercepts: Vec<String>`, `#[serde(default)]` so every stored object still parses.
Each entry is a service name in the workspace's `attachedEnvironment`, validated by the same rule
service names are validated by at create. An intercept with no `attachedEnvironment` is refused at
`/v1` and ignored by the controller — the field is meaningless without one.

The label `ATTACHED_ENV_LABEL` already exists on an attached workspace and is what makes the
environment controller's read a selector rather than a cluster-wide scan.

### 2. `/v1` (`crates/workspaces/src/api/workspaces.rs`)

- `POST /v1/workspaces/{id}/intercept` body `{ "service": "api" }` → 202.
  Refuses: 409 if the workspace is not attached, 404 if the environment has no such service,
  409 if another attached workspace already intercepts it (naming which), 409 over quota — no,
  an intercept allocates nothing and is not quota'd.
- `DELETE /v1/workspaces/{id}/intercept/{service}` → 204, idempotent.
- `detach_ws` and `delete_ws` clear `intercepts` in the same patch that clears the attachment, so
  a detached workspace never leaves a service pointing at it.

The route is on the WORKSPACE, like `attach`, because the workspace is the actor and the thing the
person names; the environment is found from `attachedEnvironment`.

### 3. The environment controller (`bins/agent/src/controller/environment.rs`)

Once per reconcile, list workspaces with `ATTACHED_ENV_LABEL={env id}` and build
`intercepts: HashMap<service, workspace id>` from their `spec.intercepts`, dropping any whose
workspace is not `Ready` — no: dropping on readiness would flap the Service every time a workspace
restarted. Keep the intercept whatever the workspace's phase; readiness decides only what goes IN
the `EndpointSlice`. A list failure leaves the previous rendering alone and requeues, like every
other read here.

For each service:

- **Not intercepted** — exactly today's rendering: `service_clusterip` with its selector, and any
  `EndpointSlice` this controller wrote for it is deleted.
- **Intercepted** — `service_clusterip` with `selector: None` and the same ports, plus an
  `EndpointSlice` named `{service}-intercept`, labelled
  `kubernetes.io/service-name={service}`, holding one endpoint: the workspace pod's IP, `ready`
  true, with a port entry per declared port. The IP is read live from the pod named by the
  workspace's `status.podRef` — never stored in status, for the reason
  `bins/gateway/src/resolve.rs` already gives: a pod IP changes on every recreate and a stale one
  in status is a wrong answer that looks right. No pod, no IP, or a pod that is not `Ready` → the
  slice is written with an empty `endpoints` list.

`status.services[].intercepted_by: Option<String>` records what is actually in force, so the web
and the CLI show the truth rather than the wish. An `Intercepted` condition on the Environment
carries the summary and the reason when a wish could not be honoured (`WorkspaceGone`,
`WorkspaceNotRunning`, `Conflicting`).

### 4. Network policy (`crates/workspaces/src/k8s.rs`)

Attachment opens workspace → environment. Interception needs environment → workspace, which today
is denied at both ends. Two more policies, written only while that workspace holds at least one
intercept and named `intercept-{ws}`:

- in the environment's namespace, egress to the workspace pod (the namespace-and-pod selector in
  ONE `to` element, for the same AND-not-OR reason `attach_ingress` documents);
- in the workspace's namespace, ingress from the environment's namespace to that pod.

They are owned the same way the attach pair is: the workspace-side one by the Workspace, the
environment-side one by the Environment, because an ownerReference cannot cross namespaces.

### 5. RBAC and admission (`deploy/k3s/agent-rbac.yaml`, `deploy/k3s/agent-admission.yaml`)

The agent gains `endpointslices` `create`, `patch`, `delete` in `discovery.k8s.io`, and the
admission policy's DELETE fence gains `endpointslices` beside `services` — the agent must not be
able to delete an `EndpointSlice` outside a namespace it reconciles, exactly as for Services.
Nothing else changes; the agent already has `services: create, patch, delete`.

### 6. Web

The environment page lists its services; each row gains an "Intercept" control when the viewer has
an attached workspace, and shows "intercepted by `<workspace>`" with a Release button when one is
in force. The workspace page shows what it is intercepting. Both read `status`, never the wish, so
what is displayed is what is actually carrying traffic.

### 7. Probe

One hourly id in stage "6 · Environment", feature "Environments":

| id | sli | target |
| --- | --- | --- |
| `env.intercept` | A service intercepted by an attached workspace answers from that workspace, and answers from the real service again when released | `p95(120_000)` |

The step stands up an environment whose one service echoes a known string, attaches a workspace,
runs a listener in the workspace that echoes a DIFFERENT string, intercepts, connects to the
service by name from inside the environment and asserts the workspace's string, then releases and
asserts the original string returns. Both halves matter: an intercept that never releases is a
broken environment, and it is the release path that a stale `EndpointSlice` breaks.

### 8. Failure modes

| Failure | Behaviour |
| --- | --- |
| Workspace stopped or restarting | empty `EndpointSlice`; callers get "connection refused" fast; restored when the pod is `Ready` again |
| Workspace deleted while intercepting | the controller drops the intercept, restores the selector, deletes the slice; `/v1` also clears it at delete |
| Detached while intercepting | same, and `/v1` clears the field in the detach patch |
| Two workspaces claim one service | `/v1` refuses the second; a raced pair resolves to the lexicographically first, reported as `Conflicting` |
| The environment is stopped | its Services go with its StatefulSets, as today; the wish survives on the workspace and takes effect when it starts |
| Agent cannot list workspaces | previous rendering left alone, requeued — never a silent restore of the real service |
| No NetworkPolicy engine on the cluster | the policies are inert and traffic flows anyway; the intercept still works, and this is already true of the attach pair |

## Out of scope

Header-based or per-developer intercepts; intercepting a service in an environment the workspace is
not attached to; port remapping; intercepting from outside the cluster; TLS termination or
protocol awareness (this is L4 — the workspace gets the bytes); intercepting more than one
environment from one workspace, which `attachedEnvironment` already forbids.

## Files

`crates/workspaces/src/crd/mod.rs` (`intercepts`, `ServiceStatus.intercepted_by`, regenerated
`crds.yaml`), `crates/workspaces/src/api/workspaces.rs` (two routes, and the detach/delete clears),
`crates/workspaces/src/api/mod.rs` (routes), `crates/workspaces/src/k8s.rs` (the selector-less
Service, the `EndpointSlice`, the two policies), `bins/agent/src/controller/environment.rs` (the
read and the per-service branch), `deploy/k3s/agent-rbac.yaml`, `deploy/k3s/agent-admission.yaml`,
`web/apps/web` (the two controls), `bins/slo/src/stages/environment.rs` +
`crates/workspaces/src/slo/catalogue.rs` + `deploy/slo.md` +
`web/apps/web/src/lib/fixtures/superadmin.ts` (the row), `CLAUDE.md`.
