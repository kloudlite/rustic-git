# Service Intercept Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** While a service is intercepted, its environment's StatefulSet is scaled to 0 and every connection to its ClusterIP is delivered to an attached workspace, on a port the caller chooses.

**Architecture:** The wish is `Environment.spec.intercepts`, written only by `/v1` and removed only by an explicit release. The environment's controller renders it three ways: not intercepted, intercepted and in force (StatefulSet at 0, selector-less Service, an `EndpointSlice` it writes naming the workspace pod's IP and the mapped port), or intercepted but not in force (the wish kept, the real service brought back, status saying why). A Workspace watch on that controller makes the transition take seconds.

**Tech Stack:** Rust (kube, k8s-openapi), Next.js, the SLO probe, k3s RBAC and ValidatingAdmissionPolicy.

**Spec:** `docs/superpowers/specs/2026-09-08-service-intercept-design.md`

## Global Constraints

- `/v1` is the ONLY writer of `spec`. A controller never writes an intercept and NEVER clears one — the admission policy enforces it, and the design depends on it: a stopped workspace must not lose the person's wish.
- An intercept not in force is a STATUS fact, never a spec edit. The real service comes back up; the wish stays exactly as written.
- The Service's own `ports[].port` is NEVER changed by an intercept — callers keep dialling what they always dialled. Only the `EndpointSlice`'s port differs.
- Service ports and EndpointSlice ports are matched by NAME. `service_clusterip` already names every port `p{port}`; the slice must use the same names.
- Keep-biased everywhere: any read failure leaves the previous rendering alone and requeues. Never a silent restore of the real service, never a delete on a guess.
- The workspace pod IP is read live on every reconcile and never stored in status (`bins/gateway/src/resolve.rs` gives the reason).
- At most one intercept per service. `/v1` holds it; the controller takes the first entry if it ever sees two.
- House style: comments say why; no new dependencies; commit subjects imperative sentence case with no trailers.
- `INTERCEPT_GRACE_SECS` = 30. A workspace unreachable for less than that changes nothing — an ordinary pod restart must not bounce the real StatefulSet up and down.
- Tasks 1 and 2 are sequential (2 consumes 1's types). Tasks 3, 4 and 6 may run in parallel after 2. Task 5 is last but one; Task 7 is last.

---

### Task 1: The CRD

**Files:**
- Modify: `crates/workspaces/src/crd/mod.rs`, `deploy/k3s/crds.yaml` (regenerated, never hand-edited)
- Test: same file's `mod tests`

**Interfaces:**
- Produces: `crd::Intercept { service: String, workspace: String, ports: Vec<PortMap> }`,
  `crd::PortMap { service: u16, workspace: u16 }`, `EnvironmentSpec.intercepts: Vec<Intercept>`
  (`#[serde(default)]`), `ServiceStatus.intercepted_by: Option<String>`
  (`#[serde(default, skip_serializing_if = "Option::is_none")]`).
- Produces: `impl Intercept { pub fn workspace_port(&self, service_port: u16) -> u16 }` — the
  mapped port, or `service_port` when nothing names it.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn a_port_with_no_mapping_is_answered_on_the_same_number() {
    let i = Intercept {
        service: "api".into(),
        workspace: "ws-1".into(),
        ports: vec![PortMap { service: 8080, workspace: 3000 }],
    };
    assert_eq!(i.workspace_port(8080), 3000, "the mapped one");
    assert_eq!(i.workspace_port(9090), 9090, "an unmapped port keeps its number");
}

/// Every stored Environment predates this field and must still parse.
#[test]
fn an_environment_without_intercepts_still_parses() {
    let v = serde_json::json!({"owner":"a","team":"","name":"n","region":"r","services":[],"desiredState":"running"});
    let s: EnvironmentSpec = serde_json::from_value(v).unwrap();
    assert!(s.intercepts.is_empty());
}
```

- [ ] **Step 2: Run** `cargo test -p kloudlite-workspaces intercept` → FAIL (types not defined).
- [ ] **Step 3: Implement** the two structs (deriving what every sibling in this file derives, including `JsonSchema`), the two fields, and `workspace_port`.
- [ ] **Step 4: Regenerate the CRDs** — `CRD_REGEN=1 cargo test -p kloudlite-workspaces --test crd_yaml` — and confirm `deploy/k3s/crds.yaml` gained `intercepts` under the Environment's spec and `interceptedBy` under its service status.
- [ ] **Step 5: Run** `cargo test -p kloudlite-workspaces && cargo clippy --workspace --all-targets -- -D warnings` → PASS.
- [ ] **Step 6: Commit** `"Intercepts are part of an environment's desired state"`.

### Task 2: `/v1`

**Files:**
- Modify: `crates/workspaces/src/api/environments.rs` (`set_intercept`, `clear_intercept`, a shared `validate`), `crates/workspaces/src/api/mod.rs` (routes)
- Test: `crates/workspaces/tests/api_intercept.rs` (new; mirror `api_packages.rs`'s harness)

**Interfaces:**
- Consumes: Task 1's `Intercept`, `PortMap`.
- Produces: `POST /v1/environments/{id}/intercepts` → 202 with the environment doc; `DELETE /v1/environments/{id}/intercepts/{service}` → 204. `env_doc` gains `intercepts: [{service, workspace, ports}]`.
- Deliberately NOT produced: anything that clears an intercept when a workspace stops, detaches or is deleted. `DELETE …/intercepts/{service}` is the only removal. Do not touch `stop_ws`, `detach_ws` or `delete_ws`.

- [ ] **Step 1: Failing HTTP tests** — one per refusal and one per success:
  unknown service → 404; workspace the caller may not act on → 404; attached elsewhere → 409;
  not running → 409; already intercepted by another → 409 naming it; a port the service does not
  declare → 422; the same service port twice → 422; a good request → 202 and the CR's
  `spec.intercepts` holds one entry with the mapping; a second POST for the SAME workspace and
  service replaces rather than duplicates; DELETE → 204 and the entry is gone; DELETE of one that
  is not there → 204. AND one that pins the rule: stopping the workspace leaves `spec.intercepts`
  untouched — assert the field still holds the entry after `stop_ws`.
- [ ] **Step 2: Run** `cargo test -p kloudlite-workspaces --test api_intercept` → FAIL (404, no route).
- [ ] **Step 3: Implement.** One `validate` used by the POST, returning the refusal `Response` directly so every arm reads as one sentence. Nothing in `workspaces.rs` changes.
- [ ] **Step 4: Run** `cargo test -p kloudlite-workspaces && cargo clippy --workspace --all-targets -- -D warnings` → PASS.
- [ ] **Step 5: Commit** `"Api: an environment service can be intercepted by an attached workspace"`.

### Task 3: The rendering helpers

**Files:**
- Modify: `crates/workspaces/src/k8s.rs` (`service_clusterip` gains an intercept argument, new `intercept_slice`, `intercept_egress`, `intercept_ingress`, `intercept_policy_name`)
- Test: same file's `mod tests`

**Interfaces:**
- Produces:
  - `service_clusterip(svc, env_id, owner, owner_ref, intercepted: bool) -> Option<CoreService>` — identical to today except `selector: None` when `intercepted`.
  - `intercept_slice(svc: &model::Service, env_id: &str, owner: &str, owner_ref: &OwnerReference, ic: &crd::Intercept, pod_ip: Option<&str>) -> EndpointSlice` — named `{svc.name}-intercept`, labelled `kubernetes.io/service-name={svc.name}`, `addressType: "IPv4"`, one port per declared service port named `p{port}` carrying `ic.workspace_port(port)`, and `endpoints` holding the one address when `pod_ip` is `Some` and EMPTY when it is `None`.
  - `intercept_egress(env_ns, ws_ns, ws_id, owner, owner_ref) -> NetworkPolicy` and `intercept_ingress(ws_ns, env_ns, ws_id, owner, owner_ref) -> NetworkPolicy`, both named `intercept_policy_name(ws_id)` = `intercept-{ws_id}`, shaped exactly like `attach_egress`/`attach_ingress` with the direction reversed — the namespace-and-pod selector in ONE element.

- [ ] **Step 1: Failing tests** — the slice's ports carry the MAPPED number while the Service's carry the dialled one; an unmapped port appears with its own number; `pod_ip: None` yields an empty `endpoints` list but still the ports; the intercepted Service has no selector and the un-intercepted one does; the egress policy's `to` has exactly ONE element carrying both selectors (the AND-not-OR rule `attach_ingress` documents).
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4:** `cargo test -p kloudlite-workspaces k8s && cargo clippy --workspace --all-targets -- -D warnings` → PASS.
- [ ] **Step 5: Commit** `"Render an intercepted service, its endpoints and its policies"`.

### Task 4: The environment controller

**Files:**
- Modify: `bins/agent/src/controller/environment.rs`, `bins/agent/src/controller/run.rs` (one more `.watches`)
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: Task 3's helpers, Task 1's `Intercept`.

- [ ] **Step 1: Failing tests** in `reconcile.rs`, copying the shape of the existing environment tests. One per state and one per transition:
  (a) intercept + a running, `Ready` workspace → StatefulSet PATCHed to `replicas: 0`, Service PATCHed with no selector, `EndpointSlice` written with the workspace pod's IP and the MAPPED port;
  (b) the wish removed → `replicas` restored, selector restored, slice DELETEd;
  (c) **the wish KEPT but the workspace stopped** → `spec.intercepts` untouched (assert no PATCH to the Environment's spec at all), `replicas` restored, selector restored, slice DELETEd, `intercepted_by` `None` and the `Intercepted` condition reading `WorkspaceStopped`;
  (d) the workspace's pod missing for LESS than `INTERCEPT_GRACE_SECS` → nothing moves, the reconcile requeues;
  (e) a Workspace GET that ERRORS (not a missing workspace) → previous rendering left alone and requeued.
- [ ] **Step 2: Run** `cargo test -p kloudlite-agent-bin intercept` → FAIL.
- [ ] **Step 3: Implement.** Build `HashMap<service, &Intercept>` from `e.spec.intercepts`, first
  entry wins per service. Per service, branch across the spec's §4 three states. The pod IP comes
  from one GET of the Workspace (for `status.podRef`) and one GET of that pod. Distinguish
  "unreachable" (stopped, gone, detached, pod not `Ready`) from "unreadable" (an API error): the
  first falls back after the grace, the second changes nothing. Write
  `status.services[].intercepted_by` and the `Intercepted` condition with the reason.
- [ ] **Step 4: Wire the watch** in `run.rs`: the environment `Controller` gains
  `.watches(Api::<crd::Workspace>::all(...), watcher::Config::default(), |w| …)` mapping a
  workspace to the environment named by its `attachedEnvironment` (a field read, no API call,
  `None` when it is unattached). This is what makes the fallback take seconds. Follow the shape of
  the four `.watches` already on that controller.
- [ ] **Step 5: Run** `cargo test -p kloudlite-agent-bin && cargo clippy --workspace --all-targets -- -D warnings` → PASS.
- [ ] **Step 6: Commit** `"Agent: an intercept in force stops the real service, and one that is not brings it back"`.

### Task 5: RBAC and admission

**Files:**
- Modify: `deploy/k3s/agent-rbac.yaml`, `deploy/k3s/agent-admission.yaml`

There is no api-side beat and no api-side watch in this design: once the wish is written, nothing
about an intercept needs `/v1`. `crates/api`'s RBAC comment that "nothing in the API watches" stays
true, and the api gains no verb.

- [ ] **Step 1:** the agent's ClusterRole gains `discovery.k8s.io/endpointslices` `create, patch, delete`, with the header table updated — that table IS the role, so a verb missing from it is a lie. Say in the comment what bounds the delete: only a slice this controller wrote, in a namespace it reconciles.
- [ ] **Step 2:** the admission policy's DELETE fence gains `endpointslices` beside `services`, so the agent cannot delete one outside a namespace it reconciles.
- [ ] **Step 3:** confirm no other grant is needed — the agent already holds `watch` on workspaces cluster-wide (used by Task 4's watch) and `patch` on statefulsets (used for `replicas: 0`). State that in the commit body.
- [ ] **Step 4: Commit** `"Let the agent write the endpoints an intercept needs"`.

### Task 6: The web

**Files:**
- Modify: `web/apps/web/src/lib/api.ts` (`setIntercept`, `clearIntercept`, `ApiEnvironment.intercepts`, `ApiService.intercepted_by`)
- Create: `web/apps/web/src/components/env/intercept-control.tsx`, and the action beside the environment page's existing actions file
- Test: `web/apps/web/src/lib/intercept.test.ts` for the pure helper

**Interfaces:**
- Produces: `setIntercept(token, envId, {service, workspace, ports})`, `clearIntercept(token, envId, service)`, and a pure `interceptSummary(service, intercepts)` → `{ heldBy: string | null; ports: {service: number; workspace: number}[] }`.

- [ ] **Step 1: Failing test** for `interceptSummary`: a service with no intercept is `heldBy: null`; one with an intercept reports the workspace and the mapping; an unmapped port appears with equal numbers.
- [ ] **Step 2: Run** `bun test` → FAIL. **Step 3: Implement** the helper, the two api calls, the control (a dialog listing the viewer's attached workspaces and a port input per declared service port, defaulting to the service's own number), and the row's "intercepted by X, service stopped" state with a Release button. Read `status`, never the wish. Copy the destructive-confirm shape from the repo settings page for Release.
- [ ] **Step 4: Run** `bun run typecheck && bun run lint && bun run test` → PASS.
- [ ] **Step 5: Commit** `"Web: intercept a service from its environment page"`.

### Task 7: The probe and docs

**Files:**
- Modify: `bins/slo/src/stages/environment.rs`, its stage id list and dispatch, `crates/workspaces/src/slo/catalogue.rs`, `deploy/slo.md`, `web/apps/web/src/lib/fixtures/superadmin.ts`, `CLAUDE.md`

**Catalogue rows** (all `Suite::Hourly`, stage `"6 · Environment"`, feature `"Environments"`):

| id | sli | target |
| --- | --- | --- |
| `env.intercept` | An intercepted service answers from the attached workspace on a remapped port | `p95(120_000)` |
| `env.intercept.fallback` | Stopping the workspace brings the real service back on its own, and the intercept is still in the environment's spec | `p95(180_000)` |
| `env.intercept.refused` | An intercept of an unattached workspace, and one naming a port the service does not declare, are both refused | `avail(99.9)` |

ONE journey reports all three — the environment, the attached workspace and the listener are the
expensive part and are shared, and the release is only meaningful against an intercept that worked.
Follow the shape `experience_ws.rs`'s pinned-package journey uses: the first id creates and the
later ids skip with a reason when it did not.

- [ ] **Step 1:** read `environment.rs` for how it stands up its environment and what the workspace stage leaves in `c.state`. The journey needs an environment service that echoes a known string and an attached workspace running a listener on a DIFFERENT port echoing another. Use the same `ws_exec` helper the workspace stage uses to start the listener; `bun` is on the workspace's PATH and `Bun.serve` is the shortest listener that stays up. Start it with `nohup … &` so it survives the exec returning.
- [ ] **Step 2 (`env.intercept`):** intercept with a port mapping, dial the service by its OWN name and port from inside the environment (a sibling service's pod, the way `env.dns` already dials), assert the workspace's string. Skip with a reason when there is no kubeconfig, no environment, or no attached workspace.
- [ ] **Step 3 (`env.intercept.fallback`):** STOP the workspace through `/v1` — never a hand release — then poll the same dial until the service answers its OWN string again, within the ceiling. This covers the StatefulSet coming back off 0, the Service regaining its selector and the slice being deleted. Then assert BOTH halves of the rule: `status` no longer reports `intercepted_by` for that service, AND the environment's `spec.intercepts` STILL holds the entry. The second assertion is the one that catches a regression back to clearing the wish, which would silently discard what the person asked for.
- [ ] **Step 4 (`env.intercept.refused`):** against the same environment, two refusals — a workspace that is not attached (409) and a `ports` entry naming a port the service does not declare (422) — asserting the STATUS and that the body names what was wrong. Independent of the other two, so it runs even when the create failed.
- [ ] **Step 5:** add the three ids to the stage's id list, its dispatch and its exactly-once test; add the rows to all three catalogues, byte-identical.
- [ ] **Step 6:** `CLAUDE.md`, one paragraph in "Workspaces and environments" after the attach paragraph: what an intercept is, that the StatefulSet is scaled to 0, that the Service goes selector-less with an agent-written `EndpointSlice`, that ports may be remapped, and that a stopped workspace releases it.
- [ ] **Step 7:** `cargo test -p kloudlite-workspaces slo && cargo test -p kloudlite-slo-bin && cargo clippy --workspace --all-targets -- -D warnings && cd web && bun run test` → PASS.
- [ ] **Step 8: Commit** `"Probe: a service can be intercepted, released and refused"`.

## Self-review

Spec §1 → Task 1; §2 → Task 2; §3 → Tasks 3 and 4 (the mechanism is helpers plus the controller);
§4 → Task 4; §5 → Task 5; §6 → Task 3 (helpers) and Task 4 (application); §7 → Task 5; §8 → Task 6;
§9 → Task 7; §10's failure table → Task 2's refusals, Task 4's empty-slice and requeue arms, and
Task 5's drop rules. Names consistent across tasks: `Intercept`, `PortMap`, `workspace_port`,
`intercepts`, `intercepted_by`, `intercept_slice`, `intercept_egress`, `intercept_ingress`,
`intercept_policy_name`, `clear_intercepts_of`, `release_dead_intercepts`, `intercepts_to_drop`,
`setIntercept`, `clearIntercept`, `interceptSummary`, `env.intercept`, `env.intercept.fallback`,
`env.intercept.refused`, `INTERCEPT_GRACE_SECS`.
