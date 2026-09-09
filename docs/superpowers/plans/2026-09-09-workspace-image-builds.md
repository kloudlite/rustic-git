# Workspace Image Builds Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `docker build` and `docker push` work inside a workspace, against a per-owner buildkit that runs only while building, and a team member can push to the team's registry.

**Architecture:** The builder is a hidden `Environment` (`bld-{owner}`, `spec.system: "builder"`) rendered by the existing environment controller; a region-wide gate starts it on the first buildx connection through two internal api routes and stops it after idle; the api projects a 24 h registry token into the `user-key` Secret it already writes; the registry consults `may_act` on the miss path.

**Tech Stack:** Rust (axum, kube, tokio), moby/buildkit:rootless, busybox/alpine workspace image, the SLO probe, k3s manifests.

**Spec:** `docs/superpowers/specs/2026-09-09-workspace-image-builds-design.md`

## Global Constraints

- `bins/api` is the ONLY writer of any CR's spec. The gate writes no CR; it calls `/v1/internal/builders/{slug}/{start,stop}`.
- A `system` environment is invisible to `/v1` and the web: omitted from lists, 404 from every other environment route. Only the two internal routes and the internal GET may touch one, by slug.
- No non-transient `Snapshot` is ever written for a builder. `push` on one is a 404.
- `hardened()` is not changed. The builder runs under the region's `runtime_class` unless Task 1 proves it cannot; only then does a `system` environment's service get `runtimeClass: none`, and only through the api.
- Registry: `may_act` runs only on the miss path (caller is not the owner and the image is not a public pull). `Source::Unavailable` is DENIED.
- Names, exactly: builder id `bld-{slug}`, service `buildkit`, port `1234`, mount folder `cache` at `/cache`, gate Service `builder-gate.kloudlite-system.svc:1234`, Secret key `registry-token`, credential helper `docker-credential-kl`, policies `allow-builder-gate`, env vars `BUILDKIT_HOST`, settings `builder_idle_secs` (600), `builder_start_secs` (120), `builder_cache_gb` (50), api secret env `KLOUDLITE_BUILDER_SECRET`.
- Every probe id is read by RESULT after a roll (`slo.step.done` / `slo.step.skipped` in `default.otel_logs`); a skip is a hole, never a pass.
- Commit subjects imperative sentence case, no tool attribution; the commit-msg hook rejects the string "CLAUDE.md" — write "the project guide".
- House style: comments say why; `// ponytail:` where a ceiling is accepted.

---

### Task 1: Spike — rootless buildkit under gvisor

**Files:**
- Create: `deploy/dev/spike/buildkit-gvisor.yaml` (deleted at the end of the task; the RESULT is recorded in the plan ledger and in Task 2's brief)

- [ ] **Step 1:** Write the pod, in an existing throwaway namespace on `centralindia-k3s` (the probe's `ws-slo-probe` is fine — it is swept by name prefix, so name it `run-spike-buildkit`). Run it once more on a second node afterwards (`nodeName` set by hand) only if the first run fails, to tell a node from a kernel:

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: run-spike-buildkit
  namespace: ws-slo-probe
spec:
  runtimeClassName: gvisor
  # No node pin: every node on the region takes every kind (the session/env split is gone;
  # both labels are on every node), so the scheduler's pick is as real as any.
  restartPolicy: Never
  containers:
    - name: buildkit
      image: moby/buildkit:v0.18.2-rootless
      args: ["--addr", "tcp://0.0.0.0:1234", "--oci-worker-no-process-sandbox", "--root", "/cache"]
      env: [{name: BUILDKITD_FLAGS, value: "--oci-worker-no-process-sandbox"}]
      securityContext:
        runAsUser: 1000
        runAsGroup: 1000
        allowPrivilegeEscalation: false
        capabilities: {drop: ["ALL"]}
        seccompProfile: {type: RuntimeDefault}
      volumeMounts: [{name: cache, mountPath: /cache}]
    - name: client
      image: moby/buildkit:v0.18.2-rootless
      command: ["sleep", "3600"]
      securityContext: {runAsUser: 1000, allowPrivilegeEscalation: false, capabilities: {drop: ["ALL"]}}
  volumes: [{name: cache, emptyDir: {}}]
```

- [ ] **Step 2:** `KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/dev/spike/buildkit-gvisor.yaml`, wait for Running, then from the `client` container:

```sh
printf 'FROM alpine:3.20\nRUN echo hello > /hello\n' > /tmp/Dockerfile
buildctl --addr tcp://127.0.0.1:1234 build --frontend dockerfile.v0 --local context=/tmp --local dockerfile=/tmp --output type=tar,dest=/tmp/out.tar && tar tf /tmp/out.tar | grep -c '^hello$'
```
Expected: `1`. Run it TWICE: the second must be faster and log `CACHED`.

- [ ] **Step 3:** Record the ruling in the ledger: `Ruling: builder runs under gvisor — <pass/fail>, <the exact error if fail>`. On fail, retry once with `seccompProfile: {type: Unconfined}` and `runtimeClassName` removed and record THAT outcome; Task 2 Step 4 then takes the fallback branch.
- [ ] **Step 4:** `kubectl delete -f …` and `git rm` the yaml. Commit `"Record the buildkit-under-gvisor spike"` with the ruling in the message body.

### Task 2: The builder shape in the CRD, the model and the pod renderer

**Files:**
- Modify: `crates/workspaces/src/crd/mod.rs` (`EnvironmentSpec`)
- Modify: `crates/workspaces/src/model.rs` (`Service`)
- Modify: `crates/workspaces/src/k8s.rs` (`service_statefulset`, `env_unit_resources` use)
- Modify: `deploy/k3s/crds.yaml` (regenerate)
- Test: `crates/workspaces/src/k8s.rs` tests, `crates/workspaces/tests/crd_yaml.rs`

**Interfaces:**
- Produces: `EnvironmentSpec.system: Option<String>` (serde `default`, `skip_serializing_if = "Option::is_none"`, rename `system`); `Service.resources: Option<crd::PodResources>` (same serde shape); `Service.runtime_class: Option<String>` ONLY if Task 1 failed; `pub const BUILDER_SYSTEM: &str = "builder";` in `crd`; `pub fn builder_id(slug: &str) -> String` returning `format!("bld-{slug}")` in `crd`.

- [ ] **Step 1: Write the failing tests.** In `k8s.rs` tests:

```rust
#[test]
fn a_service_with_its_own_resources_is_rendered_with_them_and_the_unit_otherwise() {
    let mut svc = svc("cache"); // the tests' existing helper
    svc.resources = Some(crd::PodResources::default());
    let sts = service_statefulset(&svc, "bld-alice", "bld-alice", "alice", &pod_ctx()).unwrap();
    let c = &sts.spec.unwrap().template.spec.unwrap().containers[0];
    assert_eq!(c.resources.as_ref().unwrap().limits.as_ref().unwrap()["cpu"].0, "4");
    let plain = service_statefulset(&svc_without_resources(), "e", "e", "alice", &pod_ctx()).unwrap();
    let p = &plain.spec.unwrap().template.spec.unwrap().containers[0];
    assert_eq!(p.resources.as_ref().unwrap().limits.as_ref().unwrap()["cpu"].0, "2");
}
```
and in `crd_yaml.rs` add `assert!(crds_yaml.contains("system:"))` beside the existing `intercepts` assertion. Run: `cargo test -p kloudlite-workspaces resources_is_rendered` → FAIL (no field).

- [ ] **Step 2: Implement.** `EnvironmentSpec.system`, `Service.resources`, `builder_id`, `BUILDER_SYSTEM`. In `service_statefulset`, `resources: Some(quantities(svc.resources.as_ref().unwrap_or(&env_unit_resources())))`. Also thread `resources` through `crd::SnapshotState` where services are frozen (it is `Vec<model::Service>`, so it comes for free — assert that in the existing state round-trip test).
- [ ] **Step 3:** `CRD_REGEN=1 cargo test -p kloudlite-workspaces --test crd_yaml`; `cargo test -p kloudlite-workspaces`; `cargo clippy --workspace --all-targets -- -D warnings` → PASS.
- [ ] **Step 4 (only if Task 1 failed):** `Service.runtime_class: Option<String>`; `service_statefulset` sets `runtime_class_name` to `None` when it is `Some("none")`, and sets `seccomp_profile: Unconfined` + `app_armor_profile: Unconfined` on that container only. A test that a plain service still gets the ctx's runtime class.
- [ ] **Step 5: Commit** `"An environment service may carry its own resources, and an environment may be the platform's"`.

### Task 3: The registry consults team membership

**Files:**
- Modify: `crates/registry/src/auth.rs` (`allow`)
- Test: `tests/registry_http.rs`

**Interfaces:**
- Consumes: `App::may_act(&self, user: &str, owner: &str) -> Result<bool>` (`crates/app/src/lib.rs:216`).

- [ ] **Step 1: Write the failing test** in `tests/registry_http.rs`, beside the existing push tests, using the test app's directory fixture (see how `team_member_can_push_over_ssh`-style tests in `tests/` seed membership — copy that seeding):

```rust
#[tokio::test]
async fn a_team_member_can_push_a_team_image_and_a_stranger_cannot() {
    let (app, base) = registry_app_with_team("acme", &["alice@x"]).await;
    // alice: member
    let r = put_manifest(&base, "acme", "web", basic("alice", &token_for(&app, "alice").await)).await;
    assert_eq!(r.status(), 201);
    // bob: authenticated, not a member
    let r = put_manifest(&base, "acme", "web", basic("bob", &token_for(&app, "bob").await)).await;
    assert_eq!(r.status(), 403);
    assert!(r.text().await.unwrap().contains("DENIED"));
    // directory down: denied, never allowed
    app.directory_unavailable(true);
    let r = put_manifest(&base, "acme", "web", basic("alice", &token_for(&app, "alice").await)).await;
    assert_eq!(r.status(), 403);
}
```
Run: `cargo test --test registry_http a_team_member_can_push` → FAIL (403 for alice).

- [ ] **Step 2: Implement** in `allow()`, after the `public` check and before the scope error:

```rust
    // Team membership, live: the rule git-over-SSH has used since identity moved to the
    // fingerprint. Only on the miss path, so an owner's own push and every public pull cost
    // what they cost today. `Source::Unavailable` is an Err here and lands in DENIED below —
    // a directory outage must never widen who may write.
    if let Some(u) = who.as_deref() {
        if app.may_act(u, owner).await.unwrap_or(false) {
            return Ok(who);
        }
    }
```
- [ ] **Step 3:** `cargo test --test registry_http`; clippy → PASS.
- [ ] **Step 4: Commit** `"Registry: a team member may push to the team's image"`.

### Task 4: The credential in the pod, and the tools in the image

**Files:**
- Modify: `crates/workspaces/src/api/keys.rs` (`project`: `registry-token` in `user-key`)
- Modify: `crates/workspaces/src/k8s.rs` (`login_env`: `BUILDKIT_HOST`)
- Modify: `Dockerfile` (target `workspace`): `docker-cli`, `docker-cli-buildx`, `docker-credential-kl`, `/etc/profile.d/kl-build.sh`
- Test: `keys.rs` tests, `k8s.rs` tests

**Interfaces:**
- Consumes: `Jwt::mint_registry(&self, owner: &str, scope: &str, ttl_secs: u64) -> Result<String>` (`crates/core/src/jwt.rs:154`); `ApiState.jwt`.
- Produces: env `BUILDKIT_HOST=tcp://builder-gate.kloudlite-system.svc:1234`; file `/etc/kloudlite/ssh/registry-token`.

- [ ] **Step 1: Failing tests.** In `keys.rs`: the projected Secret's `data` carries `registry-token` whose value verifies with `jwt.verify_registry` as the owner and expires within 86_400 s. In `k8s.rs`: `login_env("ws-1")` contains `BUILDKIT_HOST` with exactly the value above. Run both → FAIL.
- [ ] **Step 2: Implement.** In `keys::project`, where the `user-key` Secret is server-side applied, add `("registry-token", s.jwt.mint_registry(owner, "*", 86_400)?)`. The beat already re-projects every `KEYS_RESYNC_SECS`, so rotation needs no code. In `login_env`, one `var("BUILDKIT_HOST", "tcp://builder-gate.kloudlite-system.svc:1234".into())` with a comment: the gate, never a daemon in the pod.
- [ ] **Step 3: Image.** In the `workspace` target: `apk add --no-cache docker-cli docker-cli-buildx`; write `/usr/local/bin/docker-credential-kl` (0755):

```sh
#!/bin/sh
# docker's credential-helper protocol: the action is argv[1], the server URL is stdin.
# The token is the api's, re-minted every resync beat; nothing here is long-lived.
set -eu
case "${1:-}" in
  get)
    read -r _server
    printf '{"Username":"%s","Secret":"%s"}\n' "${KL_OWNER:?}" "$(cat /etc/kloudlite/ssh/registry-token)"
    ;;
  store|erase) cat >/dev/null ;;
  *) echo "unsupported: $1" >&2; exit 1 ;;
esac
```
and `/etc/profile.d/kl-build.sh` (NOT the seeded rc files — a person's edits to those survive, and this must not):

```sh
# Builds go to the owner's builder through the gate; the credential helper is the login.
if [ -n "${BUILDKIT_HOST:-}" ] && command -v docker >/dev/null 2>&1; then
  mkdir -p "$HOME/.docker"
  [ -e "$HOME/.docker/config.json" ] || printf '{"credHelpers":{"%s":"kl"}}\n' "${KL_REGISTRY_HOST:?}" > "$HOME/.docker/config.json"
  docker buildx inspect kl >/dev/null 2>&1 || docker buildx create --name kl --driver remote "$BUILDKIT_HOST" --use >/dev/null 2>&1 || true
fi
```
`KL_OWNER` and `KL_REGISTRY_HOST` are two more `login_env` vars: the pod's owner slug and the registry host the api already knows (`KLOUDLITE_REGISTRY_HOST`, the value `registry::auth::realm()` derives its host from — thread it into `PodContext`).
- [ ] **Step 4:** `cargo test -p kloudlite-workspaces`; clippy; build the image target locally in the dev pod (`docker buildx build --target workspace .` via the pod's buildkitd) and `docker run --rm <img> sh -lc 'docker buildx version && docker-credential-kl erase </dev/null'` → both succeed.
- [ ] **Step 5: Commit** `"A workspace carries the tools to build and the credential to push"`.

### Task 5: The api owns the builder: create, hide, start, stop, prune, quota

**Files:**
- Modify: `crates/workspaces/src/api/environments.rs` (`list_env` filter, a `visible_env` guard, `ensure_builder`, internal routes)
- Modify: `crates/workspaces/src/api/workspaces.rs` (`create_ws` calls `ensure_builder`)
- Modify: `crates/workspaces/src/api/keys.rs` (`prune_builders` in `run_beat`)
- Modify: `crates/workspaces/src/api/mod.rs` (router: the three internal routes behind `require_builder_secret`)
- Modify: `crates/workspaces/src/quota.rs` (`usage`: skip `system` for the count), `crates/workspaces/src/crd/mod.rs` (`default_quota`), `crates/workspaces/tests/crd_yaml.rs`
- Modify: `bins/api/src/main.rs` (`KLOUDLITE_BUILDER_SECRET`, required in the `user` role)
- Test: `environments.rs`, `quota.rs`, the api's route tests

**Interfaces:**
- Produces: `pub(crate) async fn ensure_builder(s: &ApiState, owner: &str, team: &str, region: &str) -> Result<(), Response>`; routes `POST /v1/internal/builders/{slug}/start`, `POST /v1/internal/builders/{slug}/stop`, `GET /v1/internal/builders/{slug}` (answers `{ "id", "state", "ready": bool, "conditions": [...] }`), all requiring `Authorization: Bearer $KLOUDLITE_BUILDER_SECRET`; `fn visible_env(e: &crd::Environment) -> bool` (`e.spec.system.is_none()`).
- Consumes: `crd::builder_id`, `crd::BUILDER_SYSTEM`, `Service.resources` (Task 2).

- [ ] **Step 1: Failing tests** (route tests with the mock kube client, the shape `environments.rs` already uses):
  - creating a workspace for `alice` writes `Environment/bld-alice` with `spec.system = "builder"`, one service `buildkit` with the exact args/port/mount of the spec, `desiredState: Stopped`, storage quota `builder_cache_gb`; a second create writes it again identically (server-side apply, no error).
  - a team workspace (`team: "acme"`) writes `bld-acme` owned by `acme`.
  - `GET /v1/environments` for alice omits `bld-alice`; `GET /v1/environments/bld-alice`, `POST …/start`, `…/push`, `…/snapshots` all answer 404.
  - `POST /v1/internal/builders/alice/start` without the secret → 401; with it → 202 and the CR's `desiredState` is `Running`; `…/stop` → `Stopped`; `GET` reports `ready` from the `Ready` condition.
  - `quota::usage` for an owner with one real environment and one builder reports `environments: 1`, `disk_gb` including the builder's, cpu/memory including the builder's ONLY when it is Running.
  - `crd_yaml.rs`: the defaults table is `(5, 2, 20, 100, 40, 80)` / `(20, 8, 80, 400, 148, 296)`.
- [ ] **Step 2: Implement.** `ensure_builder`: build the `Environment` (owner = team if non-empty else owner; region = the workspace's), SSA with field manager `kloudlite-api`. `create_ws` calls it after the Workspace write, best effort with a logged warning — a builder that fails to create must not fail the workspace. `visible_env` in every environment handler's lookup (one helper used by all; a `system` env is `not_found()`), `list_env` filters. Internal routes: a tiny middleware `require_builder_secret` comparing the Bearer to `s.builder_secret` with constant-time equality (`subtle` is already a dependency of `crates/core`; check, else `ring::constant_time`). `start`/`stop` write `desiredState` through the same CAS path `start_env`/`stop_env` use, bypassing `visible_env`. `prune_builders` in `run_beat`: list `system` environments; delete those whose owner has no Workspace (keep-biased on a failed list, like `prune_namespaces`). `quota::usage`: `if e.spec.system.is_some() { skip the count }`. `default_quota`: person cpu 40 / memory 80, team 148 / 296, and the derivation comment gains "+ one builder per owner at `PodResources::default()`"; the derivation test gains the builder term.
- [ ] **Step 3:** `cargo test -p kloudlite-workspaces`; `cargo test -p kloudlite-api-bin`; clippy → PASS. Patch the live `default-user`/`default-team` objects to the new numbers (recorded in the ledger, done at fleet time in Task 10).
- [ ] **Step 4: Commit** `"Api: every owner has a hidden builder environment, started and stopped only from inside"`.

### Task 6: Network policies for the gate

**Files:**
- Modify: `crates/workspaces/src/k8s.rs` (two policy constructors)
- Modify: `bins/agent/src/binding.rs` (owner namespaces: `allow-builder-gate` egress)
- Modify: `bins/agent/src/controller/environment.rs` (builder namespace: `allow-builder-gate` ingress, only when `spec.system == Some("builder")`)
- Modify: `deploy/k3s/agent-rbac.yaml` (nothing new: `networkpolicies` create/patch exist — check and say so in the commit)
- Test: `k8s.rs` tests, `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Produces: `k8s::builder_gate_egress(ns, owner) -> NetworkPolicy` (podSelector `{}`, egress to `namespaceSelector kubernetes.io/metadata.name=kloudlite-system` + `podSelector app=kloudlite-builder-gate`, port 1234); `k8s::builder_gate_ingress(ns, owner, owner_ref) -> NetworkPolicy` (podSelector `kloudlite.io/service=buildkit`, ingress from the same pair, port 1234).

- [ ] **Step 1: Failing tests:** the two constructors' selectors and port, by field; `apply_binding` writes the egress policy in `ws-alice` (and in a `wt-` namespace); `apply_environment` writes the ingress policy for `bld-alice` and NOT for an ordinary environment.
- [ ] **Step 2: Implement.** Copy the shape of `allow-dns` (egress) and `allow-gateway-ssh` (ingress) — those are the two existing policies with the same "one system namespace, one app label, one port" form.
- [ ] **Step 3:** tests + clippy → PASS.
- [ ] **Step 4: Commit** `"Open the path between a workspace and its builder through the gate, and nothing else"`.

### Task 7: The gate

**Files:**
- Create: `bins/builder-gate/{Cargo.toml,src/main.rs,src/lib.rs,src/who.rs,src/splice.rs,src/idle.rs}`
- Create: `bins/builder-gate/tests/gate.rs`
- Modify: `Cargo.toml` (workspace member), `Dockerfile` (a `builder-gate` target, same shape as `gateway`), `.github/workflows/image.yml` (the image), `deploy/pin.sh` (the seventh pin), `deploy/k3s/builder-gate.yaml` (Deployment in `kloudlite-system`, ServiceAccount, ClusterRole `pods: get,list,watch` for `kloudlite.io/kind=workspace`, Service `builder-gate` port 1234), `deploy/k3s/README.md` (apply list)
- Modify: `crates/core/src/settings.rs` (`CentralSettings.builder_idle_secs: 600`, `builder_start_secs: 120`, `Mark::Live`, ranges 60..=86_400 and 30..=600), `crates/workspaces/src/api/admin/schema.rs` (the two rows)

**Interfaces:**
- Consumes: the three internal api routes (Task 5), `KLOUDLITE_BUILDER_SECRET`, `KLOUDLITE_API_URL`, `LiveSettings<CentralSettings>` (the same handle `bins/gateway` holds).
- Produces: a TCP listener on `:1234`; a `/healthz` on `:8080` for the probes; Prometheus `builder_gate_connections{owner}` and `builder_gate_starts_total{outcome}`.

- [ ] **Step 1: Failing tests** in `tests/gate.rs`, against a mock api (`kloudlite_workspaces::kube_test::mock_client`'s shape, but HTTP: `axum` test server answering the three routes and recording calls) and a mock pod reflector (`who::Resolver` trait with a test impl mapping IP → `(owner, team)`):
  - a connection from an IP that maps to `(alice, "")` POSTs `start` for `alice`, polls GET until `ready: true`, then bytes written by the client arrive at a fake buildkit `TcpListener` and its reply comes back.
  - a team pod maps to the team's slug.
  - an unknown IP is closed with no api call.
  - `ready` never true within `builder_start_secs` (paused clock) → connection closed, `starts_total{outcome="timeout"}` incremented, no `stop`.
  - two connections then both closed → `stop` POSTed exactly once, `builder_idle_secs` after the LAST close (paused clock), and a connection arriving before that cancels the stop.
  - on boot with the mock reporting `bld-alice` Running → an idle timer starts and `stop` is POSTed after `builder_idle_secs`.
- [ ] **Step 2: Implement.** `who.rs`: a `kube::runtime::reflector` over `Api::<Pod>::all` with label `kloudlite.io/kind=workspace`, indexed by `status.podIP` on each event; `resolve(ip) -> Option<(owner, team)>` from the pod's `kloudlite.io/owner` and `kloudlite.io/team` labels. `splice.rs`: `tokio::io::copy_bidirectional` between the accepted socket and the dialled `buildkit.env-bld-{slug}.svc:1234`, with a `Drop` guard that decrements the owner's count. `idle.rs`: `HashMap<slug, (count, last_zero_at)>` behind a mutex, a 5 s tick that POSTs `stop` for every slug at zero for ≥ `builder_idle_secs`; on boot, GET each Running builder (the api answers a list at `GET /v1/internal/builders`) and seed `last_zero_at = now`. `main.rs`: the listener, `/healthz`, metrics (`kloudlite_core::metrics` as the gateway uses), settings handle. Start is `POST start` then GET every 2 s until `ready` or `builder_start_secs`.
- [ ] **Step 3:** `cargo test -p kloudlite-builder-gate`; clippy; `cargo build --release -p kloudlite-builder-gate`.
- [ ] **Step 4: Deploy shape.** `deploy/k3s/builder-gate.yaml` mirrors `gateway.yaml` (uid 1001, read-only root, `KLOUDLITE_BUILDER_SECRET` from a Secret `kloudlite-builder-gate`, `KLOUDLITE_API_URL` the api's in-cluster URL from the same place the gateway reads its own). `deploy/pin.sh` learns the image. The api Deployment in `deploy/kloudlite.yaml` gains `KLOUDLITE_BUILDER_SECRET` from the same Secret.
- [ ] **Step 5: Commit** `"The builder gate: start on the first connection, stop after idle"`.

### Task 8: `kl builder status`

**Files:**
- Modify: `bins/kl/src/main.rs` (subcommand), `bins/kl/src/api.rs` (one GET)
- Modify: `crates/workspaces/src/api/environments.rs`: `GET /v1/builders/me` — the ONE user-facing read, answering the caller's own builder (person, or `?team=`), the same body as the internal GET
- Test: `bins/kl` tests, the api route test (a stranger's team → 404)

- [ ] **Step 1:** failing test for the route (member sees it, non-member 404) and for `kl builder status` printing `state`, `ready` and the first non-True condition's message.
- [ ] **Step 2:** implement; `kl builder status --team acme`.
- [ ] **Step 3:** tests + clippy → PASS.
- [ ] **Step 4: Commit** `"kl builder status says why a build is waiting"`.

### Task 9: Probe, docs

**Files:**
- Modify: `bins/slo/src/stages/environment.rs` (or a new `stages/build.rs` registered in `mod.rs`), `bins/slo/src/stages/registry.rs` (`registry.team.push`)
- Modify: `crates/workspaces/src/slo/catalogue.rs`, `deploy/slo.md`, `web/apps/web/src/lib/fixtures/superadmin.ts` (three rows, byte-identical)
- Modify: `CLAUDE.md` (one paragraph in "Workspaces and environments", after the intercept paragraph)

**Catalogue rows** (`Suite::Hourly`):

| id | feature | stage | sli | target |
| --- | --- | --- | --- | --- |
| `ws.build.p95` | Workspaces | `5 · Workspace` | `docker buildx build` of a two-line Dockerfile in the probe workspace is pushed to the probe owner's own image and its manifest is readable through `/v2`; the builder was Stopped before the step | `p95(180_000)` |
| `registry.team.push` | Registry | `4 · Registry` | A team member's personal credential pushes to the team's image, and a non-member's is DENIED | `avail(99.9)` |
| `builder.hidden` | Environments | `6 · Environment` | The probe owner's builder is absent from `GET /v1/environments` and its id answers 404 on get, start, push and snapshots | `avail(99.9)` |

- [ ] **Step 1:** `ws.build.p95`: via `ws_exec`, `printf 'FROM alpine:3.20\nRUN echo slo > /slo\n' > /tmp/d/Dockerfile && docker buildx build -t $REGISTRY/$OWNER/slo-build:$RUN_ID --push /tmp/d`, then `GET /v2/{owner}/slo-build/manifests/{run}` with the probe's own registry credential → 200. Before the step, `GET /v1/builders/me` and assert `state == "stopped"`, else skip with the reason (a previous run left it running — a real finding, not a pass). `registry.team.push`: the hourly's team + the second owner: PUT a manifest as each; 201 and 403. `builder.hidden`: four requests, four expected codes. All three added to the stage's id list and exactly-once test.
- [ ] **Step 2:** the three catalogue copies; `cargo test -p kloudlite-workspaces slo` (holds `deploy/slo.md` equal); `cd web && bun run test`.
- [ ] **Step 3:** the project guide paragraph: what a builder is, that it is hidden and on demand, the gate, the credential, `may_act` in the registry, and the derived quota numbers.
- [ ] **Step 4:** `cargo test -p kloudlite-slo-bin`; clippy → PASS.
- [ ] **Step 5: Commit** `"Probe: a workspace builds and pushes, a team member pushes, the builder is invisible"`.

### Task 10: Fleet rollout and verification (the controller runs this, not a subagent)

- [ ] **Step 1:** Ship the branch (`deploy/dev/pod/ship.sh`); pin.
- [ ] **Step 2:** Order on the region, BEFORE any image pin: `crds.yaml` (the new spec fields), `agent-rbac.yaml` (unchanged, apply anyway), the `kloudlite-builder-gate` Secret (a fresh random secret, `kubectl create secret generic`), `builder-gate.yaml`. Then the api Deployment gains `KLOUDLITE_BUILDER_SECRET` (`deploy/kloudlite.yaml`) and rolls; then `deploy/roll.sh`; then `agent-daemonset.yaml` + `gateway.yaml`.
- [ ] **Step 3:** Patch `default-user` / `default-team` to `40/80` and `148/296`.
- [ ] **Step 4:** Hand test on `centralindia-k3s`: in the owner's real workspace, `docker buildx build -t <registry>/karthik1729/hello:1 --push .` on a two-line Dockerfile; watch `bld-karthik1729` go Stopped → Running → (11 min later) Stopped; `docker pull` it from the laptop. Then the same against a team image as a member.
- [ ] **Step 5:** Suspend fast + hourly crons; `deploy/dev/run-job.sh fast` then `hourly`; read the three ids BY RESULT from `default.otel_logs`; restore the crons. `reconcile.queue.failed` and the gate's own logs clean for 10 minutes.
- [ ] **Step 6:** Merge to master, push origin and platform, record the fleet lessons in the ledger and in memory.

## Self-review

Spec §1 → Tasks 1, 2, 5, 6; §2 → Task 7 (+ Task 6's policies, Task 4's `BUILDKIT_HOST`); §3 → Task 4; §4 → Task 3; §5 → Task 5 (hidden, quota) + Task 8 (`kl builder status`); §6 → nothing to build (inherited; Task 5 makes push a 404); §7 → Task 9; §8's rows → Task 7's timeout/idle tests and Task 5's `visible_env`. Names used consistently: `bld-{slug}`, `buildkit`, `1234`, `cache`, `builder-gate`, `registry-token`, `docker-credential-kl`, `allow-builder-gate`, `KLOUDLITE_BUILDER_SECRET`, `builder_idle_secs`, `builder_start_secs`, `builder_cache_gb`, `ensure_builder`, `visible_env`, `builder_id`, `BUILDER_SYSTEM`. The one open decision (gvisor) is Task 1 and feeds Task 2 Step 4 only.
