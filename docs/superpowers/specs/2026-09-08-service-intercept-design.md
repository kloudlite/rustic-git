# Intercepting an environment's service with an attached workspace

Status: draft for review, 2026-09-08. Revised after review: the real service is stopped while
intercepted, a stopped workspace releases the intercept, the wish lives on the Environment, and
ports may be remapped. The mechanism in §3 was verified end to end on `centralindia-k3s` before
this draft was written.

## Why

A workspace attached to an environment can already CALL the environment's services by bare name
(`mongodb://mongodb:27017`). The other direction is missing: you cannot put the code you are
editing in the path of the environment's own traffic. Today, to see your change answer a sibling
service you have to build an image, push it, and restart the environment's StatefulSet — a loop of
minutes, on every edit, for a change you already have running in your workspace.

Interception closes that loop. While `api` is intercepted by a workspace, the environment's own
`api` is stopped and every connection to `api:8080` lands on the workspace's process instead.
Nothing is rebuilt and nothing is redeployed; you edit, and the next request is yours.

## Global on purpose

While an intercept is on, **all** of that service's traffic goes to the workspace, a teammate's
requests included. That is the point, not a limitation: the environment is where people try things
together, and an intercept is how you put your unbuilt change in front of them. A colleague hitting
the environment reaches your workspace without doing anything or knowing anything, and you watch
their request arrive in your own terminal. Only one workspace may hold a service at a time, and a
second is refused.

The alternative — a per-developer split, where only requests carrying a marker (an HTTP header,
say) reach your workspace — would defeat that: your collaborators would have to know the marker and
set it, and by default they would keep hitting the old code.

It is also far more machinery. Routing on a marker means something in the path that reads and
understands the traffic — a protocol-aware proxy deployed into every environment and owned
forever — and it would only ever work for HTTP, never for mongodb or redis, which have no headers
to route on. This design changes only which address a name resolves to, so it works for any
protocol and adds nothing to run.

## Decisions taken

| Question | Answer |
| --- | --- |
| What is intercepted | one SERVICE of the environment, named. Not the whole environment. |
| Where the wish lives | `Environment.spec.intercepts: [{service, workspace, ports}]`, written only by `/v1`. On the environment and not the workspace, so validating and reconciling it is one object's own business — the alternative made every environment reconcile read every workspace to find out what it was supposed to do. |
| The real service while intercepted | **stopped**: its StatefulSet is scaled to 0. Leaving it running is wrong for anything that acts on its own rather than only answering — a queue consumer would take messages your workspace never sees, a scheduler would fire twice, two writers would race the same data. The cost is a cold start on release, and it is the right trade. |
| How traffic is directed | endpoints, not DNS and not a proxy. See §3; verified on the cluster. |
| Ports | **remappable**. The service's port stays what callers dial; each one names the port on the workspace that answers it, defaulting to the same number. `api:8080 → workspace:3000` is the ordinary case, because the process you are debugging listens where your dev server listens, not where the deployed image did. |
| A stopped or unreachable workspace | the wish **stays**; the intercept simply stops being in force. Status says so, and the REAL service is brought back up — StatefulSet off 0, Service selector restored, slice deleted. When the workspace returns, the intercept takes hold again with nobody re-asking. |
| What removes the wish | an explicit release (`DELETE …/intercepts/{service}`) and nothing else — not a stop, not a detach, not a delete of the workspace. The wish is the person's, and a transient blip must not silently discard what they asked for. A wish whose workspace is gone for good shows as `WorkspaceGone` with a Release button; it costs nothing but a line of spec. |
| How fast the fallback is | seconds. The environment's controller WATCHES workspaces (mapped to the environment through `attachedEnvironment`), so it reconciles on the transition rather than waiting for a beat. |
| Flapping | a fallback waits `INTERCEPT_GRACE_SECS` (30) of continuous unreachability before scaling the real service back up, so an ordinary pod restart does not bounce the StatefulSet up and down. |
| Who may intercept | the caller must be able to act on the workspace AND on the environment, and the workspace must be attached to that environment. Acting on the environment alone is not enough: an intercept redirects traffic INTO someone's workspace. |
| Two workspaces, one service | refused at `/v1` with 409 naming the holder. |
| Quota | none. An intercept allocates nothing; it moves traffic and stops a StatefulSet. |

## Design

### 1. The wish (`crates/workspaces/src/crd/mod.rs`)

```rust
pub struct Intercept {
    /// A service of THIS environment, by name.
    pub service: String,
    /// The workspace id serving it, which `/v1` has checked is attached here.
    pub workspace: String,
    /// Which port on the workspace answers each of the service's ports. A service port with no
    /// entry is answered on the same number.
    #[serde(default)]
    pub ports: Vec<PortMap>,
}

pub struct PortMap {
    /// One of the service's own declared ports — what callers dial. Unchanged by an intercept.
    pub service: u16,
    /// The port the workspace listens on.
    pub workspace: u16,
}
```

`EnvironmentSpec.intercepts: Vec<Intercept>`, `#[serde(default)]` so every stored object still
parses. At most one entry per `service`; `/v1` is what holds that, and the controller takes the
first so a hand-edited object cannot make it flap.

`ServiceStatus` gains `intercepted_by: Option<String>` — what is actually in force, so the web and
the CLI show the truth rather than the wish.

### 2. `/v1` (`crates/workspaces/src/api/environments.rs`)

- `POST /v1/environments/{id}/intercepts`, body
  `{ "service": "api", "workspace": "ws-…", "ports": [{ "service": 8080, "workspace": 3000 }] }`
  → 202. Refusals, one sentence each: 404 if the environment has no such service; 404 if the
  workspace is not one the caller may act on; 409 if it is not attached to this environment; 409
  if it is not running; 409 if another workspace already intercepts that service, naming it; 422
  if a `ports` entry names a port the service does not declare, or names one twice.
- `DELETE /v1/environments/{id}/intercepts/{service}` → 204, idempotent. This is the ONLY thing
  that removes a wish. `stop_ws`, `detach_ws` and `delete_ws` deliberately do not touch
  `intercepts`: stopping a workspace for the night must not throw away what you asked for, and a
  controller bringing the real service back covers the traffic in the meantime.

### 3. How traffic is directed

**The Service loses its selector and the controller writes the `EndpointSlice` itself.**

A `Service` with a selector has its endpoints maintained by Kubernetes, and the selector can only
ever match pods in the Service's OWN namespace — so it can never name a workspace, which lives in
`ws-{owner}`. A Service with no selector is the supported way to say "these exact addresses", and
kube-proxy programs them without caring which namespace the address belongs to. The ClusterIP and
the DNS name are untouched, so callers keep dialling `api:8080` and notice nothing.

The port mapping lives in the slice: the Service's `ports[].port` is what callers dial, and the
`EndpointSlice`'s `ports[].port` is where it is delivered. They are matched by NAME, and
`service_clusterip` already names every port `p{port}`.

**Verified on `centralindia-k3s`, 2026-09-08**, in the live `devstack` environment:

| step | result |
| --- | --- |
| `Service itest`, port 9099, no selector | ClusterIP allocated, selector empty |
| `EndpointSlice` → workspace pod `10.42.3.231`, port **3000** | accepted |
| From `mongodb-0`: dial `itest:9099` | answered `from-workspace` |

A real environment pod reached a listener inside a workspace, in another namespace, with the port
remapped. Every test object was removed afterwards.

**Rejected alternatives.** `ExternalName` is a DNS CNAME: it cannot remap ports at all, it only
redirects clients that re-resolve DNS (a cached address or a live connection keeps hitting the old
place), and toggling it destroys and reissues the ClusterIP. A proxy pod in the environment's
namespace would work, but it is an image to ship, run, patch and secure in every environment, plus
a network hop, and it still has to learn the workspace's IP — the same reconcile problem with more
moving parts.

### 4. The environment controller (`bins/agent/src/controller/environment.rs`)

Everything it needs is in its own spec. Each service is in one of THREE states, and the third is
the one that makes the wish safe to keep:

- **Not intercepted** — exactly today's rendering: the StatefulSet at its declared replicas and
  `service_clusterip` with its selector. Any `EndpointSlice` this controller wrote for that service
  is deleted.
- **Intercepted and in force** — the workspace is running and its pod is `Ready`. The StatefulSet
  is applied with `replicas: 0`, the `Service` is written with `selector: None` and the same ports,
  and an `EndpointSlice` named `{service}-intercept`, labelled
  `kubernetes.io/service-name={service}`, holds one endpoint: the workspace pod's IP, with a port
  entry per declared service port, named `p{service port}` and carrying the mapped workspace port.
  The IP is read live — one GET of the Workspace for its `status.podRef`, one GET of that pod — and
  never stored in status, for the reason `bins/gateway/src/resolve.rs` already gives: a pod IP
  changes on every recreate, and a stale one in status is a wrong answer that looks right.
  `status.services[].intercepted_by` names the workspace.
- **Intercepted but NOT in force** — the workspace is stopped, deleted, detached, or its pod has
  been unreachable for `INTERCEPT_GRACE_SECS`. The wish stays in spec, untouched. The rendering
  goes back to the first state entirely: the real StatefulSet at its declared replicas, the
  selector restored, the slice deleted. `intercepted_by` is `None` and the `Intercepted` condition
  says why — `WorkspaceStopped`, `WorkspaceGone`, `WorkspaceDetached`, `PodUnreachable`.

The grace is what stops a flap: a workspace pod restarting is unreachable for a few seconds, and
scaling the real service up and down around every restart would be worse than the gap. Inside the
grace the slice is left as it is and the reconcile requeues; only sustained unreachability falls
back.

Coming back is the same path in reverse and needs nobody to re-ask: the workspace becomes `Ready`,
the watch in §5 fires, and the service is intercepted again.

### 5. Reacting in seconds, not minutes (`bins/agent/src/controller/run.rs`)

The environment `Controller` gains one more `.watches(...)`: every `Workspace`, mapped to the
environment named by its `attachedEnvironment`. A workspace stopping, starting, losing its pod or
being deleted therefore reconciles the environment that intercepts it immediately, instead of
waiting for a tick.

The mapper is a field read and costs no API call, and the agent already holds `watch` on
workspaces cluster-wide, so this needs no new grant. It is the same shape as the four `.watches`
the environment controller already carries.

There is no api-side beat and no api-side watch: nothing about an intercept needs `/v1` after the
wish is written, and `crates/api`'s RBAC comment that "nothing in the API watches" stays true.

### 6. Network policy (`crates/workspaces/src/k8s.rs`)

Attachment opens workspace → environment. Interception needs environment → workspace, and the
2026-09-08 experiment confirmed that direction is DENIED today: a dial straight to the workspace
pod's IP from an environment pod was refused, because `allow_internet_egress` deliberately excludes
private address space. Adding the two policies below made both the direct dial and the service dial
succeed immediately. This section is load-bearing, not defensive.

Written only while that workspace holds at least one intercept, named `intercept-{ws}`:

- in the environment's namespace, egress to the workspace pod (the namespace-and-pod selector in
  ONE `to` element, for the same AND-not-OR reason `attach_ingress` documents);
- in the workspace's namespace, ingress from the environment's namespace to that pod.

Owned the way the attach pair is: the workspace-side one by the Workspace, the environment-side one
by the Environment, because an ownerReference cannot cross namespaces.

### 7. RBAC and admission (`deploy/k3s/agent-rbac.yaml`, `deploy/k3s/agent-admission.yaml`)

The agent gains `endpointslices` `create`, `patch`, `delete` in `discovery.k8s.io`, and the
admission policy's DELETE fence gains `endpointslices` beside `services` — the agent must not be
able to delete one outside a namespace it reconciles. It already has `services: create, patch,
delete` and `statefulsets: patch`, so scaling to 0 needs no new grant.

### 8. Web

The environment page lists its services; each row gains an "Intercept" control offering the
viewer's attached workspaces and a port for each of the service's ports, and shows "intercepted by
`<workspace>`, service stopped" with a Release button when one is in force. The workspace page
shows what it is intercepting. Both read `status`, never the wish.

### 9. Probe

Three hourly ids in stage "6 · Environment", feature "Environments". One journey reports all
three, because the expensive part — an environment, an attached workspace and a listener — is
shared, and because the release path is only meaningful against an intercept that worked:

| id | sli | target |
| --- | --- | --- |
| `env.intercept` | An intercepted service answers from the attached workspace on a remapped port | `p95(120_000)` |
| `env.intercept.released` | Stopping the workspace releases the intercept on its own, and the real service answers again | `p95(180_000)` |
| `env.intercept.refused` | An intercept of an unattached workspace, and one naming a port the service does not declare, are both refused | `avail(99.9)` |

The journey: an environment whose one service echoes a known string; an attached workspace running
a listener on a DIFFERENT port that echoes a different string.

- `env.intercept` — intercept with a port mapping, dial the service by its own name and port from
  inside the environment, assert the workspace's string. The remap is in the probe because it is
  the ordinary case, not an edge one.
- `env.intercept.released` — STOP the workspace rather than releasing by hand, then poll until the
  service answers its own string again. This is the one that matters most: it covers `/v1`'s clear,
  the StatefulSet coming back off 0, the Service regaining its selector and the `EndpointSlice`
  being deleted — the whole path that, if it breaks, leaves an environment answering nothing with
  no one having asked for that. A hand release would not exercise the automatic path at all.
- `env.intercept.refused` — two refusals against the same environment: a workspace that is not
  attached (409) and a port mapping naming a port the service does not declare (422). Cheap, and
  they are the guards that stop an intercept pointing traffic somewhere nobody authorised.

`NotCached`-style states are not probed: an intercept whose workspace pod is merely restarting is
transient by construction and cannot be held still long enough to sample.

### 10. Failure modes

| Failure | Behaviour |
| --- | --- |
| Workspace pod restarting, briefly | inside `INTERCEPT_GRACE_SECS` nothing moves; the intercept resumes when the pod is `Ready` again |
| Workspace stopped, deleted or detached | the wish STAYS; within seconds the real service is back up and answering, and status says why the intercept is not in force |
| The workspace comes back | intercepted again automatically; nobody re-asks |
| Two workspaces claim one service | `/v1` refuses the second, naming the holder |
| A port mapping names a port the service does not declare | 422 at `/v1`; nothing written |
| The environment is stopped | its StatefulSets and Services go as today; the wish survives and takes effect when it starts |
| Agent cannot read the workspace pod at all (API error, not a missing pod) | previous rendering left alone, requeued — an unreadable answer is not evidence of anything |
| No NetworkPolicy engine on the cluster | the policies are inert and traffic flows anyway; already true of the attach pair |

## Out of scope

Per-developer intercepts (see "Global on purpose"); intercepting a service in an environment the
workspace is not attached to; intercepting from outside the cluster; TLS termination or protocol
awareness — this is L4, the workspace gets the bytes; more than one environment per workspace,
which `attachedEnvironment` already forbids.

## Files

`crates/workspaces/src/crd/mod.rs` (`Intercept`, `PortMap`, `EnvironmentSpec.intercepts`,
`ServiceStatus.intercepted_by`, regenerated `crds.yaml`),
`crates/workspaces/src/api/environments.rs` (two routes),
`crates/workspaces/src/api/mod.rs` (routes),
`bins/agent/src/controller/run.rs` (the Workspace watch on the environment controller),
`crates/workspaces/src/k8s.rs` (the selector-less Service, the `EndpointSlice`, the two
policies), `bins/agent/src/controller/environment.rs` (the per-service branch and the scale to 0),
`deploy/k3s/agent-rbac.yaml`, `deploy/k3s/agent-admission.yaml`, `web/apps/web` (the two controls),
`bins/slo/src/stages/environment.rs` + `crates/workspaces/src/slo/catalogue.rs` + `deploy/slo.md` +
`web/apps/web/src/lib/fixtures/superadmin.ts` (the row), `CLAUDE.md`.
