# Intercept by proxy pod, not by endpoint rewriting

Status: design, 2026-09-14. Supersedes the mechanism in §3 of
`2026-09-08-service-intercept-design.md`. Everything else in that spec — the wish in
`Environment.spec.intercepts`, one workspace per service, the grace, what removes a wish — stands
unchanged.

## The bug this fixes

An intercept today makes the service's ClusterIP selector-less and hand-writes an `EndpointSlice`
naming the intercepting workspace's POD IP, in another namespace
(`k8s::intercept_slice`, `controller/environment/run.rs` phase 4). Reaching that pod needs a
NetworkPolicy pair, and the pair `intercept_policies` writes admits exactly ONE namespace: the
environment's (`k8s::intercept_egress` selects the env namespace's pods; `k8s::intercept_ingress`
admits `namespaceSelector: env_ns`).

Since spaces, an environment is followed by every member of every space that points at it
(`SpaceEnvironment`, `k8s::space_egress`/`space_ingress`). A teammate's workspace or bench in
`ws-bob` dialling `api:8080` resolves the environment's ClusterIP fine, kube-proxy DNATs it to the
workspace pod in `ws-alice` — and `ws-alice`'s ingress policy does not admit `ws-bob`. The packet is
dropped. The service works for the environment's own pods and silently fails for every follower,
which is exactly the collaboration case the intercept exists for.

**Rejected: fan-out grants.** Writing an `intercept_ingress` peer per following space would make a
workspace's ingress a function of a cluster-wide set that changes whenever anybody switches spaces —
more writers on one object, more stale-cache deletes of the F1/F3 class the space vetting already
found, and a policy that grows without bound. Owner's ruling: "will open up more issues."

## The fix

Put a **proxy pod in the environment's namespace** and let Kubernetes do the rest.

Per service in force: a Pod `intercept-{service}` in `env-{id}` labelled
`SERVICE_LABEL: {service}` plus `KIND_LABEL: intercept`, running one TCP listener per declared
service port, each forwarding to the workspace's remapped port. The ClusterIP Service **keeps a
selector** — now selecting the proxy pod instead of the StatefulSet's. The real StatefulSet is still
scaled to 0 (the queue-consumer reason from the old spec is unchanged: a service that acts on its
own must not keep acting while its traffic goes elsewhere).

Everything the old mechanism fought then disappears. The endpoints are Kubernetes' own, so no
hand-written slice, no abandoned `Endpoints` to delete (`services::drop_abandoned_endpoints` goes),
no union of our slice with the controller's. The traffic a follower sends is DNAT'd to a pod **in
the environment's namespace**, which every follower already reaches through its own `space_egress`
grant — so the cross-space bug is fixed by construction, with no grant per follower. Only the
proxy needs to reach the workspace, and that is the existing fixed pair, now scoped to one pod
instead of the whole namespace.

**The "never a proxy" rule is superseded.** The old spec refused a proxy because it read
"protocol-aware proxy, HTTP-only, deployed into every environment and owned forever". This is not
that: a byte-for-byte TCP forwarder, protocol-blind (mongodb and redis work exactly as HTTP does),
one pod that exists only while an intercept is in force, no routing rules and no configuration a
person ever sees. The property that rule was protecting — works for any protocol — is kept.

## The forwarder

New binary `bins/intercept-proxy` → `kloudlite-intercept-proxy`, musl static, its own Dockerfile
stage and its own image, pinned by `deploy/pin.sh` exactly as `kloudlite-builder-gate` is
(`image.yml` `--bin`, the pin loop, `k3s/*.yaml`). Tokio, no other dependency beyond what the
workspace already builds.

- **Args**: `--forward {listen}:{target_port}` repeated, and `--target {host}`. Nothing else. No
  config file, no reload — the spec of an intercept is immutable while it runs; a change to ports
  or workspace is a new pod.
- **Per port**: one `TcpListener`. Bind failure at startup is fatal (exit non-zero) — a half-bound
  proxy that answers on 8080 and refuses 9229 is worse than a pod that will not start, because the
  Service would go Ready with half its ports dead.
- **Per connection**: resolve `{target}:{port}` per connection (never once at boot — the target
  Service's ClusterIP is stable but the name must survive a target recreate), then
  `tokio::io::copy_bidirectional`. That is the whole data path: no buffering beyond its own, no
  parsing, no TLS termination, no logging of payload — only accept/close counters and errors, with
  the peer address, never the bytes.
- **Half-close** comes free with `copy_bidirectional`, which shuts each direction down as its
  source ends. It matters: a client that half-closes to signal end-of-request (plain HTTP/1.0, some
  RPC framings) hangs forever without it.
- **Bounded concurrency**: a `Semaphore` of `--max-conns` (default 512) around accepted
  connections; over the bound the accept loop waits rather than spawning. A workspace dev server is
  not a fleet service, and an unbounded accept loop turns one loop in a caller into an OOM in the
  environment's namespace.
- **Idle timeout**: `--idle-secs` (default 600) of no bytes in either direction closes the
  connection. Workspace pods are restarted often; without this every restart leaks a half-open
  socket per port until the proxy dies.
- **Readiness** is the listener being up. No health endpoint and no probe port: a `readinessProbe`
  of `tcpSocket` on the first forwarded port is Kubernetes' own check and costs no code. That is
  also what makes the Service's endpoint appear only once the proxy can actually accept.
- **Resources**: 10m / 32Mi requests, 200m / 128Mi limits — it copies bytes. Counted against the
  environment's namespace `ResourceQuota` like everything else; no `Quota` dimension of its own.
- **Security context**: `hardened()` verbatim — non-root (uid 1000), read-only root, `drop: ALL`,
  `RuntimeDefault` seccomp. It opens no file. `runtimeClassName` is taken from the same
  `PodContext.runtime_class` the environment's services use, so it runs under gvisor exactly when
  they do — a pod in the data path must not be the one thing in the namespace outside the sandbox.

## Target addressing: a Service in the workspace namespace

Two options were on the table.

(a) Bake the workspace's pod IP into the proxy's args. Simple, and wrong in the ordinary case: a
workspace pod restarts (an edit to `spec.packages`, an OOM, a node reboot) and the IP moves. The
controller would have to watch that pod and recreate or patch the proxy on every change — the same
"pod IP read live, never stored" rule that already governs `decide_intercept` — and between the
restart and the controller's next pass the proxy forwards to whoever holds that IP now. That last
part is not a gap, it is a misdelivery to another tenant's pod.

(b) A ClusterIP Service `intercept-target-{ws}` in the WORKSPACE namespace, selecting
`WORKSPACE_LABEL: {ws}`, with one port per intercepted workspace-side port. The proxy dials
`intercept-target-{ws}.{ws_ns}.svc.cluster.local`. A workspace pod restart is then Kubernetes'
problem and nothing of ours moves.

**Recommend (b).** Both policy and DNS permit it:

- The proxy's `resolv.conf` is the environment namespace's, so its search path does not contain
  `ws-alice` — the FQDN is mandatory, not a preference, and it is what the args carry.
- `allow-dns` in the environment's namespace already permits CoreDNS, and CoreDNS answers for any
  namespace. Nothing new is needed to resolve it.
- `allow_internet_egress` excludes 10/8, so the ClusterIP is unreachable by default — as it is
  today for the pod IP. The existing `intercept_egress` covers it unchanged: **NetworkPolicy is
  evaluated after DNAT**, on the backend pod's address, so a rule whose peer is
  `namespaceSelector: ws_ns` AND `podSelector: WORKSPACE_LABEL` admits traffic sent to that
  Service's ClusterIP. This is the one fact the whole recommendation rests on; the fleet probe
  below is what holds it.

One workspace-side Service per workspace, not per service, so a workspace serving two of an
environment's services has one object; its ports are `intercepted_ports(e, plan, ws)`, the same
union the ingress policy is scoped to, named `p{port}`.

## Objects, ownership and who collects them

| Object | Namespace | Owner | Collected by |
| --- | --- | --- | --- |
| `intercept-{service}` Pod | `env-{id}` | the Environment | ownerRef on env delete; the controller on release |
| ClusterIP `{service}` (selector switched) | `env-{id}` | the Environment | unchanged |
| `intercept-target-{ws}` Service | `ws-{owner}` | the **Workspace** | ownerRef on workspace delete; the controller on release |
| `intercept-{ws}` policy pair | both | env side / workspace side | unchanged |

An ownerReference may not cross namespaces, which is why the workspace-side Service is owned by the
Workspace — the identical split `intercept_ingress` already makes. The release path in
`intercept_policies` already reaches into the workspace's namespace to delete the ingress half,
holding the Workspace it needs; the target Service is deleted on the same line, by the same name
rule, and `forget_applied` is called for it the way the vetting demands of every delete.

## Behaviour

**Single writer.** The region controller owns the proxy pod, the Service selector, the StatefulSet
scale and both grants — see `2026-09-14-region-controller-design.md`. Nothing here changes who
decides; `decide_intercept` and its grace are untouched. This spec defines only the objects and
their behaviour.

**Order, in force**: create the target Service, create the proxy pod, wait for it Ready, THEN switch
the ClusterIP's selector and scale the StatefulSet to 0. The old spec's ordering rule inverted —
there the slice had to exist before the selector went, here the proxy must be Ready before the
selector moves — but the same principle: the service must never be without a ready endpoint.
**Order, releasing**: selector back to the StatefulSet's labels, scale up, then delete the proxy and
the target Service.

**Status**: `intercepted_by` keeps its meaning exactly — the workspace serving this service, `None`
when the intercept is not in force. One field is added to `ServiceStatus`:

```rust
/// The proxy pod backing this intercept: `starting` until it is Ready, `ready` once the
/// selector points at it, `failed` when the pod is Failed or its container cannot start
/// (an image pull, most often). Absent when the service is not intercepted.
pub proxy: Option<String>,   // "ready" | "starting" | "failed"
```

The web reads `intercepted_by` as it does today and shows `proxy` beside it, so "my intercept is on
but nothing answers" has an answer on the page rather than in a controller log.

**Failure modes.** A crashed proxy leaves the Service with no ready endpoint: callers get connection
refused immediately, which is the honest answer — never a silent misdelivery, the failure mode the
pod-IP option carried. `restartPolicy: Always`, so a crash is seconds. An image pull failure is
`proxy: failed` with the pod's own message; the intercept stays wished for and the real service
stays at 0 — deliberately, since flapping the StatefulSet on a registry outage is worse, and the
status says exactly what to fix. A stopped or unreachable workspace is unchanged: grace, then
release, then the real service back.

**Port remap** is preserved end to end: the ClusterIP still publishes `p{port}` at the service's own
port; the proxy listens on that port and forwards to `ic.workspace_port(p)`; the target Service
publishes the workspace-side port. `invalid_port_map` and the 7788 refusal are unchanged and still
run before anything is rendered.

**TCP only.** The forwarder is TCP; a UDP service port is refused at `/v1` with 422 naming the port,
beside the existing port checks, rather than rendering an intercept that drops half a protocol.
(Nothing declares UDP today — `service_clusterip` hard-codes TCP — so this is a guard for when
`model::Service` grows a protocol, not a migration.)

**Performance**: one extra hop, in-node when kube-proxy lands the connection on the proxy's node and
one more otherwise. Byte copying in the kernel's page cache through `copy_bidirectional`; the added
latency is under a millisecond against a hop that already crossed a node boundary to reach a
workspace. Not measured yet — the `env.intercept` probe's 120 s ceiling has three orders of
magnitude of headroom, and a real number belongs in the fleet write-up, not here.

**Security.** The proxy's egress policy is `intercept_egress` narrowed from `podSelector: {}` to the
proxy pod alone: it may reach the target workspace pod and nothing else. That is strictly tighter
than today, where every pod in the environment could dial the workspace directly. Workspace ingress
is unchanged — the same pod selector, the same port list, the same union — and 7788 is still refused
at `/v1` and by `invalid_port_map`, so the tool server is unreachable from the environment by two
independent guards.

## Migration

Intercepts in force on the fleet are converted in place by the controller's ordinary pass, with no
flag and no downtime window: it renders the target Service and the proxy, waits Ready, puts the
selector back (a selector-less Service gaining one is an ordinary update), and deletes the
hand-written `{service}-intercept` slice. Mixed builds are safe in both directions — an old agent
finds a Service with a selector and a slice it did not write, and its next pass rewrites both to its
own shape; a new agent finds a stale slice and deletes it. Neither leaves the service without
endpoints, because in both shapes the Service has a ready backend throughout. The delete of the
legacy slice is kept in the code for one release and then goes with
`k8s::intercept_slice`, `drop_abandoned_endpoints` and the `Endpoints` delete.

## Probes

`env.intercept`, `env.intercept.fallback` and `env.intercept.refused` keep their ids, ceilings and
meanings — the mechanism changed, the promise did not. Two are added
(`bins/slo/src/stages/environment.rs`, `deploy/slo.md`, held equal by the catalogue test):

- **`env.intercept.peer`** — a SECOND space, following the same environment, reaches the intercepted
  service on the remapped port. This is the bug, as a probe; it would have failed every hour since
  spaces shipped.
- **`env.intercept.proxy.restart`** — delete the intercepting workspace's POD and assert the service
  answers again without the controller touching the proxy. That is option (b)'s whole claim, and
  the only way to catch a regression to baked-in addressing.

## Tests

- Forwarder unit tests against a stub target on a loopback port: bytes round-trip, the remap lands
  on the right target port, a half-close propagates, an idle connection is dropped at the deadline,
  and the semaphore bounds in-flight connections. No fixtures, no framework — a `tokio::test` and a
  `TcpListener`.
- `k8s` render tests beside `tests/environment.rs`: the proxy pod's args are one `--forward` per
  declared port with the remapped target, the ClusterIP keeps a selector and it names the proxy, the
  target Service's ports are the workspace-side union, and the proxy's egress policy peer is the
  workspace pod only.
- Controller reconcile tests in `bins/agent/tests/reconcile/`: in-force order (target, proxy,
  selector, scale 0), release order reversed, a not-Ready proxy leaves the selector on the real
  StatefulSet, and a release deletes the workspace-side Service and forgets its apply hash.
- The `deploy/slo.md` ↔ catalogue equality test covers the two new ids.

## Open questions

1. Should the proxy be a Deployment rather than a bare Pod? A Pod matches the workspace's own shape
   and a crash restarts in place; a Deployment would survive a node loss without the controller.
   Leaning Pod, since the controller is already the thing that repairs it.
2. `--idle-secs` 600 is a guess. It should probably be a `ClusterSettings` `Mark::Live` field once
   somebody reports a long-poll being cut.
3. Does anything rely on the intercepted Service's `Endpoints` object naming the workspace IP
   directly (a dashboard, a debug path)? Nothing in this repo greps as doing so.
4. One proxy pod per service, or one per intercept-holding workspace with all its listeners? Per
   service is simpler to release and to read in `kubectl get pod`; per workspace is fewer pods in a
   heavily intercepted environment.
