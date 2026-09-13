# Bench platform: the `Bench` object, its pod, its folder, its tunnel

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax for tracking. Every edit happens in the dev pod (`/work/src`); nothing is fixed until the step passed on the fleet on the carrying build.

**Goal:** the platform half of `docs/superpowers/specs/2026-09-13-bench-sessions-server-side-design.md` — a person's `Bench` per team, placed and reconciled by the node agent, whose pod mounts that person's bench folder from the region share at `/bench`, reachable only through the gateway tunnel on port 7789, created and driven through `/v1/bench*`, opened on a laptop by `kl-connect bench`, able to reach the person's own workspaces' tool servers for the workspace sessions it runs, and held on the fleet by SLO ids.

**Architecture:** one new cluster-scoped CRD beside `Workspace`, reusing every mechanism a workspace already has — the claim (`claim::claim`), the shared-share mount (`mount_homes`), the `ensure_shared_home` shape, the owner namespace (`ws_namespace`), the `user-key` Secret, the attach `resolv.conf`, the ssh-session JWT pattern and the gateway pump. The bench differs only in having no btrfs volume, no sshd, and a different port.

**Out of scope:** the `harness-bench` program and the harness UI (the other plan). This plan relies only on its interface: a listener on `0.0.0.0:7789`, a `--read-only` flag (a departed member's bench: history, no pi, no tools), a `--ping` flag, exit code 0 with `idle` in `/dev/termination-log` once no client has been connected and no tool has run for `KL_BENCH_IDLE_SECS`, exit code 75 when the folder lock is held with the holder in `/dev/termination-log`, `KL_TEAM` passed on to the `workspace-tools` extension, all shipped in `ghcr.io/kloudlite/kloudlite-bench`. Until that image carries the real program, Task 11 ships a stub honouring the same interface so every platform step is verifiable on its own.

## Decisions this plan makes (the spec left them open or the code contradicts it)

1. **A region belongs to the team, and the bench stores none.** No team→region binding exists in the code today (`RegionSpec` is `{name, status}`, `crates/workspaces/src/crd/region.rs:23`; `directory::Team` has no region, `crates/pulls/src/directory/teams.rs:12`; `create_ws` takes `region` from the body). Task 7 adds it, as the smallest binding that can hold:
   - **Where it lives:** a `region` field on the directory's `Team` record and on its `User` record (`crates/pulls/src/directory/mod.rs:54`), `#[serde(default)]` so every existing document still parses as unbound. A personal "team" is the person's handle, so the person's own record carries it; no second store.
   - **Set once.** `Directory::bind_region(slug, region)` is a compare-and-set on the empty value and answers the region the slug is bound to afterwards. A team is bound by a superadmin through `PUT /admin/owners/{slug}/region` (the admin process holds the directory, checks the id with `check_region`, writes an audit row). A person binds their own at their first `POST /v1/bench`, from the body's `region`, because nobody else governs a personal namespace — the spec's "personal bench region" question, now decided there. A rebind is refused with 409 in this cut: the folders already on the old region's share would silently stop being reachable (`// ponytail:` on `bind_region`: moving a team between regions is a migration of its benches' folders, designed when it is needed).
   - **Where the region is derived, at each point, never stored on the bench:** `/v1` reads it from the directory on every bench call (`team_region`, Task 8) and uses it for `check_region`, `gateway_url` and the tunnel token's `region` claim; the gateway compares that claim to its own region exactly as it does for a workspace token; the claim and `ensure_binding` use the agent's own `ctx.region` (`bins/agent/src/controller/mod.rs:231`), because a Bench is written to its team's region's cluster and only that cluster's agents ever see it. No status copy is stamped: nothing downstream of `/v1` needs one.
   - An unbound team is a 409 on every `/v1/bench` route, "team {team} has no region; a platform admin binds one"; an unbound person without `region` in the body is a 422, "choose a region for your personal bench".
2. **The folder is `{pool}/homes/.benches/{team}/{person}`.** The export is mounted AT `{pool}/homes` (`mount_homes`), so the folder lives on the same share, under the one mount and the one repair path the homes have. It cannot collide with a person's home because `valid_owner` refuses a leading dot.
3. **Quota: the person** (open question 2, default taken). `quota::usage` sums bench cpu/memory under `spec.owner`, `spec.resources`, only while a pod is wanted: `desiredState: Running` and `status.phase` not `Idle`. A bench scaled to zero or Stopped costs cpu and memory nothing. No `benches` dimension. A team's usage never includes a bench.
4. **Personal bench is `team == owner`** (the spec's "a person's own handle"), which `ws_namespace` already maps to `ws-{owner}`. The API normalizes an absent team to the caller's handle, never to empty.
5. **Object name is `bench-{hash(owner, team)}`** (`crd::bench_id`, deterministic like `builder_id`), so "one per (owner, team)" is the API server's name uniqueness, not a read-then-write race.
6. **A bench scales to zero when idle, the builder's shape with the pieces where they can see.** The builder gate counts connections, stops the builder after `builder_idle_secs` through `/v1`, and starts it on the next connection, holding that connection while it polls for ready (`bins/builder-gate/src/idle.rs`, `bins/builder-gate/src/lib.rs:143`). A bench's idleness includes tools running with nobody connected, which only `harness-bench` sees, so:
   - **Who observes:** `harness-bench` counts connected clients (open WebSockets) and running work (a pi turn between `agent_start` and `agent_end`, a task `running` or `background`, a process without `ended`). Once both have been zero for `KL_BENCH_IDLE_SECS` it writes `idle` to its termination message and exits 0.
   - **Who sets the pod absent:** the agent. The pod's `restartPolicy` is `OnFailure`, so exit 0 leaves it `Succeeded` (exit 75 still restarts). The reconciler deletes a succeeded pod and writes `phase: Idle`, `Ready=False/Idle`, `status.idleSince` = the container's `finishedAt` (derived from the object, so a replayed pass writes the same value). It creates no pod while `spec.wakeAt` is not later than `status.idleSince`. The agent still writes status only.
   - **The grace knob:** `benchIdleSecs` on `ClusterSettings` (per region, where the agent reads it), default 300, range 60..=86400, `Mark::Live`. The agent stamps it into the pod's `KL_BENCH_IDLE_SECS` at create; a running pod keeps the value it started with, as an in-flight `btrfs send` keeps its timeout.
   - **Who starts it on connect:** `POST /v1/bench/session`, which every tunnel connection already calls. On an `Idle` bench it runs `guard_alloc` (waking re-charges cpu and memory) and patches `spec.wakeAt` to now, `/v1` being the only writer of spec; on any bench not yet Ready it answers 202 `{"state": phase}` with no token. `kl-connect bench` is the gate: it holds the local connection and re-asks every second, for up to `BENCH_START_WAIT` (90 s), until a 201 carries a token, then dials. The harness and the probe reach a bench only through that local end, so neither learns a bench was asleep.
   - **Explicit `Stopped`** is the same zero with the auto-start refused: no pod, and `POST /v1/bench/session` answers 409 "bench is stopped; start it". `POST /v1/bench/start` sets `Running` and writes `wakeAt`; the pod then idles away again if nobody connects.
   - Deleting the Bench removes any pod by ownerReference GC; no finalizer, and the platform never touches the folder.
7. **Tunnel token is a new `typ: "bench-session"`**, not a reused `ssh-session`: the gateway must know which kind to resolve and which port to dial, and a workspace token must never open a bench, nor the reverse.
8. **The bench listens on the pod IP, fenced by a NetworkPolicy (deviation from the spec's first draft; the other plan's Task 12 now binds `0.0.0.0` by default).** The gateway dials the pod IP and a bench pod has no sshd to port-forward through. `harness-bench` binds `0.0.0.0:7789`; the agent writes a `bench-ingress` NetworkPolicy admitting 7789 only from the gateway pods. The k3s regions enforce policies; AKS does not (`// ponytail:` on the policy).
9. **`kl-connect bench` opens one tunnel per local TCP connection**, each with its own 60 s single-use token, because the harness opens several HTTP and WebSocket connections and the gateway spends a token on connect.
10. **A person who left a team still reads their own sessions there; nothing else.** The spec lists no `/v1` delete; deleting is `kubectl delete bench` until the folder's fate is designed (the spec's open question 2, retention). Membership decides `spec.access`, `Full` or `ReadOnly`, written only by `/v1`:
    - **Member (and always for the personal bench):** `Full` — pi, tools, workspace sessions.
    - **Not a member, but `spec.owner` is the caller and the Bench exists:** `GET /v1/bench`, `POST /v1/bench/start`, `/stop` and `/session` are admitted, and each first patches `access: ReadOnly`. `POST /v1/bench` (create) and `/attach` stay 404 "no such team". The pod runs `harness-bench --read-only`: the list, every transcript, the exchanges and the workspace threads, read through pi's SDK, with no pi process, no tool, no model and no lock. It reaches no workspace: nothing in it dials one, and `GET /v1/workspaces/{id}/tools` refuses a non-member through `my_ws` regardless.
    - **A running `Full` pod of someone who just left:** the api's keys beat (`run_beat`, `crates/workspaces/src/api/keys.rs:131`, every `KEYS_RESYNC_SECS`, `user` role only) patches `access: ReadOnly` on every Bench whose owner is no longer a member of `spec.team`, and the reconciler replaces the pod (a container command is immutable). So tools stop within one beat of the removal, and a workspace call fails at once. Rejoining flips it back at the next `/v1/bench` call.
    - A read-only bench scales to zero exactly like any other (decision 6), and its cpu and memory count against the person while it runs.
11. **The tool server listens on the pod IP; the namespace is the fence.** Every session runs in the bench pod and only tool calls reach a workspace, so the bench must dial `kl ide serve`, today bound to `127.0.0.1:7788` and reached only through the ssh tunnel. The prelude passes `--bind 0.0.0.0:7788`; the flag already exists (`bins/kl/src/main.rs:59`). A person's bench and their workspaces in one team share `ws_namespace(owner, team)`, whose `default-deny` plus `allow-same-namespace` (`bins/agent/src/binding.rs:142`) admit exactly those pods to each other and nobody else's. `allow-bench-tools` (bench pods to workspace pods, TCP 7788) names the grant; it widens nothing today, and it keeps the bench's path if `allow-same-namespace` is ever narrowed. `intercept_ingress` admits the intercepting environment on every port. With the tool server on the pod IP, that would hand the environment an unauthenticated `exec`, so it is narrowed to the intercepted ports in the same commit. `allow-gateway-ssh` stays port 22 only. The ssh tunnel (`kl-connect ws ide`) still reaches loopback. AKS enforces no policy (the `// ponytail:` of decision 8); workspaces and benches run on the k3s regions.
12. **Ownership is checked by `/v1`, never by the pod.** The tool server keeps no auth code. `GET /v1/workspaces/{id}/tools?team=` answers `{"address": "{podIP}:7788"}` only to the caller who is `spec.owner`, only while Ready, and only for a workspace of the bench's own team when `team` is given. A workspace that is not Ready, or is in another team, is a 409 naming why. Everyone but the owner gets a 404, a team admin and a superadmin included. The bench's `workspace-tools` extension dials only an address it got there.

## Global Constraints

- Edit, build, test and commit only in `/work/src` in the dev pod. Never `cargo` on the laptop; in piped pod scripts spell it `c=$(printf 'car%s' go)`. Never a plain `cargo update`, never `cargo fmt`.
- `spec.owner` is the truth; labels are a view stamped by `/v1` and healed by the agent. Never authorize on a label.
- The agent writes status only; `deploy/k3s/agent-admission.yaml` covers `benches` in the same commit that gives the agent `patch` on them.
- No admin router, admin page, history reflector or audit view touches a Bench. Task 8 has a test that proves the admin router has no bench route; Task 2 proves the admin ServiceAccount cannot read one.
- Files stay under ~800 lines; module `//!` docs carry the design context; comments say why; keep and add `// ponytail:` markers.
- Commit subjects are imperative sentence case with no tool attribution. Commit after each task from `/work/src`. Push `origin` and `platform` once, at the end of the verified batch (end of Task 14), never mid-batch.
- Before every commit: `cargo clippy --workspace --all-targets -- -D warnings` clean. Confirm package names once with `grep -h '^name' crates/*/Cargo.toml bins/*/Cargo.toml` and substitute them in the commands below if they differ.

## Task list

1. `Bench` CRD, `bench_id`, generated `crds.yaml`
2. RBAC and admission policy for benches
3. The bench pod, its folder path, its ingress policy (`k8s/bench.rs`)
4. `ensure_bench_folder` on the agent
5. The bench reconciler, its claim, its dead-node release, its controller wiring
6. The `bench-session` tunnel token
7. A region on every team and person: the directory field, `bind_region`, the admin route
8. `/v1/bench` routes, access, wake, quota, ingress allow-list
9. Gateway: `/tunnel/{id}` resolves a bench to port 7789
10. `kl-connect bench`: the local gate that wakes a bench and waits for it
11. The bench image (stub until harness-bench lands) and its CI job
12. The tool server on the pod IP: prelude, `allow-bench-tools`, intercept ingress narrowed to its ports
13. `GET /v1/workspaces/{id}/tools`: the owner's tool server address
14. SLO ids, catalogue, `deploy/slo.md`, probe stage; push the batch
15. Ship, pin, roll, apply, verify on the fleet

---

### Task 1: `Bench` CRD, `bench_id`, generated `crds.yaml`

**Files:**
- Create: `crates/workspaces/src/crd/bench.rs`
- Modify: `crates/workspaces/src/crd/mod.rs:41-56` (`mod bench;` and `pub use bench::*;`), `crates/workspaces/src/crd/names.rs` (add `bench_id` beside `builder_id`), `crates/workspaces/tests/crd_yaml.rs` (add `Bench::crd()` to the generated set), `deploy/k3s/crds.yaml` (regenerated)

**Interfaces:**
```rust
// crd/bench.rs
#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "kloudlite.io", version = "v1alpha1", kind = "Bench", plural = "benches",
    status = "BenchStatus", selectable = ".status.nodeName", derive = "PartialEq",
    printcolumn = r#"{"name":"Owner","type":"string","jsonPath":".spec.owner"}"#,
    printcolumn = r#"{"name":"Team","type":"string","jsonPath":".spec.team"}"#,
    printcolumn = r#"{"name":"Node","type":"string","jsonPath":".status.nodeName"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct BenchSpec {
    pub owner: String,
    pub team: String,
    // No region: a team is bound to one (the directory's record), and /v1 reads it there.
    pub image: String,
    #[serde(default)]
    pub model: String,
    pub desired_state: DesiredState,
    /// `Full` for a member; `ReadOnly` once the owner has left `team`. Written only by /v1.
    #[serde(default)]
    pub access: BenchAccess,
    /// RFC 3339, written by /v1 when a client asks for a tunnel to an idle bench. A pod is wanted
    /// again only while this is later than `status.idleSince`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_at: Option<String>,
    #[serde(default)]
    pub resources: PodResources,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_environment: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum BenchAccess { #[default] Full, ReadOnly }

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BenchStatus {
    pub phase: Phase,
    #[serde(default)]
    pub node_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_ref: Option<String>,
    /// The `finishedAt` of the pod that exited idle; cleared when a pod is created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_since: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

pub const FOLDER_READY: &str = "FolderReady";
pub const FOLDER_NOT_READY: &str = "FolderNotReady";
pub const FOLDER_LOCKED: &str = "FolderLocked";
pub const BENCH_IDLE: &str = "Idle";
/// Whether a pod should exist now: Running, and not asleep unless a wake came after it slept.
pub fn bench_wants_pod(b: &Bench) -> bool;

// crd/names.rs
/// "bench-" + 12 hex of sha256("{owner}\0{team}") over lowercased inputs.
pub fn bench_id(owner: &str, team: &str) -> String;
```

- [ ] **Failing test** in `crd/bench.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bench_is_one_name_per_owner_and_team_and_parses_without_optional_fields() {
        assert_eq!(bench_id("Alice", "acme"), bench_id("alice", "acme"));
        assert_ne!(bench_id("alice", "acme"), bench_id("alice", "alice"));
        assert_ne!(bench_id("ab", "c"), bench_id("a", "bc"), "the separator keeps pairs apart");
        assert!(bench_id("alice", "acme").len() <= 63);
        let v = serde_json::json!({"owner":"alice","team":"acme","image":"i","desiredState":"running"});
        let s: BenchSpec = serde_json::from_value(v).unwrap();
        assert_eq!(s.resources, PodResources::default());
        assert_eq!(s.access, BenchAccess::Full, "an absent access is a member's bench");
        assert!(s.model.is_empty() && s.attached_environment.is_none() && s.wake_at.is_none());
    }

    #[test]
    fn a_pod_is_wanted_while_running_and_awake_or_woken_after_it_slept() {
        let mut b = Bench::new("bench-1", serde_json::from_value(serde_json::json!(
            {"owner":"alice","team":"acme","image":"i","desiredState":"running"})).unwrap());
        assert!(bench_wants_pod(&b), "never slept");
        b.status = Some(BenchStatus { idle_since: Some("2026-09-13T10:00:00Z".into()), ..Default::default() });
        assert!(!bench_wants_pod(&b), "asleep and nobody asked");
        b.spec.wake_at = Some("2026-09-13T09:59:59Z".into());
        assert!(!bench_wants_pod(&b), "a wake from before it slept is spent");
        b.spec.wake_at = Some("2026-09-13T10:00:01Z".into());
        assert!(bench_wants_pod(&b));
        b.spec.desired_state = DesiredState::Stopped;
        assert!(!bench_wants_pod(&b), "stopped refuses a wake");
    }
}
```
- [ ] Run: `cargo test -p kloudlite-workspaces a_bench_is_one_name` → expected FAIL: `cannot find function bench_id` / `cannot find type BenchSpec`.
- [ ] Implement `bench.rs` as in Interfaces with a `//!` doc ("a person's bench in one team; no volume, no sshd, no region of its own; the folder is on the region share; it sleeps when idle; see the spec"). `bench_wants_pod` compares the two RFC 3339 strings by parsing them with `chrono::DateTime::parse_from_rfc3339` (an unparsable `wakeAt` is no wake), and `bench_id` in `names.rs` using the hashing helper `builder_id`'s neighbours use (`hex_prefix`).
- [ ] Run again → expected `test ... ok`.
- [ ] Add `Bench::crd()` to `crates/workspaces/tests/crd_yaml.rs` in the same shape as the entries at `:165`/`:193`; regenerate `deploy/k3s/crds.yaml` as that test's failure message instructs; add `assert!(yaml.contains("name: benches.kloudlite.io"))`.
- [ ] Run: `cargo test -p kloudlite-workspaces --test crd_yaml` → all ok.
- [ ] Clippy; commit: `Add the Bench CRD: one per person per team, no volume`.

### Task 2: RBAC and admission policy for benches

**Files:**
- Modify: `deploy/k3s/agent-rbac.yaml` (header table and rules), `deploy/k3s/agent-admission.yaml` (the `resources:` list of `kloudlite-agent-spec-is-read-only`), `deploy/k3s/api-rbac.yaml:21-22` (the `kloudlite-api` ClusterRole only), `deploy/k3s/slo-rbac.yaml` (`benches: get,list`)

**Interfaces (Kubernetes):**
- Agent: `benches` `get,list,watch` (controller, claim re-read) and `patch` (heal_labels, metadata only); `benches/status` `get,patch,update` (claim's `replace_status` and status writes).
- Admission: add `"benches"` to `resourceRules[0].resources`. The CEL's non-Volume arm (`object.spec == oldObject.spec`) covers it unchanged; the agent has no `create` verb on benches.
- API (`kloudlite-api`): `benches` `get,list,create,patch`. No `delete` (decision 10). The `kloudlite-admin` role gets nothing.

- [ ] **Failing check** (no Rust test exists for yaml; this is the gate). With Task 1's CRD applied to the dev cluster (`kubectl apply -f deploy/k3s/crds.yaml`):
```sh
kubectl auth can-i patch benches.kloudlite.io --subresource=status --as=system:serviceaccount:kube-system:kloudlite-agent
```
→ expected `no`.
- [ ] Add the table rows and rules; add `benches` to the admission list with one comment line ("a Bench is desired state from /v1 like a Workspace; the agent writes its status").
- [ ] Apply the four files to the dev cluster. Re-run the check → `yes`. Then:
```sh
kubectl auth can-i get benches.kloudlite.io --as=system:serviceaccount:kloudlite:kloudlite-admin    # expected: no
kubectl auth can-i delete benches.kloudlite.io --as=system:serviceaccount:kloudlite:kloudlite-api   # expected: no
```
- [ ] Admission check: create a Bench as yourself, then
```sh
kubectl patch bench <id> --type merge -p '{"spec":{"image":"x"}}' \
  --as=system:serviceaccount:kube-system:kloudlite-agent --dry-run=server
```
→ expected `denied ... kloudlite-agent writes status, not spec`.
- [ ] Commit: `Let the agent reconcile benches and fence it from their spec`.

### Task 3: The bench pod, its folder path, its ingress policy

**Files:**
- Create: `crates/workspaces/src/k8s/bench.rs`, `crates/workspaces/src/k8s/tests/bench.rs`
- Modify: `crates/workspaces/src/k8s/mod.rs` (`mod bench; pub use bench::*;`), `crates/workspaces/src/k8s/tests/mod.rs` (`mod bench;`)

**Interfaces:**
```rust
pub const BENCH_PORT: u16 = 7789;
pub const BENCH_DIR: &str = "/bench";
pub const BENCH_CONTAINER: &str = "bench";
pub const BENCH_POD: &str = "bench";
/// `{pool}/homes/.benches/{team}/{owner}`, each segment checked by `model::validate_mount`.
pub fn bench_folder(pool: &str, team: &str, owner: &str) -> Result<String, String>;
/// `idle_secs` is the region's `benchIdleSecs`, stamped into KL_BENCH_IDLE_SECS at create.
pub fn bench_pod(b: &crd::Bench, id: &str, pool: &str, runtime_class: Option<&str>, registry_host: &str, idle_secs: u64) -> Result<Pod, String>;
/// Admits BENCH_PORT only from the gateway's pods.
pub fn bench_ingress_policy(namespace: &str, id: &str) -> NetworkPolicy;
```
Pod shape — start from `workspace_pod` (`k8s/workspace.rs:490`) and delete, never re-derive:
- Name `BENCH_POD`, namespace `crd::ws_namespace(owner, team)`, labels `KIND_LABEL=bench`, `OWNER_LABEL`, `TEAM_LABEL`, `WORKSPACE_LABEL=id` (the pod-watch mapper and the attach NetworkPolicy both key on it), an ownerReference to the Bench with `controller: true`.
- `nodeName` from `status.nodeName`, `runtimeClassName`, and the same hardened security context (uid/gid `SSH_UID`, no privilege escalation, read-only root, `drop: ALL`).
- Volumes: `home_volume(pool, owner)` at `HOME_DIR`; a hostPath `bench_folder(..)` with `type: Directory` at `BENCH_DIR` (never `DirectoryOrCreate`: a missing folder must fail, never become an empty local directory); the `user-key` Secret projected as the workspace does; the attach `resolv.conf` (`attach_file(pool, id)`, `type: File`) at `/etc/resolv.conf`; an `emptyDir` at `/tmp`.
- Not carried: the btrfs worktree, homecache, sshd config and host-key Secret, `authorized_keys`, the git seed init container, the `.cache` subPath mounts.
- Command: `["harness-bench"]` for `access: Full`, `["harness-bench", "--read-only"]` for `access: ReadOnly`. Env: `KL_OWNER`, `KL_TEAM`, `KL_BENCH=id`, `KL_MODEL=spec.model`, `KL_REGISTRY_HOST`, `KL_BENCH_IDLE_SECS=idle_secs`, `NODE_NAME` from the downward API (the lock holder's name), `HOME=/home/kl`, `LANG=C.UTF-8`.
- `restartPolicy: OnFailure`: an idle exit (0) leaves the pod `Succeeded` for the agent to remove; a held lock (75) or a crash restarts it.
- Resources: `quantities(&spec.resources)`. A bench that should cost nothing has no pod at all.
- Readiness: `exec: ["harness-bench", "--ping"]`, period 5 s.
- `terminationMessagePolicy: File` (the default path `/dev/termination-log` carries the lock holder, Task 5).

- [ ] **Failing tests** in `k8s/tests/bench.rs` (a `fixture_bench(owner, team, state)` helper at the top builds a `crd::Bench` with `status.nodeName = "n1"`):
```rust
#[test]
fn a_bench_pod_mounts_only_its_own_folder_and_no_worktree() {
    let b = fixture_bench("alice", "acme", DesiredState::Running);
    let p = bench_pod(&b, "bench-1", "/wspool", None, "cr.example", 300).unwrap();
    assert_eq!(p.metadata.namespace.as_deref(), Some(crate::crd::ws_namespace("alice", "acme").as_str()));
    let spec = p.spec.unwrap();
    let paths: Vec<String> = spec.volumes.as_ref().unwrap().iter()
        .filter_map(|v| v.host_path.as_ref().map(|h| h.path.clone())).collect();
    assert!(paths.contains(&"/wspool/homes/.benches/acme/alice".to_string()));
    assert!(paths.contains(&"/wspool/homes/alice".to_string()));
    assert!(!paths.iter().any(|p| p.contains("/vol/") || p.contains("homecache") || p.ends_with("/.benches") || p.ends_with("/acme")));
    let c = &spec.containers[0];
    assert_eq!(c.command.as_deref(), Some(&["harness-bench".to_string()][..]));
    assert!(c.volume_mounts.as_ref().unwrap().iter().any(|m| m.mount_path == "/bench"));
}

#[test]
fn a_departed_members_bench_runs_the_reader_and_every_bench_may_exit_idle() {
    let mut b = fixture_bench("alice", "acme", DesiredState::Running);
    b.spec.access = crate::crd::BenchAccess::ReadOnly;
    let spec = bench_pod(&b, "bench-1", "/wspool", None, "cr", 420).unwrap().spec.unwrap();
    let c = &spec.containers[0];
    assert_eq!(c.command.as_ref().unwrap().last().map(String::as_str), Some("--read-only"));
    assert_eq!(spec.restart_policy.as_deref(), Some("OnFailure"), "exit 0 is idle and must not restart");
    let idle = c.env.as_ref().unwrap().iter().find(|e| e.name == "KL_BENCH_IDLE_SECS").unwrap();
    assert_eq!(idle.value.as_deref(), Some("420"));
}

#[test]
fn a_folder_segment_that_escapes_is_refused_before_it_becomes_a_hostpath() {
    assert!(bench_folder("/wspool", "..", "alice").is_err());
    assert!(bench_folder("/wspool", "acme", "a/b").is_err());
    assert!(bench_folder("/wspool", "acme", ".").is_err());
    assert!(bench_pod(&fixture_bench("alice", "../x", DesiredState::Running), "b", "/wspool", None, "cr", 300).is_err());
}

#[test]
fn only_the_gateway_may_reach_the_bench_port() {
    let np = bench_ingress_policy("ws-alice", "bench-1");
    let spec = np.spec.unwrap();
    assert_eq!(spec.pod_selector.match_labels.unwrap()[WORKSPACE_LABEL], "bench-1");
    let rule = &spec.ingress.unwrap()[0];
    assert_eq!(rule.ports.as_ref().unwrap()[0].port, Some(IntOrString::Int(BENCH_PORT as i32)));
    assert_eq!(rule.from.as_ref().unwrap().len(), 1);
}
```
- [ ] Run: `cargo test -p kloudlite-workspaces a_bench_pod_mounts` → expected FAIL: `cannot find function bench_pod`.
- [ ] Implement. `bench_folder` refuses `.` and `..` explicitly, runs `model::validate_mount` (`model.rs:170`) on a `Mount { folder: segment, path: BENCH_DIR }` per segment, then formats the path. `bench_ingress_policy`'s `from` is one peer with a namespace selector and a pod selector copied from the labels in `deploy/k3s/gateway.yaml` (read them; do not guess). Put `// ponytail: AKS runs no network policy engine; the fence holds on the k3s regions where benches run` on the policy builder. The `//!` doc names what is absent and why.
- [ ] Run the four tests → ok. Run `cargo test -p kloudlite-workspaces` → no regressions.
- [ ] Clippy; commit: `Build the bench pod: home, its own folder, the gateway-only port`.

### Task 4: `ensure_bench_folder` on the agent

**Files:**
- Modify: `bins/agent/src/controller/workspace/home.rs:1-63` (add the function beside `ensure_shared_home`; both are a directory on the share), `bins/agent/src/controller/workspace/mod.rs` (re-export beside `ensure_shared_home`), `crates/storage/src/store.rs` (a test next to `RESERVED_OWNERS` asserting `!valid_owner(".benches")`, commented that the bench path depends on it)

**Interfaces:**
```rust
/// `{pool}/homes/.benches/{team}/{owner}`: re-verifies the share, then mkdir; the team directory
/// root-owned 0755, the person's directory uid 1000 mode 0700 so another person's bench pod cannot read it.
pub(crate) fn ensure_bench_folder(pool: &str, export: &str, team: &str, owner: &str, uid: u32) -> Result<(), String>;
```

- [ ] **Failing test** in `home.rs`'s `home_tests`:
```rust
#[test]
fn a_bench_folder_is_made_on_the_share_private_and_refuses_an_escaping_segment() {
    use super::super::ensure_bench_folder;
    let tmp = tempfile::tempdir().unwrap();
    let pool = tmp.path().display().to_string();
    std::fs::create_dir_all(crate::homes_root(&pool)).unwrap();
    ensure_bench_folder(&pool, "unused", "acme", "alice", 1000).unwrap();
    let dir = crate::homes_root(&pool).join(".benches/acme/alice");
    assert!(dir.is_dir());
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    ensure_bench_folder(&pool, "unused", "acme", "alice", 1000).unwrap();
    assert!(ensure_bench_folder(&pool, "unused", "..", "alice", 1000).is_err());
    assert!(ensure_bench_folder(&pool, "unused", "acme", "../bob", 1000).is_err());
    assert!(!crate::homes_root(&pool).join("bob").exists());
}
```
- [ ] Run: `cargo test -p kloudlite-agent-bin a_bench_folder_is_made` → expected FAIL: `cannot find function ensure_bench_folder`.
- [ ] Implement with the same `crate::may_mount()` / `crate::mount_homes` gate as `ensure_shared_home`; segments through `k8s::bench_folder` (Task 3) so the agent and the pod builder cannot disagree on the path; `create_dir_all`; `set_permissions` 0700 on the person's directory; `chown` only under `geteuid() == 0` as the neighbour does.
- [ ] Run → ok; `cargo test -p kloudlite-storage` for the reserved-owner test → ok; clippy.
- [ ] Commit: `Make a private bench folder on the region share under the homes`.

### Task 5: The bench reconciler, its claim, its dead-node release, its controller wiring

**Files:**
- Create: `bins/agent/src/controller/bench.rs` (under 400 lines), `bins/agent/tests/reconcile/bench.rs`
- Modify: `crates/workspaces/src/crd/settings.rs` (`bench_idle_secs: Option<u64>` on `ClusterSettingsSpec` beside `sync_secs` at `:77`, its row `("benchIdleSecs", Mark::Live, &[])` in `CLUSTER_SETTING_META` at `:142`, and its range 60..=86400), `crates/workspaces/src/settings.rs` (`AgentSettings::bench_idle_secs`, env `WS_BENCH_IDLE_SECS`, default 300, merged like `sync_secs`), `bins/agent/src/controller/mod.rs` (`mod bench; pub use bench::reconcile_bench;`), `bins/agent/src/claim.rs` (`claim_bench` after `claim_environment` near `:642`), `bins/agent/src/controller/run.rs` (a placed `Controller<Bench>` beside `workspaces` at `:269`; an unplaced claim controller beside `claim_ws` at `:399`), `bins/agent/src/peer/sweeps.rs` (bench release on an unplaceable node), `bins/agent/tests/reconcile/main.rs` (`mod bench;`), `deploy/k3s/agent-rbac.yaml` (a `networkpolicies` row naming `reconcile_bench` if the attach row does not already grant create/patch)

**Interfaces:**
```rust
pub async fn reconcile_bench(b: Arc<crd::Bench>, ctx: Arc<Ctx>) -> Result<Action, ReconcileErr>;
pub async fn claim_bench(b: &crd::Bench, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr>;
/// Pure: what the pod says about the bench. Split out so the arms are unit-testable.
pub(crate) fn bench_state(b: &crd::Bench, pod: Option<&Pod>) -> PodVerdict;
pub(crate) enum PodVerdict { Create, Replace, Starting, Ready, Locked(String), Idle(String), Absent, Remove }
```
The reconcile, in order. Every status write goes through `bench_conditions(prev, c)`, which replaces by type and keeps `Placed` and `FolderReady` (the `replaced` helper in `controller/workspace/conditions.rs` already does this; reuse it).
1. `heal_labels` (the existing generic helper): owner, team, kind.
2. Namespace readiness: the same `NAMESPACE_READY` wait `apply_workspace` does (`controller/workspace/mod.rs:190-202`).
3. `ctx.homes_export` is `None` → phase `Creating`, `Ready=False/FolderNotReady` "this node has no shared-home mount (WS_HOMES_EXPORT)", requeue `TICK`.
4. `spawn_blocking(ensure_bench_folder)` under `super::timed("bench_folder", ..)`. `Err` → `Creating`, `FolderReady=False/FolderNotReady` with the error, requeue `TICK`. `Ok` → `FolderReady=True/Ready`.
5. Attach `resolv.conf` for `id` with the render the workspace path uses (the function writing `attach_file(pool, id)`), so `spec.attachedEnvironment` works. `/v1` writes the NetworkPolicy half (Task 8).
6. Apply `k8s::bench_ingress_policy` (server-side apply, field manager `kloudlite-agent`).
7. The `user-key` Secret must exist in the namespace (a GET). Missing → `Ready=False/KeysNotReady`, requeue `TICK`. `/v1` installs it (Task 8).
8. GET the pod, then `bench_state`, which reads `crd::bench_wants_pod` first:
   - pod `Succeeded` → `Idle(finishedAt)`: delete the pod, write phase `Idle`, `Ready=False/Idle` "no client and nothing running for benchIdleSecs; the next connection starts it", `status.idleSince = finishedAt`, clear `podRef`. The next pass decides from the new status (a `wakeAt` that raced the exit is later than `finishedAt` and creates a pod at once).
   - no pod and `bench_wants_pod` → `Create`: create `k8s::bench_pod` with `ctx.settings.load().bench_idle_secs`; the status write that records `podRef` clears `idleSince`.
   - no pod and not wanted → `Absent`: phase `Stopped` with `Ready=False/Stopped` when `desiredState` is Stopped, else phase `Idle` unchanged; requeue on change only.
   - a pod and not wanted (`desiredState: Stopped`) → `Remove`: delete it, requeue 2 s.
   - a pod whose `--read-only` does not match `spec.access` → `Replace` (a container command is immutable): delete, requeue 2 s.
   - `Starting` → phase `Starting`; `Ready` → phase `Ready`, `Ready=True/Running` or `Ready=True/ReadOnly`; `Locked(holder)` (container last terminated with exit code 75) → `Ready=False/FolderLocked`, message "folder held by {holder}". Record `status.podRef = "{ns}/bench"` once the pod exists.

`claim` takes `parts: fn(&K) -> Parts<'_>` today (`bins/agent/src/claim.rs:543`), a function pointer that cannot capture `ctx`. Widen it to `parts: for<'a> fn(&'a K, &'a Ctx) -> Parts<'a>`; the Workspace and Environment callers ignore the second argument and keep `region: &o.spec.region`. `claim_bench` is `claim(b, ctx, "Bench", crd::Phase::Pending, |o, c| Parts { node_name: status node, storage: None, volume: None, region: &c.region, owner: &o.spec.owner, want: if crd::bench_wants_pod(o) { want_of(&o.spec.resources) } else { Want::default() } })`: a Bench is written to its team's region's cluster, so the agent's own region is the team's. With no storage and no volume, `decide` (`claim.rs:415`) reaches the capacity and placeability arms and nothing volume-bound; a sleeping bench asks for no capacity.

Dead node: a bench holds no state on its node, so it is always releasable. In `peer/sweeps.rs`, next to the pass that calls `unplaceable(`, add a bench pass: for each Bench whose `status.nodeName` names an unplaceable node, clear `nodeName` and write `Placed=False/NodeDead` through `mark_parent_of::<crd::Bench>` (read its signature; it is generic over the kind). Any up node then claims it; the folder lock (`FolderLocked`) is what keeps a zombie writer on a partitioned node from double-writing.

- [ ] **Failing tests**, first the pure ones in `controller/bench.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pod_decides_create_replace_ready_locked_idle_and_absent() {
        let running = fixture_bench(DesiredState::Running);
        assert!(matches!(bench_state(&running, None), PodVerdict::Create));
        assert!(matches!(bench_state(&running, Some(&pod_with(&["harness-bench", "--read-only"], None, true))), PodVerdict::Replace), "a member's bench running the reader");
        assert!(matches!(bench_state(&running, Some(&pod_with(&["harness-bench"], None, true))), PodVerdict::Ready));
        assert!(matches!(bench_state(&running, Some(&pod_with(&["harness-bench"], None, false))), PodVerdict::Starting));
        match bench_state(&running, Some(&pod_with(&["harness-bench"], Some((75, "node-b")), false))) {
            PodVerdict::Locked(h) => assert_eq!(h, "node-b"),
            _ => panic!("exit 75 is a held lock"),
        }
        let mut exited = pod_with(&["harness-bench"], Some((0, "idle")), false);
        exited.status.as_mut().unwrap().phase = Some("Succeeded".into());
        match bench_state(&running, Some(&exited)) {
            PodVerdict::Idle(at) => assert_eq!(at, FINISHED_AT),
            _ => panic!("exit 0 is asleep"),
        }
        let mut asleep = fixture_bench(DesiredState::Running);
        asleep.status.as_mut().unwrap().idle_since = Some(FINISHED_AT.into());
        assert!(matches!(bench_state(&asleep, None), PodVerdict::Absent), "nobody asked");
        asleep.spec.wake_at = Some("2099-01-01T00:00:00Z".into());
        assert!(matches!(bench_state(&asleep, None), PodVerdict::Create), "a client asked after it slept");
        let stopped = fixture_bench(DesiredState::Stopped);
        assert!(matches!(bench_state(&stopped, Some(&pod_with(&["harness-bench"], None, true))), PodVerdict::Remove));
        assert!(matches!(bench_state(&stopped, None), PodVerdict::Absent));
        let mut departed = fixture_bench(DesiredState::Running);
        departed.spec.access = crd::BenchAccess::ReadOnly;
        assert!(matches!(bench_state(&departed, Some(&pod_with(&["harness-bench"], None, true))), PodVerdict::Replace), "tools stop when the owner leaves");
    }
}
```
(`fixture_bench`, `FINISHED_AT` (`"2026-09-13T10:00:00Z"`, the terminated state's `finishedAt` in `pod_with`) and `pod_with(command, last_terminated: Option<(exit, message)>, ready)` are ten-line helpers in the same test module.)

Then the loop tests in `tests/reconcile/bench.rs`, built on `kube_test::{mock_client, Recorder, Route}` with route tables copied from `the_workspace_reconciler_and_its_volume.rs`:
```rust
#[tokio::test]
async fn a_bench_on_a_node_without_the_share_parks_and_starts_no_pod() {
    // ctx.homes_export = None. Serve: GET namespace (ready), status PUT/PATCH recorded.
    // assert: the recorded status carries Ready=False reason FolderNotReady
    // assert: recorder has no POST to /api/v1/namespaces/*/pods
}

#[tokio::test]
async fn a_running_bench_makes_its_folder_and_one_pod() {
    // tempdir pool with homes/; homes_export = Some("unused"); GET pod -> 404; GET user-key -> 200.
    // assert: homes/.benches/acme/alice is a directory
    // assert: exactly one POST pods whose body command == ["harness-bench"] and a hostPath ending "/.benches/acme/alice"
}

#[tokio::test]
async fn an_idle_exit_removes_the_pod_and_only_a_later_wake_brings_it_back() {
    // desiredState Running; GET pod -> phase Succeeded, container terminated exit 0 finishedAt FINISHED_AT.
    // assert: one DELETE pods/bench; the status write carries phase Idle, Ready=False/Idle, idleSince == FINISHED_AT, no podRef
    // second pass, Bench re-seeded with that status and no wakeAt, GET pod -> 404: no POST pods, no DELETE
    // third pass, wakeAt one second after FINISHED_AT: exactly one POST pods whose env KL_BENCH_IDLE_SECS == ctx's benchIdleSecs
}

#[tokio::test]
async fn stopping_a_bench_leaves_no_pod_at_all() {
    // desiredState Stopped; GET pod -> command ["harness-bench"], Ready.
    // assert: one DELETE pods/bench and no POST in this pass; second pass with GET pod -> 404: no POST, status phase Stopped, Ready=False/Stopped
}

#[tokio::test]
async fn an_unplaced_bench_is_claimed_without_touching_a_volume() {
    // claim_bench on an unplaced Bench; serve GET node (Ready), capacity lists as the capacity tests do.
    // assert: PUT benches/{id}/status with nodeName == ctx.node and Placed=True/Claimed
    // assert: recorder has no request under /apis/kloudlite.io/v1alpha1/volumes
}

#[tokio::test]
async fn a_bench_on_a_dead_node_is_released_for_another_node() {
    // node n-dead NotReady past node_dead_secs; Bench nodeName n-dead.
    // assert: the status write clears nodeName and carries Placed=False/NodeDead
}
```
Each comment is the assertion list the body implements in full with `Recorder` checks; no body is left as a comment.
- [ ] Add `benchIdleSecs` to `ClusterSettingsSpec`, `CLUSTER_SETTING_META` and `AgentSettings`; the existing test that holds the meta table equal to the spec's fields (`crates/workspaces/src/crd/mod.rs:578`) is the check. Run `cargo test -p kloudlite-workspaces cluster_setting` → ok.
- [ ] Run: `cargo test -p kloudlite-agent-bin the_pod_decides` → FAIL: `cannot find function bench_state`. Run: `cargo test -p kloudlite-agent-bin --test reconcile bench` → FAIL: unresolved import `kloudlite_agent::controller::reconcile_bench`.
- [ ] Implement `controller/bench.rs`, `claim_bench`, the sweep pass, and the `run.rs` wiring: bench pods watched with `KIND_LABEL=bench`, mapped through `held(&bench_store, owned_by::<crd::Bench, _>(&p))`; the claim controller gated on `ctx.has_pool` like `claim_ws`; both runs wrapped in `observed("bench", ..)` and `observed("claim", ..)`.
- [ ] Run both commands → ok. `cargo test -p kloudlite-agent-bin` → no regressions; the one-kind-of-node merge noted two wall-clock flakes, so re-run a single failure once before calling it a regression.
- [ ] Clippy; commit: `Reconcile benches on the node: claim, folder, pod, and no pod while it sleeps`.

### Task 6: The `bench-session` tunnel token

**Files:**
- Modify: `crates/core/src/jwt.rs` (claims struct near `:58`, mint/verify beside `mint_ssh_session` at `:194`, test beside `:349`)

**Interfaces:**
```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchSessionClaims { pub sub: String, pub bench: String, pub region: String, pub jti: String, pub iat: u64, pub exp: u64, pub typ: String }
pub fn mint_bench_session(&self, owner: &str, bench: &str, region: &str) -> Result<(String, BenchSessionClaims)>; // typ "bench-session", ttl SSH_SESSION_TTL_SECS
pub fn verify_bench_session(&self, token: &str) -> Result<BenchSessionClaims>; // verify_typed(token, "bench-session")
```

- [ ] **Failing test:**
```rust
#[test]
fn a_bench_session_is_sixty_seconds_and_never_opens_a_workspace() {
    let j = Jwt::new("0123456789abcdef0123456789abcdef").unwrap();
    let (tok, c) = j.mint_bench_session("alice", "bench-1", "r1").unwrap();
    assert_eq!(c.exp - c.iat, SSH_SESSION_TTL_SECS);
    assert_eq!(j.verify_bench_session(&tok).unwrap().bench, "bench-1");
    assert_eq!(c.jti.len(), 32);
    assert!(j.verify_ssh_session(&tok).is_err(), "a bench token is not a workspace token");
    let (ws, _) = j.mint_ssh_session("alice", "bench-1", "r1").unwrap();
    assert!(j.verify_bench_session(&ws).is_err(), "a workspace token is not a bench token");
    assert!(j.verify(&tok).is_err(), "a session token is not a login");
}
```
- [ ] Run: `cargo test -p kloudlite-core a_bench_session_is_sixty` → FAIL: `no method named mint_bench_session`.
- [ ] Implement by copying `mint_ssh_session`'s body with the new claims.
- [ ] Run → ok; clippy; commit: `Mint a single-use bench tunnel token`.

### Task 7: A region on every team and person

**Files:**
- Modify: `crates/pulls/src/directory/teams.rs:12` (`region` on `Team`, and in its `Default` at `:47`), `crates/pulls/src/directory/mod.rs:54` (`region` on `User`), `crates/pulls/src/directory/teams.rs` (`bind_region` beside `update_team`, and its test beside the team tests), `crates/workspaces/src/api/state.rs:72-105` (two trait methods), `bins/api/src/main.rs:55` (their implementation on `Dir`), every `impl Directory for` stub under `crates/workspaces/tests/` and the one in `crates/workspaces/src/api/mod.rs:535` (default bodies make this a no-op for them), `crates/workspaces/src/api/admin/owners.rs` (the route), `crates/workspaces/src/api/admin.rs:177` (mount it), the admin tests where `owner_detail` is tested (grep `owner_detail` under `crates/workspaces/tests`)

**Interfaces:**
```rust
// crates/pulls/src/directory
#[serde(default)] pub region: String,          // on Team and on User; empty = unbound
/// Compare-and-set on the empty value: a team slug, else a person by handle. Returns the region the
/// slug is bound to after the call — `region` itself, or what it already held — and `None` for no such owner.
// ponytail: set once; moving a team to another region is a migration of its benches' folders, designed when needed.
pub async fn bind_region(&self, slug: &str, region: &str) -> Result<Option<String>>;

// crates/workspaces/src/api/state.rs, on trait Directory
async fn region_of(&self, slug: &str) -> Option<String> { None }            // team or person; None = unbound or unreadable
async fn bind_region(&self, slug: &str, region: &str) -> Result<Option<String>, String> { Err("no directory".into()) }

// crates/workspaces/src/api/admin/owners.rs
#[derive(Deserialize)] pub(crate) struct RegionBody { region: String }
/// PUT /admin/owners/{slug}/region — 200 {slug, region}; 409 when already bound elsewhere; 404 no such owner; 422 unknown region.
pub(crate) async fn bind_owner_region(State(s): State<Arc<ApiState>>, headers: HeaderMap, Path(slug): Path<String>, Json(b): Json<RegionBody>) -> Result<Response, Response>;
```
Mongo: `update_one({_id: slug, $or: [{region: {$exists: false}}, {region: ""}]}, {$set: {region}})` on `teams`, then the same on `users` keyed by `username: slug` when no team matched; afterwards read the document back and return its `region`. The memory backend does the same under its lock. The admin handler runs `check_region`, then `bind_region`, answers 409 `{"error": "{slug} is bound to {r}; a region is set once"}` when the returned region differs, and writes `crate::audit::record` with action `owner.region.bind` on success.

- [ ] **Failing test** in `teams.rs`'s test module (memory backend):
```rust
#[tokio::test]
async fn a_region_binds_once_to_a_team_or_a_person_and_never_moves() {
    let d = Directory::in_memory();
    d.upsert_user("alice@x.io", "Alice").await.unwrap();
    d.claim_username("alice@x.io", "alice").await.unwrap().unwrap();
    d.create("acme", "Acme", "alice@x.io").await.unwrap().unwrap();
    assert_eq!(d.get("acme").await.unwrap().unwrap().region, "", "an existing team is unbound");
    assert_eq!(d.bind_region("acme", "r1").await.unwrap().as_deref(), Some("r1"));
    assert_eq!(d.bind_region("acme", "r2").await.unwrap().as_deref(), Some("r1"), "set once");
    assert_eq!(d.bind_region("alice", "r2").await.unwrap().as_deref(), Some("r2"), "a person's handle binds their own record");
    assert_eq!(d.user_by_handle("alice").await.unwrap().unwrap().region, "r2");
    assert_eq!(d.bind_region("nobody", "r1").await.unwrap(), None);
}
```
`Directory::in_memory` is what the directory's own tests construct (`crates/pulls/src/directory/mod.rs:737`); `upsert_user`, `claim_username` and `user_by_handle` are `users.rs:10`, `:107` and `:160`.
- [ ] Run: `cargo test -p kloudlite-pulls a_region_binds_once` → FAIL: `no field region` / `no method bind_region`.
- [ ] Implement the field, `bind_region` and its `// ponytail:`; run → ok.
- [ ] **Failing test** in the admin tests:
```rust
#[tokio::test]
async fn only_a_superadmin_binds_a_region_and_only_once() {
    // StubMembership gains region_of/bind_region over a Mutex<HashMap<String, String>> seeded {"acme": ""}; Region r1 and r2 exist.
    // PUT /admin/owners/acme/region {"region":"r1"} with a superadmin token          -> 200 {"slug":"acme","region":"r1"}; one audit row owner.region.bind
    // the same with {"region":"r2"}                                                    -> 409, error == "acme is bound to r1; a region is set once"
    // {"region":"nope"}                                                                -> 422
    // a token without the superadmin claim                                             -> 403 (refuse_without_claim)
}
```
The comments are the exact assertions; write them out in full.
- [ ] Run: `cargo test -p kloudlite-workspaces --test api_admin only_a_superadmin_binds` → FAIL; implement the trait methods, `Dir`'s bodies (`self.0.get(slug)` then `self.0.user_by_handle(slug)` for `region_of`; `self.0.bind_region` for the bind), the handler and `.route("/admin/owners/{slug}/region", put(owners::bind_owner_region))`; run → ok.
- [ ] `cargo test -p kloudlite-pulls`, `cargo test -p kloudlite-workspaces` → no regressions; clippy.
- [ ] Commit: `Bind a region to each team and person, once`.

### Task 8: `/v1/bench` routes, access, wake, quota, ingress allow-list

**Files:**
- Create: `crates/workspaces/src/api/bench.rs` (under 500 lines); tests where the `ssh_session` handler's tests live (grep `ssh_session` under `crates/workspaces/tests` and `crates/workspaces/src/api`; add `api_bench.rs` beside them with the same mock-kube harness)
- Modify: `crates/workspaces/src/api/mod.rs:198-231` (routes), `crates/workspaces/src/api/workspaces/mod.rs:440` (`gateway_url` to `pub(crate)`), `crates/workspaces/src/api/workspaces/attach.rs` (extract the NetworkPolicy and environment checks into a function taking `(id, namespace, region)`, called by both `attach_ws` and `attach_bench`), `crates/workspaces/src/quota.rs:135-175`, `crates/workspaces/src/api/keys.rs:131` (the departed-member pass in `run_beat`, beside `prune_builders` at `:275`), `crates/workspaces/src/model.rs` (`DEFAULT_BENCH_IMAGE`, `DEFAULT_BENCH_MODEL`), `deploy/pin.sh` (rewrite the bench image pin beside the workspace image), `deploy/kloudlite-web.yaml:246` (allow-list and its comment)

**Interfaces:**
```rust
#[derive(Deserialize)] pub(crate) struct TeamQuery { team: Option<String> }
#[derive(Deserialize)] pub(crate) struct NewBench { team: Option<String>, #[serde(default)] region: Option<String>, #[serde(default)] model: Option<String> }
#[derive(Deserialize)] pub(crate) struct AttachBody { environment: String }

pub(crate) async fn get_bench(..)     // GET  /v1/bench?team=           200 doc | 404
pub(crate) async fn create_bench(..)  // POST /v1/bench                 201 created | 200 existing, now Running
pub(crate) async fn start_bench(..)   // POST /v1/bench/start?team=     202
pub(crate) async fn stop_bench(..)    // POST /v1/bench/stop?team=      202
pub(crate) async fn bench_session(..) // POST /v1/bench/session?team=   201 {id, token, gateway, expires_at} | 202 {state} | 409 stopped
pub(crate) async fn attach_bench(..)  // POST /v1/bench/attach?team=    202
pub(crate) async fn detach_bench(..)  // POST /v1/bench/detach?team=    202
fn bench_doc(b: &crd::Bench, region: &str) -> serde_json::Value; // {id, owner, team, region, model, desiredState, access, phase, nodeName, conditions}
/// The region `team` is bound to, from the directory; the 409 (team) or 422 (person, no `region` given) otherwise.
/// A person's first call with `region` binds it (`check_region`, then `Directory::bind_region`).
async fn team_region(s: &ApiState, caller: &Caller, team: &str, first: Option<&str>) -> Result<String, Response>;
/// What the caller may do with this bench.
enum Standing { Member, Departed }
/// caller, normalized team, region, standing, and the caller's own bench — or the 404 every other case gets.
async fn my_bench(s: &ApiState, headers: &HeaderMap, team: Option<&str>) -> Result<(Caller, String, String, Standing, Option<crd::Bench>), Response>;
```
Rules, all in `my_bench` so no handler can forget one:
- `caller()`; team trimmed and lowercased; absent or empty → `caller.name` (decision 4).
- GET `crd::bench_id(&caller.name, &team)`; a found object with `spec.owner != caller.name` → 404. No superadmin arm anywhere in this file.
- `may_allocate_for(s, &caller, &team)` (membership, never the superadmin claim) → `Member`. Not a member and a Bench exists → `Departed` (decision 10). Not a member and no Bench → 404 "no such team". This is the spec's `may_act(person, team)`.
- `team_region(s, &caller, &team, None)`; it is never cached, so an admin's bind takes effect on the next call.
- Every handler's access write goes through one helper, `ensure_access(&api, &b, standing)`: a JSON merge patch of `spec.access` only when it differs from `Full` for `Member` or `ReadOnly` for `Departed`.

Handlers:
- `create_bench`: `Departed` → 404 "no such team". `team_region(.., body.region.as_deref())`; for a team, a body `region` that differs from the binding → 409 "team {team} is in region {r}". Existing → `ensure_access`, `set_desired::<crd::Bench>(Running)` plus `wakeAt = now` in the same patch, and 200 with the doc. New → `guard_alloc(&s, &caller.name, false, &bench_cost(&PodResources::default()))`; create with `labels(&caller.name, "bench")` plus `TEAM_LABEL`, `image: DEFAULT_BENCH_IMAGE`, `model` defaulting to `DEFAULT_BENCH_MODEL`, `access: Full`; spawn the user-key install the way `create_ws` spawns `install_user_key_after_placed` (read its signature; if it is Workspace-bound, generalize its lookup to take the namespace rather than copying it).
- `start_bench`: `ensure_access`; `guard_alloc` for `spec.resources` unless the bench already wants a pod (`crd::bench_wants_pod`); then one merge patch `{desiredState: Running, wakeAt: now}`. `stop_bench`: `set_desired(Stopped)`.
- `bench_session`: `ensure_access`; `desiredState` Stopped → 409 `{"error": "bench is stopped; start it"}`. Phase `Idle` → `guard_alloc` for `spec.resources`, patch `wakeAt = now`, answer 202 `{"state": "waking"}`. Any other phase but `ready` → 202 `{"state": "{phase}"}` and no write. Ready → `s.jwt.mint_bench_session(&caller.name, &id, &region)`; `gateway = gateway_url(&region, &id)`; 201 with the response shape of `ssh_session` minus `host_key`.
- `attach_bench` / `detach_bench`: `Departed` → 404. The extracted attach function with the bench's namespace, `region` and `WORKSPACE_LABEL=id`; patch `spec.attachedEnvironment`.

Quota: add `benches.list(&lp)` to the `try_join!` in `usage`; for each Bench with `spec.owner == owner` and `desiredState == Running` and `status.phase != Idle`, add cpu and memory of `spec.resources`. `bench_cost` lives beside `workspace_cost`.

Departed-member pass, in `keys::run_beat` after `prune_builders`: list Benches; for each with `spec.team != spec.owner` whose owner is not a member (`teams_for(owner)` does not contain `spec.team`) and `access == Full`, merge-patch `access: ReadOnly` and log `bench.access.readonly` with owner and team. It never sets `Full`: rejoining is restored by the person's own next `/v1/bench` call, so an unreadable directory (`teams_for` fails closed to empty) can only ever take tools away, never hand them back.

- [ ] **Failing tests** (`api_bench.rs`), each built with the neighbouring `ssh_session` test's router and route recorder and called with `tower::ServiceExt::oneshot`:
```rust
#[tokio::test]
async fn creating_a_bench_twice_is_one_object_and_starts_it() {
    // first POST -> 201 and one POST benches recorded; seed the store with that object;
    // second POST (desiredState Stopped seeded) -> 200, no second create, one PATCH desiredState Running
}

#[tokio::test]
async fn a_non_member_without_a_bench_cannot_see_create_or_tunnel_to_a_teams_bench() {
    // directory: caller not in "acme", acme bound to r1, no Bench seeded; GET, POST, POST /start, POST /session with team=acme -> 404 each; recorder has no benches write
}

#[tokio::test]
async fn a_departed_member_reads_their_own_bench_and_nothing_more() {
    // seed Bench{owner alice, team acme, access Full, phase Idle}; directory: alice no longer in acme, acme bound to r1.
    // GET /v1/bench?team=acme            -> 200, and the recorded PATCH carries spec.access "ReadOnly"
    // POST /v1/bench/session?team=acme   -> 202 {"state":"waking"}, a PATCH with wakeAt
    // POST /v1/bench {"team":"acme"}     -> 404; POST /v1/bench/attach?team=acme -> 404
    // re-seed phase Ready                -> POST /session 201
    // run the departed pass of keys::run_beat over Bench{owner carol, team acme, access Full}, carol not in acme -> one PATCH access ReadOnly;
    // the same pass over a personal Bench{owner dave, team dave} -> no PATCH
}

#[tokio::test]
async fn the_region_comes_from_the_team_and_a_person_binds_their_own_once() {
    // acme unbound: POST /v1/bench {"team":"acme"} by a member -> 409, error == "team acme has no region; a platform admin binds one"
    // acme bound r1: POST {"team":"acme","region":"r2"} -> 409 "team acme is in region r1"; POST {"team":"acme"} -> 201, and the created Bench has no region anywhere in its spec
    // alice personal, unbound: POST {} -> 422 "choose a region for your personal bench"; POST {"region":"r2"} -> 201 and the stub records bind_region("alice","r2")
    // POST /v1/bench/session for the acme bench once Ready -> the token's region claim == "r1" and gateway == gateway_url("r1", id)
}

#[tokio::test]
async fn a_superadmin_claim_does_not_open_someone_elses_bench() {
    // caller superadmin, not a member; seeded bench owned by bob in acme; GET and POST /session -> 404
}

#[tokio::test]
async fn a_session_wakes_an_idle_bench_waits_on_a_starting_one_and_refuses_a_stopped_one() {
    // phase Idle -> 202 {"state":"waking"}, one PATCH whose spec.wakeAt parses as RFC 3339 within 5 s of now
    // phase Starting -> 202 {"state":"starting"}, no PATCH
    // phase Ready -> 201; token verifies with verify_bench_session and names the bench id and the team's region
    // desiredState Stopped -> 409, error == "bench is stopped; start it", no PATCH
    // phase Idle with the owner's quota already at its cpu limit -> 409 with quota::refuse's sentence, no PATCH
}

#[tokio::test]
async fn a_bench_costs_the_person_only_while_it_has_a_pod() {
    // seed Bench{owner alice, team acme, Running, phase Ready, cpu_limit "1"}; quota::usage(alice).millicores += 1000; usage(acme) unchanged;
    // phase Idle: usage(alice) += 0; desiredState Stopped: usage(alice) += 0
}

#[test]
fn the_admin_router_has_no_bench_route() {
    for src in [include_str!("../src/api/admin.rs")] {
        assert!(!src.contains("/bench"), "no platform surface reads a person's bench");
    }
    // and every file under src/api/admin/: iterate with std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src/api/admin"))
}
```
The comments are the exact assertions; write them out in full.
- [ ] Run: `cargo test -p kloudlite-workspaces --test api_bench` → FAIL: unresolved `create_bench`.
- [ ] Implement; add the routes before `.with_state(state)`:
```rust
.route("/v1/bench", get(get_bench).post(create_bench))
.route("/v1/bench/start", post(start_bench))
.route("/v1/bench/stop", post(stop_bench))
.route("/v1/bench/session", post(bench_session))
.route("/v1/bench/attach", post(attach_bench))
.route("/v1/bench/detach", post(detach_bench))
```
- [ ] Ingress: `path: /v1/(cli|workspaces|keys|internal|builders|bench)(/.*)?$`, with a comment sentence saying `bench` is the CLI's and the harness's route to their own bench.
- [ ] Run → ok; `cargo test -p kloudlite-workspaces` → no regressions; clippy.
- [ ] Commit: `Serve /v1/bench to the person only: create, start, stop, wake, attach and a tunnel token`.

### Task 9: Gateway resolves a bench to port 7789

**Files:**
- Modify: `bins/gateway/src/resolve.rs` (add `resolve_bench` after `resolve`), `bins/gateway/src/tunnel.rs:33-64` (`bench_port` on `Gateway`) and `:162-230` (accept either token), `bins/gateway/src/main.rs` (pass `k8s::BENCH_PORT`), the gateway's tunnel tests (grep `ssh_port` under `bins/gateway` for where `Gateway::new` is exercised), `deploy/k3s/gateway.yaml` (its ClusterRole gains `benches: get`)

**Interfaces:**
```rust
// resolve.rs
pub async fn resolve_bench(client: &kube::Client, id: &str, port: u16) -> Result<Target, Refusal>; // is_dns_label; Bench phase Ready; podRef -> pod IP:port; owner = spec.owner
// tunnel.rs
pub struct Gateway { /* existing */ pub bench_port: u16 }
pub fn new(jwt: Jwt, region: String, kube: kube::Client, ssh_port: u16, bench_port: u16) -> Gateway;
enum Ticket { Workspace(SshSessionClaims), Bench(BenchSessionClaims) }
impl Ticket { fn id(&self) -> &str; fn region(&self) -> &str; fn jti(&self) -> &str; fn exp(&self) -> u64 }
```
In `tunnel`: `verify_ssh_session` else `verify_bench_session` else 401. `ticket.id() != path id || ticket.region() != gw.region` → 401. `reserve(&id)` unchanged. Resolve by kind: `resolve(&gw.kube, &id, gw.ssh_port)` or `resolve_bench(&gw.kube, &id, gw.bench_port)`. Everything after the dial — charge, spend, upgrade, pump — is shared and unchanged. The comment on `set_nodelay` stays true for WebSocket frames. Module doc gains one sentence: a bench is a second target of the same pipe.

- [ ] **Failing tests** (the local echo listener the ssh tunnel tests use; a mock kube serving a Bench with `phase: Ready`, `podRef: ws-alice/bench`, and a pod with `podIP: 127.0.0.1`):
```rust
#[tokio::test]
async fn a_bench_token_opens_a_tunnel_to_the_bench_port() {
    // Gateway::new(.., ssh_port = unused, bench_port = echo port); connect with a bench token; send b"ping"; read b"ping"
}

#[tokio::test]
async fn a_workspace_token_cannot_open_a_bench_with_the_same_id() {
    // mint_ssh_session for "bench-1"; mock kube serves ONLY a Bench named bench-1 (no Workspace) -> status 404 from resolve, and the echo listener accepted nothing
}

#[tokio::test]
async fn a_bench_token_for_another_region_is_refused() {
    // mint_bench_session(.., region "r2") against a gateway in "r1" -> 401, no kube request recorded
}
```
- [ ] Run: `cargo test -p kloudlite-gateway a_bench_token` → FAIL.
- [ ] Implement; update every `Gateway::new` caller.
- [ ] Run → ok; `cargo test -p kloudlite-gateway`; clippy.
- [ ] Commit: `Tunnel to a bench's port with a bench token`.

### Task 10: `kl-connect bench`

**Files:**
- Create: `bins/kl-connect/src/bench.rs`
- Modify: `bins/kl-connect/src/main.rs:8-50` (`mod bench;`, `Cmd::Bench { #[arg(long)] team: Option<String>, #[arg(long, default_value_t = 0)] port: u16, #[arg(long)] start: bool }` and its dispatch), `bins/kl-connect/src/api.rs` (`create_bench`, `get_bench`, `bench_session`), `bins/kl-connect/src/proxy.rs:54-` (split `pump` into `connect(url, token)` and a generic `pump_io`; `proxy` calls both unchanged in behaviour)

**Interfaces:**
```rust
// bench.rs
pub async fn bench(team: Option<&str>, port: u16, start: bool) -> Result<(), String>;
// api.rs
#[derive(Deserialize)] pub struct BenchSession { pub id: String, pub token: String, pub gateway: String, pub expires_at: String }
/// `region` is sent only for a personal bench (its first use binds the person's region); a team's is the team's.
pub async fn create_bench(cfg: &Config, team: Option<&str>, region: Option<&str>) -> Result<serde_json::Value, Error>;
pub async fn get_bench(cfg: &Config, team: Option<&str>) -> Result<serde_json::Value, Error>;
/// 201 → `Ready(session)`; 202 → `Waking(state)`; anything else an Error carrying the body's `error`.
pub enum SessionAnswer { Ready(BenchSession), Waking(String) }
pub async fn bench_session(cfg: &Config, team: Option<&str>) -> Result<SessionAnswer, Error>;
// bench.rs
pub const BENCH_START_WAIT: Duration = Duration::from_secs(90);
// proxy.rs
pub(crate) async fn connect(url: &str, token: &str) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, String>;
pub(crate) async fn pump_io<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(ws: WebSocketStream<MaybeTlsStream<TcpStream>>, r: R, w: W) -> Result<(), String>;
```
Behaviour: this is the bench's builder gate, on the laptop. With `--start`, `create_bench` (region = the config's default region, sent only when `team` is absent or equals the caller's handle) first. Bind `127.0.0.1:{port}`. Print exactly one stdout line, `127.0.0.1:<port>`, and flush. For each accepted connection spawn a task that holds the connection open and loops: `bench_session`; on `Waking(state)` print `bench is {state}; waiting` to stderr once per state change, sleep 1 s and ask again, giving up after `BENCH_START_WAIT` (the connection is closed and stderr says "bench did not start within 90 s"); on `Ready(s)`, `connect(gateway_url(&s.gateway), &s.token)`, `pump_io(ws, read_half, write_half)`. A connection's bytes are never read before the pump starts, so a client that sent a request while the bench slept has it delivered once it is up. A 409 ("bench is stopped; start it") prints that sentence and closes that connection; the listener keeps serving. A 401 prints "your login has expired — run `kl-connect login`" and exits non-zero; any other per-connection error is logged to stderr and the listener keeps serving. The `//!` doc says the token never appears in output, as `proxy.rs`'s does.

- [ ] **Failing test** in `bench.rs`:
```rust
#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn each_local_connection_gets_its_own_tunnel_and_token() {
        // One local axum app serves POST /v1/bench/session (an AtomicUsize counter; returns 201 with gateway "wss://x/tunnel/bench-1")
        // and GET /tunnel/bench-1 (WebSocket echo of binary frames).
        // KL_CONFIG_DIR = tempdir with config.json pointing api at the app; KL_GATEWAY_OVERRIDE = "ws://127.0.0.1:{app port}".
        // Run bench_on(listener) — the testable core `bench` wraps — on a pre-bound 127.0.0.1:0 listener.
        // Open two TcpStreams; write b"a" / b"b"; read back b"a" / b"b".
        // assert counter == 2
    }

    #[tokio::test]
    async fn a_sleeping_bench_is_waited_for_and_the_early_bytes_arrive() {
        // The same app, but POST /v1/bench/session answers 202 {"state":"waking"} for its first two calls and 201 after.
        // Open one TcpStream and write b"early" at once; read back b"early" within 5 s.
        // assert counter == 3 and the tunnel route saw exactly one upgrade
    }
}
```
Extract `async fn bench_on(listener: TcpListener, cfg: Config, team: Option<String>)` so the test needs no stdout parsing; write the body in full.
- [ ] Run: `cargo test -p kl-connect each_local_connection` → FAIL.
- [ ] Implement. No new dependencies (musl build).
- [ ] Run → ok; `cargo test -p kl-connect` (the existing proxy tests prove the split kept behaviour); clippy.
- [ ] Commit: `Open a bench on a local port with kl-connect bench, waking it on connect`.

### Task 11: The bench image and its CI job

**Files:**
- Create: `deploy/bench/Dockerfile`, `deploy/bench/harness-bench-stub.sh`
- Modify: `.github/workflows/image.yml` (a build-push step after the workspace image step at `:228-236`, same shape), `.dockerignore` (whitelist `deploy/bench/`; the name whitelist broke the workspace-caches merge)

**Interfaces:**
- Image: `node:22-bookworm-slim`; `npm i -g @mariozechner/pi-coding-agent@<the version in harness/package-lock.json>`; `kl` copied from the `kl-musl` artifact the workflow already downloads; `harness-bench` = `harness/bench/dist/harness-bench` when the other plan has produced it, else the stub (a build stage does `test -f` and copies one or the other); `util-linux` for `flock`, `socat`; user `1000:1000`; `WORKDIR /home/kl`; `ENTRYPOINT []` (the pod sets `command`).
- Stub contract (the same interface the real program honours):
  - `harness-bench [--read-only]`: `exec 9>/bench/.lock; flock -n 9 || { cat /bench/.lock.holder > /dev/termination-log; exit 75; }`; write `$NODE_NAME` to `/bench/.lock.holder`; serve one connection at a time, each under the idle clock: `while timeout "${KL_BENCH_IDLE_SECS:-300}" socat TCP-LISTEN:7789,reuseaddr SYSTEM:'printf "HTTP/1.1 200 OK\r\ncontent-length: N\r\n\r\nok stub MODE"'; do :; done; echo idle > /dev/termination-log; exit 0` where MODE is `read-only` or `running`. `timeout` exits 124 only when no connection arrived for the whole period, which is the stub's "no client and nothing running"; the stub runs no tools.
  - `harness-bench --ping`: `socat -u TCP:127.0.0.1:7789 - </dev/null | grep -q ok`.

- [ ] **Failing check** in the dev pod's build path (`deploy/dev-push.sh` or the builder gate): `docker buildx build -f deploy/bench/Dockerfile .` → fails, file not found.
- [ ] Write both files. Build. Then, with a scratch folder mounted at `/bench`: start one container detached with `harness-bench`; `docker exec` it with `harness-bench --ping` → exit 0; start a second container on the same folder → exits 75 and its termination file names the first one's `NODE_NAME`; stop both, start one with `KL_BENCH_IDLE_SECS=5` and connect nothing → it exits 0 within 10 s with `idle` in its termination file.
- [ ] Add the CI step (tags `ghcr.io/kloudlite/kloudlite-bench:latest` and `:${{ github.sha }}`); add the bench image to `deploy/pin.sh`'s package check (Task 8 added the rewrite).
- [ ] Commit: `Package the bench image with pi, kl and a stub harness-bench`.

### Task 12: The tool server on the pod IP, fenced by its namespace

**Files:**
- Modify: `crates/workspaces/src/k8s/mod.rs` (`IDE_PORT` beside `WORKSPACE_LABEL`), `crates/workspaces/src/k8s/workspace.rs:156` (the prelude's `kl ide serve` line), `crates/workspaces/src/k8s/tests/pod.rs:446-460`, `crates/workspaces/src/k8s/policies.rs` (`allow_bench_tools` beside `allow_gateway_ingress`; `intercept_ingress` takes the ports; the `//!` list), `crates/workspaces/src/k8s/tests/environment.rs:302`, `bins/agent/src/controller/environment/intercept.rs:274`, `bins/agent/src/binding.rs:148`, `bins/kl/src/main.rs:57` (help text), `bins/kl-connect/src/ws.rs:25` (comment), `crates/ide/src/lib.rs:6` (module doc), `CLAUDE.md` (the tool-server paragraph)

**Interfaces:**
```rust
// k8s/mod.rs
pub const IDE_PORT: u16 = 7788;
// k8s/policies.rs
/// Workspace pods accept IDE_PORT from bench pods in their own namespace: a person's bench runs their workspace sessions.
pub fn allow_bench_tools(ns: &str, owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy;
/// Now scoped to `ports`, the workspace-side ports of the intercept. Empty admits nothing, never everything.
pub fn intercept_ingress(ws_ns: &str, env_ns: &str, ws_id: &str, ports: &[u16], owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy;
```
`allow_bench_tools`: name `allow-bench-tools`; `podSelector` `matchExpressions: [{key: WORKSPACE_LABEL, operator: Exists}]`; one ingress rule `from: [{podSelector: {matchLabels: {KIND_LABEL: "bench"}}}]` with NO `namespaceSelector` (the policy's own namespace only), `ports: [{protocol: TCP, port: IDE_PORT}]`. Bench pods carry `KIND_LABEL=bench` (Task 3).

- [ ] **Failing tests.** In `k8s/tests/pod.rs`, add to `the_prelude_starts_kl_ide_serve_as_kl_before_sshd`:
```rust
    // The bench dials the tool server on the pod IP (the bench spec's What runs where); the ssh tunnel still reaches it on loopback.
    assert!(line.contains("exec kl ide serve --bind 0.0.0.0:7788 "), "{line}");
```
In `k8s/tests/environment.rs`, beside the existing `intercept_ingress` test (which gains `&[3000]` as its ports argument):
```rust
#[test]
fn an_intercepting_environment_reaches_only_the_intercepted_ports() {
    let r = owner_ref_fixture();
    let np = intercept_ingress("ws-acme", "env-abc", "ws-1", &[3000, 9229], "acme", &r);
    let rule = &np.spec.unwrap().ingress.unwrap()[0];
    let ports: Vec<_> = rule.ports.as_ref().unwrap().iter().map(|p| p.port.clone()).collect();
    assert_eq!(ports, vec![Some(IntOrString::Int(3000)), Some(IntOrString::Int(9229))]);
    let none = intercept_ingress("ws-acme", "env-abc", "ws-1", &[], "acme", &r);
    assert!(none.spec.unwrap().ingress.unwrap_or_default().is_empty(), "no ports admits nothing, never every port");
}

#[test]
fn only_bench_pods_in_the_namespace_reach_the_tool_port() {
    let np = allow_bench_tools("wt-alice-acme", "alice", &owner_ref_fixture());
    let spec = np.spec.unwrap();
    let sel = spec.pod_selector.match_expressions.unwrap();
    assert_eq!((sel[0].key.as_str(), sel[0].operator.as_str()), (WORKSPACE_LABEL, "Exists"));
    let rule = &spec.ingress.unwrap()[0];
    let from = rule.from.as_ref().unwrap();
    assert_eq!(from.len(), 1);
    assert!(from[0].namespace_selector.is_none(), "this namespace only");
    assert_eq!(from[0].pod_selector.as_ref().unwrap().match_labels.as_ref().unwrap()[KIND_LABEL], "bench");
    assert_eq!(rule.ports.as_ref().unwrap(), &vec![NetworkPolicyPort { protocol: Some("TCP".into()), port: Some(IntOrString::Int(IDE_PORT as i32)), end_port: None }]);
}
```
`owner_ref_fixture` is whatever the existing test at `environment.rs:302` builds its `r` with; reuse it, do not add a second.
- [ ] Run: `cargo test -p kloudlite-workspaces the_prelude_starts_kl_ide_serve an_intercepting_environment only_bench_pods` → FAIL: the prelude assertion, and `cannot find function allow_bench_tools`.
- [ ] Implement:
  - Prelude: `exec kl ide serve --bind 0.0.0.0:{IDE_PORT} >> …`, rendered, never a prelude variable (the `su -c` trap the test comment records).
  - `intercept_ingress`: `"ports": ports.iter().map(|p| json!({"protocol": "TCP", "port": p}))`; with `ports` empty write `"ingress": []`.
  - `intercept.rs:274`: pass the workspace-side ports this pass writes into the EndpointSlice. Read the slice-building code above `:274`: the slice's port numbers are the workspace ports (`Intercept::workspace_port` over the service's ports). Pass that list, deduplicated; never re-derive it.
  - `binding.rs:148`: `ensure(&policies, &k8s::allow_bench_tools(&ns, owner, &owner_ref), ctx).await?;` beside `allow_gateway_ingress`.
  - Docs: `bins/kl` help "Serve on 0.0.0.0:7788: a person's bench reaches it inside the namespace, `kl-connect ws ide` over the tunnel"; the same fact in `ws.rs:25` and `crates/ide/src/lib.rs:6`. In `CLAUDE.md`, replace "on `127.0.0.1:7788` and nowhere else — the ssh tunnel a person already holds (`kl-connect ws ide <ws>`) is the boundary, so there is no second credential and no auth code in the crate" with "on port 7788 of the pod IP, fenced by the namespace: only the person's own bench (`allow-bench-tools`) and the ssh tunnel (`kl-connect ws ide <ws>`) reach it, an intercepting environment only on its intercepted ports, and `/v1/workspaces/{id}/tools` hands the address to the owner alone — so there is no second credential and no auth code in the crate".
- [ ] Run the three tests → ok. `cargo test -p kloudlite-workspaces` and `cargo test -p kloudlite-agent-bin` → no regressions (re-run a single wall-clock flake once).
- [ ] Clippy; commit: `Serve the workspace tool server on the pod IP for the owner's bench`.

### Task 13: `GET /v1/workspaces/{id}/tools`: the owner's tool server address

**Files:**
- Modify: `crates/workspaces/src/api/workspaces/mod.rs` (handler beside `get_ws` at `:421`), `crates/workspaces/src/api/mod.rs:183` (route); tests where `get_ws` is tested (grep `get_ws` under `crates/workspaces/tests` and `crates/workspaces/src/api`)

**Interfaces:**
```rust
#[derive(Deserialize)] pub(crate) struct ToolsQuery { team: Option<String> }
/// 200 {address} to spec.owner while Ready; 409 {error} naming the state, the team, or a pod between restarts; 404 otherwise.
pub(crate) async fn ws_tools(State(s): State<Arc<ApiState>>, headers: HeaderMap, Path(id): Path<String>, Query(q): Query<ToolsQuery>) -> Result<Response, Response>;
```
Rules, in order:
- `caller`; then the workspace through `my_ws` (read it). Whatever `my_ws` admits, require `spec.owner == caller.name` HERE, never by changing `my_ws`: a workspace session is the person's, so a team member or a superadmin acting for the workspace gets 404 like any stranger.
- `status.phase != Ready` → 409 `{"error": "workspace {spec.name} is {phase}; start it to run tools"}`, phase lowercased.
- `q.team` given, normalized like `/v1/bench` (trimmed, lowercased, empty meaning the caller's handle), and not equal to `spec.team` (empty meaning the owner) → 409 `{"error": "workspace {spec.name} is in team {team}; open it from that team's bench"}`.
- `status.podRef` → GET pod → `status.podIP`; any miss → 409 `{"error": "workspace {spec.name} is between pods; try again"}`. This is the gateway's `resolve` (`bins/gateway/src/resolve.rs:24`) without the port; it lives in another binary, so copy its six lines rather than link it.
- 200 `{"address": format!("{ip}:{}", k8s::IDE_PORT)}`.

The ingress allow-list already carries `workspaces`; no change there.

- [ ] **Failing test** (the neighbouring `get_ws` test's router and mock kube, called with `oneshot`):
```rust
#[tokio::test]
async fn the_tool_address_is_the_owners_alone_and_only_while_ready() {
    // Seed Workspace w1 {owner alice, team acme, name "api", phase Ready, podRef "wt-alice-acme/ws"} and Pod ws {podIP 10.42.0.9}.
    // alice GET /v1/workspaces/w1/tools                -> 200, body == {"address":"10.42.0.9:7788"}
    // bob, a member of acme, same request             -> 404
    // a superadmin who is not alice                   -> 404
    // alice GET /v1/workspaces/w1/tools?team=labs      -> 409, error contains "is in team acme"
    // alice ?team=acme                                 -> 200
    // re-seed w1 with phase Stopped; alice             -> 409, error == "workspace api is stopped; start it to run tools"
    // re-seed Ready with the Pod absent; alice         -> 409, error contains "between pods"
}
```
The comments are the exact assertions; write each out in full.
- [ ] Run: `cargo test -p kloudlite-workspaces the_tool_address_is_the_owners` → FAIL: unresolved `ws_tools`.
- [ ] Implement; route: `.route("/v1/workspaces/{id}/tools", get(ws_tools))` after the `{id}` route.
- [ ] Run → ok; `cargo test -p kloudlite-workspaces` → no regressions; clippy.
- [ ] Commit: `Tell a workspace's owner where its tool server listens`.

### Task 14: SLO ids, catalogue, `deploy/slo.md`, probe stage

**Files:**
- Create: `bins/slo/src/stages/bench.rs`
- Modify: `crates/workspaces/src/slo/catalogue.rs` (`CATALOGUE` rows; the stage→ids table after `STAGES` at `:101`), `deploy/slo.md` (rows in catalogue order), `bins/slo/src/stages/mod.rs:36-55` (`pub mod bench;` and the calls from the fast journey's stage 5, the hourly experience stage and the weekly stage), `deploy/k3s/quotas-slo.yaml` (one bench's cpu/memory added to each probe owner)

**Interfaces — ids:**

| id | suite | stage | target | SLI |
|---|---|---|---|---|
| `bench.create` | Fast | 5 · Workspace | avail 99.9 | `POST /v1/bench` answers, and a second POST names the same id |
| `bench.start.p95` | Fast | 5 · Workspace | p95 90 000 ms | A started bench reaches phase `ready` |
| `bench.tunnel` | Fast | 5 · Workspace | bound 20 000 ms | A bench token opens the tunnel and `/healthz` answers through it |
| `bench.idle.wake` | Hourly | 14 · Experience | bound 480 000 ms | With every client gone past `benchIdleSecs` the bench has no pod, a new connection starts it, and the session list and a transcript read back unchanged |
| `bench.session.roundtrip` | Hourly | 14 · Experience | bound 60 000 ms | A session is created, a no-tools prompt answered, and read back from `/sessions/{id}/messages` |
| `bench.exchange.both_views` | Hourly | 14 · Experience | avail 99.9 | An exchange reads back by `?session=` and by `?workspace=` |
| `bench.two_clients` | Hourly | 14 · Experience | avail 99.9 | Two WebSockets on one session see the same events in the same order |
| `bench.survives.reschedule` | Weekly | 12 · Weekly | bound 180 000 ms | After the pod is deleted every session reopens and processes read `lost` |
| `bench.workspace.tool_roundtrip` | Hourly | 14 · Experience | bound 180 000 ms | A workspace session on the bench runs `exec echo` in a workspace through its tool server, and the turn lands under `/bench/workspaces/{ws}/` |

`bench.idle.wake` goes beyond the spec's list: "an idle bench costs nothing and a connection brings its history back" is otherwise unprobed. Its bound is the region's default `benchIdleSecs` (300 s) plus a 90 s start plus 90 s of reads; a region that raises the knob raises the ceiling constant with it. `bench.workspace.tool_roundtrip` holds the whole chain the spec's "What runs where" draws: `/v1`'s address, `allow-bench-tools`, the tool server on the pod IP and the thread file. The hourly and weekly ids need the real `harness-bench`; while `/healthz` answers `ok stub …` the stage SKIPS them with the reason "bench image is the stub". A skipped id must reach the run row as skipped, never as passed (the service-intercept merge found skipped ids reading as passed).

- [ ] **Failing test** in `catalogue.rs` (beside `the_catalogue_matches_deploy_slo_md`):
```rust
#[test]
fn every_bench_id_is_catalogued() {
    for id in ["bench.create", "bench.start.p95", "bench.tunnel", "bench.idle.wake",
               "bench.session.roundtrip", "bench.exchange.both_views", "bench.two_clients",
               "bench.survives.reschedule", "bench.workspace.tool_roundtrip"] {
        assert!(find(id).is_some(), "{id} missing from CATALOGUE");
    }
}
```
- [ ] Run: `cargo test -p kloudlite-workspaces every_bench_id` → FAIL.
- [ ] Add the catalogue rows and the matching `deploy/slo.md` rows; run both tests → ok.
- [ ] Implement `stages/bench.rs` with ceilings as constants at the top, each at least its target (the rule in `stages/workspace.rs`'s doc):
  - `fast(ctx)`: create twice (same id) → poll ready → `POST /v1/bench/session` → open the tunnel with the WebSocket client `stages/workspace.rs` uses for `gw.tunnel.p95` and write a raw `GET /healthz HTTP/1.1\r\nhost: bench\r\n\r\n`, expecting `ok` → stop → `POST /v1/bench/session` answers 409 "bench is stopped; start it" and `GET /v1/bench` shows phase `stopped` → start again. The probe owner's region is bound once by hand (Task 15). The object name is a hash, so the `run-{id}` teardown prefix does not apply: the probe owner's bench is long-lived and is left Running with no client, so it sleeps between runs and costs nothing.
  - `hourly(ctx)`: `bench.idle.wake` runs first, before the session ids, because it needs every client gone: close the forward; poll `GET /v1/bench` until phase `idle` and `kubectl get pod bench` in its namespace answers NotFound, within `benchIdleSecs` read from the region's `ClusterSettings` plus 60 s; record the phase change and the pod's absence as two checks in the stage's log. Open a new forward and a new connection: `GET /v1/bench` goes `starting` then `ready`, and through the forward `GET /sessions` equals the list read before the forward closed and `GET /sessions/{first}/messages` has the same `total`. With the stub image the history half cannot be read, so the id is SKIPPED with "bench image is the stub" after the no-pod check passes, never recorded as passed. Then skip-or-run the four session ids through a local forward built on `kl-connect`'s `bench_on` (or the `kl-connect` binary the probe image ships for `id.cli.flow`). `bench.workspace.tool_roundtrip`: create a workspace the way `ide_server` does (`bins/slo/src/stages/experience_ws.rs:350`) and wait for `ide.serve.up`'s healthz; `POST /workspaces/{ws}/session` through the forward; on `WS /sessions/w-{ws}/rpc` prompt "Run `echo bench-$(id -un)` with the bash tool."; pass on a `tool_execution_end` whose `toolName` is `bash` and whose result text contains `bench-kl` (the event, never the model's prose); then `GET /workspaces/{ws}/messages` has `total > 0`, and `kubectl exec` in the bench pod finds `/bench/workspaces/{ws}/thread.jsonl` non-empty. Drop the workspace at the end as `ide_server` does.
  - `weekly(ctx)`: delete the bench pod with the probe's kube client (reuse the drill's existing pod-delete grant in `slo-rbac.yaml`; add `benches` to its resource list if the grant is resource-scoped), then the reschedule checks, or skip with the stub reason.
- [ ] Run `cargo test -p kloudlite-slo` and `cargo test -p kloudlite-workspaces`; clippy.
- [ ] Commit: `Probe benches: create, start, tunnel, sleep and wake, the session journey and a workspace's tools`.
- [ ] **End of batch:** `cargo test` for the whole workspace and clippy green in the pod; then `git push origin master` and `git push platform master` as two separate steps.

### Task 15: Ship, pin, roll, apply, verify on the fleet

- [ ] Wait for `image.yml` on the pushed SHA: the test job green and `kloudlite-bench:<sha>` published.
- [ ] `deploy/pin.sh <sha>` (refuses a SHA with no package, the bench image included); commit `Pin every tier to <sha>`; push both remotes.
- [ ] `deploy/roll.sh` for AKS (api, gateway). On the k3s region, by hand per `deploy/k3s/README.md`, in this order: `crds.yaml`, `agent-rbac.yaml`, `agent-admission.yaml`, `api-rbac.yaml`, `gateway.yaml`, `slo-rbac.yaml`, `quotas-slo.yaml`; the agent DaemonSet then rolls on its repin. Do not edit `/work/src` while a ship runs.
- [ ] Bind the regions once, with a superadmin token against the admin process: `PUT $ADMIN_API/admin/owners/kloudlite/region {"region":"centralindia-k3s"}` and the same for each SLO probe owner (a personal probe owner binds on its own first `POST /v1/bench`; a probe team does not). Expect 200, and 409 on a second call with another region.
- [ ] Direct checks with a minted token, not a wait for the hourly:
```sh
curl -sX POST "$API/v1/bench" -H "authorization: Bearer $T" -d '{"team":"kloudlite"}'
kubectl get benches -o jsonpath='{.items[*].spec}' | grep -c region  # 0: the bench stores no region
kubectl get benches -o wide                                          # Node set, Phase Ready
kubectl -n "$NS" get pod bench -o jsonpath='{.spec.volumes[*].hostPath.path}'   # includes /homes/.benches/kloudlite/<owner>
kl-connect bench --team kloudlite &                                  # prints 127.0.0.1:PORT
curl -s "127.0.0.1:$PORT/healthz"                                    # ok stub running
kill %1; sleep 360                                                   # past the default benchIdleSecs with no client
kubectl -n "$NS" get pod bench                                       # NotFound
kubectl get bench "$ID" -o jsonpath='{.status.phase} {.status.idleSince}'   # Idle <rfc3339>
kl-connect bench --team kloudlite & sleep 1; time curl -s "127.0.0.1:$PORT/healthz"   # ok stub running, after a cold start of seconds
curl -sX POST "$API/v1/bench/stop?team=kloudlite" -H "authorization: Bearer $T"
curl -s "127.0.0.1:$PORT/healthz"; kubectl -n "$NS" get pod bench     # the connection closes naming "bench is stopped; start it"; NotFound
curl -sX POST "$API/v1/bench/start?team=kloudlite" -H "authorization: Bearer $T"
kubectl auth can-i list benches.kloudlite.io --as=system:serviceaccount:kloudlite:kloudlite-admin   # no
WS_IP=$(kubectl -n "$NS" get pod "$WS_POD" -o jsonpath='{.status.podIP}')                      # one of your workspaces in this team
curl -s "$API/v1/workspaces/$WS/tools?team=kloudlite" -H "authorization: Bearer $T"          # {"address":"$WS_IP:7788"}
curl -s "$API/v1/workspaces/$WS/tools" -H "authorization: Bearer $OTHER_MEMBER_T"             # 404
kubectl -n "$NS" exec bench -- node -e "fetch('http://$WS_IP:7788/healthz').then(r=>r.text()).then(console.log)"   # "ok":true
kubectl -n "$ENV_NS" exec "$INTERCEPTING_POD" -- sh -c "timeout 5 nc -z $WS_IP 7788; echo \$?"   # non-zero: an intercepting environment cannot reach the tool port
```
- [ ] Departed member, with a scratch team `bench-leave` bound to the region and a second person `$U2` in it: `$U2` creates a bench there and connects; the owner removes `$U2` from the team; within `KEYS_RESYNC_SECS` (300 s) `kubectl get bench "$ID2" -o jsonpath='{.spec.access}'` reads `ReadOnly` and the pod's command ends in `--read-only`; `$U2`'s `GET /v1/bench?team=bench-leave` answers 200 and `POST /v1/bench/attach?team=bench-leave` answers 404; `$U2`'s `GET /v1/workspaces/$WS2/tools` answers 404. Delete the scratch team afterwards.
- [ ] Run the fast suite by hand (`deploy/dev/run-job.sh fast`): `bench.create`, `bench.start.p95`, `bench.tunnel` pass. Run the hourly suite by hand once: `bench.idle.wake` records its no-pod check and then shows as skipped with "bench image is the stub", as do the other hourly and weekly bench ids, `bench.workspace.tool_roundtrip` among them, until harness-bench ships. `ide.serve.up` and `ide.exec` still pass: the tunnel's loopback path is unchanged.
- [ ] Call it shipped only after the fast run passed on the carrying SHA; record the SHA and the skipped ids on the status board.
