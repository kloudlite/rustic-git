# Bench platform: the `Bench` object, its pod, its folder, its tunnel

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax for tracking. Every edit happens in the dev pod (`/work/src`); nothing is fixed until the step passed on the fleet on the carrying build.

**Goal:** the platform half of `docs/superpowers/specs/2026-09-13-bench-sessions-server-side-design.md` — a person's `Bench` per team, placed and reconciled by the node agent, whose pod mounts that person's bench folder from the region share at `/bench`, reachable only through the gateway tunnel on port 7789, created and driven through `/v1/bench*`, opened on a laptop by `kl-connect bench`, able to reach the person's own workspaces' tool servers for the workspace sessions it runs, and held on the fleet by SLO ids.

**Architecture:** one new cluster-scoped CRD beside `Workspace`, reusing every mechanism a workspace already has — the claim (`claim::claim`), the shared-share mount (`mount_homes`), the `ensure_shared_home` shape, the owner namespace (`ws_namespace`), the `user-key` Secret, the attach `resolv.conf`, the ssh-session JWT pattern and the gateway pump. The bench differs only in having no btrfs volume, no sshd, and a different port.

**Out of scope:** the `harness-bench` program and the harness UI (the other plan). This plan relies only on its interface: a listener on `0.0.0.0:7789`, a `--read-only` flag, a `--ping` flag, exit code 75 when the folder lock is held with the holder in `/dev/termination-log`, `KL_TEAM` passed on to the `workspace-tools` extension, all shipped in `ghcr.io/kloudlite/kloudlite-bench`. Until that image carries the real program, Task 10 ships a stub honouring the same interface so every platform step is verifiable on its own.

## Decisions this plan makes (the spec left them open or the code contradicts it)

1. **Region is stored on the Bench (deviation from the spec, flagged).** The spec says "no region field — read from the team". No team→region binding exists in the code (`RegionSpec` is `{name, status}`; the directory has no region; `create_ws` takes `region` from the body). The claim, the gateway's region check and `ensure_binding` all need a region on the object. So `POST /v1/bench` takes `region` (checked by `check_region`), writes it once into `spec.region`, and later calls for that `(owner, team)` ignore a different one. A `// ponytail:` on the field names the upgrade: when teams get a region binding, `/v1` fills it from the team and refuses a mismatch.
2. **Folder path is `{pool}/homes/.benches/{team}/{person}` (deviation, flagged).** The export is mounted AT `{pool}/homes` (`mount_homes`), so the spec's sibling `{pool}/benches` would be node-local rootfs, not the share. A dot-prefixed directory at the share's root is still "beside the homes", keeps one mount and one repair path, and cannot collide with a person's home because `valid_owner` refuses a leading dot.
3. **Quota: the person** (open question 2, default taken). `quota::usage` sums bench cpu/memory under `spec.owner`: `spec.resources` when Running, the reader's 50m/128Mi when Stopped, because the reader is a real pod. No `benches` dimension. A team's usage never includes a bench.
4. **Personal bench is `team == owner`** (the spec's "a person's own handle"), which `ws_namespace` already maps to `ws-{owner}`. The API normalizes an absent team to the caller's handle, never to empty.
5. **Object name is `bench-{hash(owner, team)}`** (`crd::bench_id`, deterministic like `builder_id`), so "one per (owner, team)" is the API server's name uniqueness, not a read-then-write race.
6. **Stopped keeps a pod** (spec): `desiredState: Stopped` replaces the pod with `harness-bench --read-only`. Deleting the Bench removes the pod by ownerReference GC; no finalizer, and the platform never touches the folder.
7. **Tunnel token is a new `typ: "bench-session"`**, not a reused `ssh-session`: the gateway must know which kind to resolve and which port to dial, and a workspace token must never open a bench, nor the reverse.
8. **The bench listens on the pod IP, fenced by a NetworkPolicy (deviation from the spec's first draft; the other plan's Task 12 now binds `0.0.0.0` by default).** The gateway dials the pod IP and a bench pod has no sshd to port-forward through. `harness-bench` binds `0.0.0.0:7789`; the agent writes a `bench-ingress` NetworkPolicy admitting 7789 only from the gateway pods. The k3s regions enforce policies; AKS does not (`// ponytail:` on the policy).
9. **`kl-connect bench` opens one tunnel per local TCP connection**, each with its own 60 s single-use token, because the harness opens several HTTP and WebSocket connections and the gateway spends a token on connect.
10. **No delete route and no read-after-leaving in this cut.** The spec lists no `/v1` delete; deleting is `kubectl delete bench` until the folder's fate is designed (open questions 1 and 4). A person no longer in the team gets 404 from every `/v1/bench` route, including the tunnel token, so a departed member's read-only access (open question 1) is not granted yet.
11. **The tool server listens on the pod IP; the namespace is the fence.** Every session runs in the bench pod and only tool calls reach a workspace, so the bench must dial `kl ide serve`, today bound to `127.0.0.1:7788` and reached only through the ssh tunnel. The prelude passes `--bind 0.0.0.0:7788`; the flag already exists (`bins/kl/src/main.rs:59`). A person's bench and their workspaces in one team share `ws_namespace(owner, team)`, whose `default-deny` plus `allow-same-namespace` (`bins/agent/src/binding.rs:142`) admit exactly those pods to each other and nobody else's. `allow-bench-tools` (bench pods to workspace pods, TCP 7788) names the grant; it widens nothing today, and it keeps the bench's path if `allow-same-namespace` is ever narrowed. `intercept_ingress` admits the intercepting environment on every port. With the tool server on the pod IP, that would hand the environment an unauthenticated `exec`, so it is narrowed to the intercepted ports in the same commit. `allow-gateway-ssh` stays port 22 only. The ssh tunnel (`kl-connect ws ide`) still reaches loopback. AKS enforces no policy (the `// ponytail:` of decision 8); workspaces and benches run on the k3s regions.
12. **Ownership is checked by `/v1`, never by the pod.** The tool server keeps no auth code. `GET /v1/workspaces/{id}/tools?team=` answers `{"address": "{podIP}:7788"}` only to the caller who is `spec.owner`, only while Ready, and only for a workspace of the bench's own team when `team` is given. A workspace that is not Ready, or is in another team, is a 409 naming why. Everyone but the owner gets a 404, a team admin and a superadmin included. The bench's `workspace-tools` extension dials only an address it got there.

## Global Constraints

- Edit, build, test and commit only in `/work/src` in the dev pod. Never `cargo` on the laptop; in piped pod scripts spell it `c=$(printf 'car%s' go)`. Never a plain `cargo update`, never `cargo fmt`.
- `spec.owner` is the truth; labels are a view stamped by `/v1` and healed by the agent. Never authorize on a label.
- The agent writes status only; `deploy/k3s/agent-admission.yaml` covers `benches` in the same commit that gives the agent `patch` on them.
- No admin router, admin page, history reflector or audit view touches a Bench. Task 7 has a test that proves the admin router has no bench route; Task 2 proves the admin ServiceAccount cannot read one.
- Files stay under ~800 lines; module `//!` docs carry the design context; comments say why; keep and add `// ponytail:` markers.
- Commit subjects are imperative sentence case with no tool attribution. Commit after each task from `/work/src`. Push `origin` and `platform` once, at the end of the verified batch (end of Task 13), never mid-batch.
- Before every commit: `cargo clippy --workspace --all-targets -- -D warnings` clean. Confirm package names once with `grep -h '^name' crates/*/Cargo.toml bins/*/Cargo.toml` and substitute them in the commands below if they differ.

## Task list

1. `Bench` CRD, `bench_id`, generated `crds.yaml`
2. RBAC and admission policy for benches
3. The bench pod, its folder path, its ingress policy (`k8s/bench.rs`)
4. `ensure_bench_folder` on the agent
5. The bench reconciler, its claim, its dead-node release, its controller wiring
6. The `bench-session` tunnel token
7. `/v1/bench` routes, quota, ingress allow-list
8. Gateway: `/tunnel/{id}` resolves a bench to port 7789
9. `kl-connect bench`
10. The bench image (stub until harness-bench lands) and its CI job
11. The tool server on the pod IP: prelude, `allow-bench-tools`, intercept ingress narrowed to its ports
12. `GET /v1/workspaces/{id}/tools`: the owner's tool server address
13. SLO ids, catalogue, `deploy/slo.md`, probe stage; push the batch
14. Ship, pin, roll, apply, verify on the fleet

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
    // ponytail: stored because no team->region binding exists yet; /v1 fills it from the team once one does.
    pub region: String,
    pub image: String,
    #[serde(default)]
    pub model: String,
    pub desired_state: DesiredState,
    #[serde(default)]
    pub resources: PodResources,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_environment: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BenchStatus {
    pub phase: Phase,
    #[serde(default)]
    pub node_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

pub const FOLDER_READY: &str = "FolderReady";
pub const FOLDER_NOT_READY: &str = "FolderNotReady";
pub const FOLDER_LOCKED: &str = "FolderLocked";
/// The reader a Stopped bench keeps: enough to parse JSONL, nothing to run a model.
pub fn reader_resources() -> PodResources; // cpu request/limit 50m, memory request/limit 128Mi

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
        let v = serde_json::json!({"owner":"alice","team":"acme","region":"r1","image":"i","desiredState":"running"});
        let s: BenchSpec = serde_json::from_value(v).unwrap();
        assert_eq!(s.resources, PodResources::default());
        assert!(s.model.is_empty() && s.attached_environment.is_none());
        assert_eq!(reader_resources().memory_limit, "128Mi");
    }
}
```
- [ ] Run: `cargo test -p kloudlite-workspaces a_bench_is_one_name` → expected FAIL: `cannot find function bench_id` / `cannot find type BenchSpec`.
- [ ] Implement `bench.rs` as in Interfaces with a `//!` doc ("a person's bench in one team; no volume, no sshd; the folder is on the region share; see the spec"), and `bench_id` in `names.rs` using the hashing helper `builder_id`'s neighbours use (`hex_prefix`).
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
pub fn bench_pod(b: &crd::Bench, id: &str, pool: &str, runtime_class: Option<&str>, registry_host: &str) -> Result<Pod, String>;
/// Admits BENCH_PORT only from the gateway's pods.
pub fn bench_ingress_policy(namespace: &str, id: &str) -> NetworkPolicy;
```
Pod shape — start from `workspace_pod` (`k8s/workspace.rs:490`) and delete, never re-derive:
- Name `BENCH_POD`, namespace `crd::ws_namespace(owner, team)`, labels `KIND_LABEL=bench`, `OWNER_LABEL`, `TEAM_LABEL`, `WORKSPACE_LABEL=id` (the pod-watch mapper and the attach NetworkPolicy both key on it), an ownerReference to the Bench with `controller: true`.
- `nodeName` from `status.nodeName`, `runtimeClassName`, and the same hardened security context (uid/gid `SSH_UID`, no privilege escalation, read-only root, `drop: ALL`).
- Volumes: `home_volume(pool, owner)` at `HOME_DIR`; a hostPath `bench_folder(..)` with `type: Directory` at `BENCH_DIR` (never `DirectoryOrCreate`: a missing folder must fail, never become an empty local directory); the `user-key` Secret projected as the workspace does; the attach `resolv.conf` (`attach_file(pool, id)`, `type: File`) at `/etc/resolv.conf`; an `emptyDir` at `/tmp`.
- Not carried: the btrfs worktree, homecache, sshd config and host-key Secret, `authorized_keys`, the git seed init container, the `.cache` subPath mounts.
- Command: `["harness-bench"]` when Running, `["harness-bench", "--read-only"]` when Stopped. Env: `KL_OWNER`, `KL_TEAM`, `KL_BENCH=id`, `KL_MODEL=spec.model`, `KL_REGISTRY_HOST`, `NODE_NAME` from the downward API (the lock holder's name), `HOME=/home/kl`, `LANG=C.UTF-8`.
- Resources: `quantities(&spec.resources)` when Running, `quantities(&crd::reader_resources())` when Stopped.
- Readiness: `exec: ["harness-bench", "--ping"]`, period 5 s.
- `terminationMessagePolicy: File` (the default path `/dev/termination-log` carries the lock holder, Task 5).

- [ ] **Failing tests** in `k8s/tests/bench.rs` (a `fixture_bench(owner, team, state)` helper at the top builds a `crd::Bench` with `status.nodeName = "n1"`):
```rust
#[test]
fn a_bench_pod_mounts_only_its_own_folder_and_no_worktree() {
    let b = fixture_bench("alice", "acme", DesiredState::Running);
    let p = bench_pod(&b, "bench-1", "/wspool", None, "cr.example").unwrap();
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
fn a_stopped_bench_is_a_small_read_only_reader() {
    let p = bench_pod(&fixture_bench("alice", "acme", DesiredState::Stopped), "bench-1", "/wspool", None, "cr").unwrap();
    let c = &p.spec.unwrap().containers[0];
    assert_eq!(c.command.as_ref().unwrap().last().map(String::as_str), Some("--read-only"));
    assert_eq!(c.resources.as_ref().unwrap().limits.as_ref().unwrap()["memory"].0, "128Mi");
}

#[test]
fn a_folder_segment_that_escapes_is_refused_before_it_becomes_a_hostpath() {
    assert!(bench_folder("/wspool", "..", "alice").is_err());
    assert!(bench_folder("/wspool", "acme", "a/b").is_err());
    assert!(bench_folder("/wspool", "acme", ".").is_err());
    assert!(bench_pod(&fixture_bench("alice", "../x", DesiredState::Running), "b", "/wspool", None, "cr").is_err());
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
- [ ] Commit: `Make a private bench folder on the region share beside the homes`.

### Task 5: The bench reconciler, its claim, its dead-node release, its controller wiring

**Files:**
- Create: `bins/agent/src/controller/bench.rs` (under 400 lines), `bins/agent/tests/reconcile/bench.rs`
- Modify: `bins/agent/src/controller/mod.rs` (`mod bench; pub use bench::reconcile_bench;`), `bins/agent/src/claim.rs` (`claim_bench` after `claim_environment` near `:642`), `bins/agent/src/controller/run.rs` (a placed `Controller<Bench>` beside `workspaces` at `:269`; an unplaced claim controller beside `claim_ws` at `:399`), `bins/agent/src/peer/sweeps.rs` (bench release on an unplaceable node), `bins/agent/tests/reconcile/main.rs` (`mod bench;`), `deploy/k3s/agent-rbac.yaml` (a `networkpolicies` row naming `reconcile_bench` if the attach row does not already grant create/patch)

**Interfaces:**
```rust
pub async fn reconcile_bench(b: Arc<crd::Bench>, ctx: Arc<Ctx>) -> Result<Action, ReconcileErr>;
pub async fn claim_bench(b: &crd::Bench, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr>;
/// Pure: what the pod says about the bench. Split out so the arms are unit-testable.
pub(crate) fn bench_state(b: &crd::Bench, pod: Option<&Pod>) -> PodVerdict;
pub(crate) enum PodVerdict { Create, Replace, Starting, Ready, Locked(String) }
```
The reconcile, in order. Every status write goes through `bench_conditions(prev, c)`, which replaces by type and keeps `Placed` and `FolderReady` (the `replaced` helper in `controller/workspace/conditions.rs` already does this; reuse it).
1. `heal_labels` (the existing generic helper): owner, team, kind.
2. Namespace readiness: the same `NAMESPACE_READY` wait `apply_workspace` does (`controller/workspace/mod.rs:190-202`).
3. `ctx.homes_export` is `None` → phase `Creating`, `Ready=False/FolderNotReady` "this node has no shared-home mount (WS_HOMES_EXPORT)", requeue `TICK`.
4. `spawn_blocking(ensure_bench_folder)` under `super::timed("bench_folder", ..)`. `Err` → `Creating`, `FolderReady=False/FolderNotReady` with the error, requeue `TICK`. `Ok` → `FolderReady=True/Ready`.
5. Attach `resolv.conf` for `id` with the render the workspace path uses (the function writing `attach_file(pool, id)`), so `spec.attachedEnvironment` works. `/v1` writes the NetworkPolicy half (Task 7).
6. Apply `k8s::bench_ingress_policy` (server-side apply, field manager `kloudlite-agent`).
7. The `user-key` Secret must exist in the namespace (a GET). Missing → `Ready=False/KeysNotReady`, requeue `TICK`. `/v1` installs it (Task 7).
8. GET the pod, then `bench_state`: `Create` → create `k8s::bench_pod`; `Replace` (the pod's `--read-only` does not match `desiredState`; a container command is immutable) → delete, requeue 2 s; `Starting` → phase `Starting`; `Ready` → phase `Ready`, `Ready=True/Running` or `Ready=True/ReadOnly`; `Locked(holder)` (container last terminated with exit code 75) → `Ready=False/FolderLocked`, message "folder held by {holder}". Always record `status.podRef = "{ns}/bench"` once the pod exists.

`claim_bench` is `claim(b, ctx, "Bench", crd::Phase::Pending, |o| Parts { node_name: status node, storage: None, volume: None, region: &o.spec.region, owner: &o.spec.owner, want: want_of(&o.spec.resources) })`. With no storage and no volume, `decide` (`claim.rs:415`) reaches the capacity and placeability arms and nothing volume-bound.

Dead node: a bench holds no state on its node, so it is always releasable. In `peer/sweeps.rs`, next to the pass that calls `unplaceable(`, add a bench pass: for each Bench whose `status.nodeName` names an unplaceable node, clear `nodeName` and write `Placed=False/NodeDead` through `mark_parent_of::<crd::Bench>` (read its signature; it is generic over the kind). Any up node then claims it; the folder lock (`FolderLocked`) is what keeps a zombie writer on a partitioned node from double-writing.

- [ ] **Failing tests**, first the pure ones in `controller/bench.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pod_decides_create_replace_ready_and_locked() {
        let running = fixture_bench(DesiredState::Running);
        assert!(matches!(bench_state(&running, None), PodVerdict::Create));
        assert!(matches!(bench_state(&running, Some(&pod_with(&["harness-bench", "--read-only"], None, true))), PodVerdict::Replace));
        assert!(matches!(bench_state(&running, Some(&pod_with(&["harness-bench"], None, true))), PodVerdict::Ready));
        assert!(matches!(bench_state(&running, Some(&pod_with(&["harness-bench"], None, false))), PodVerdict::Starting));
        match bench_state(&running, Some(&pod_with(&["harness-bench"], Some((75, "node-b")), false))) {
            PodVerdict::Locked(h) => assert_eq!(h, "node-b"),
            _ => panic!("exit 75 is a held lock"),
        }
        let stopped = fixture_bench(DesiredState::Stopped);
        assert!(matches!(bench_state(&stopped, Some(&pod_with(&["harness-bench"], None, true))), PodVerdict::Replace));
    }
}
```
(`fixture_bench` and `pod_with(command, last_terminated: Option<(exit, message)>, ready)` are ten-line helpers in the same test module.)

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
async fn stopping_a_bench_replaces_its_pod_with_the_reader() {
    // desiredState Stopped; GET pod -> command ["harness-bench"].
    // assert: one DELETE pods/bench and no POST in this pass; second pass with GET pod -> 404 POSTs a pod ending "--read-only"
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
- [ ] Run: `cargo test -p kloudlite-agent-bin the_pod_decides` → FAIL: `cannot find function bench_state`. Run: `cargo test -p kloudlite-agent-bin --test reconcile bench` → FAIL: unresolved import `kloudlite_agent::controller::reconcile_bench`.
- [ ] Implement `controller/bench.rs`, `claim_bench`, the sweep pass, and the `run.rs` wiring: bench pods watched with `KIND_LABEL=bench`, mapped through `held(&bench_store, owned_by::<crd::Bench, _>(&p))`; the claim controller gated on `ctx.has_pool` like `claim_ws`; both runs wrapped in `observed("bench", ..)` and `observed("claim", ..)`.
- [ ] Run both commands → ok. `cargo test -p kloudlite-agent-bin` → no regressions; the one-kind-of-node merge noted two wall-clock flakes, so re-run a single failure once before calling it a regression.
- [ ] Clippy; commit: `Reconcile benches on the node: claim, folder, pod, and the reader when stopped`.

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

### Task 7: `/v1/bench` routes, quota, ingress allow-list

**Files:**
- Create: `crates/workspaces/src/api/bench.rs` (under 500 lines); tests where the `ssh_session` handler's tests live (grep `ssh_session` under `crates/workspaces/tests` and `crates/workspaces/src/api`; add `api_bench.rs` beside them with the same mock-kube harness)
- Modify: `crates/workspaces/src/api/mod.rs:198-231` (routes), `crates/workspaces/src/api/workspaces/mod.rs:440` (`gateway_url` to `pub(crate)`), `crates/workspaces/src/api/workspaces/attach.rs` (extract the NetworkPolicy and environment checks into a function taking `(id, namespace, region)`, called by both `attach_ws` and `attach_bench`), `crates/workspaces/src/quota.rs:135-175`, `crates/workspaces/src/model.rs` (`DEFAULT_BENCH_IMAGE`, `DEFAULT_BENCH_MODEL`), `deploy/pin.sh` (rewrite the bench image pin beside the workspace image), `deploy/kloudlite-web.yaml:246` (allow-list and its comment)

**Interfaces:**
```rust
#[derive(Deserialize)] pub(crate) struct TeamQuery { team: Option<String> }
#[derive(Deserialize)] pub(crate) struct NewBench { team: Option<String>, region: String, #[serde(default)] model: Option<String> }
#[derive(Deserialize)] pub(crate) struct AttachBody { environment: String }

pub(crate) async fn get_bench(..)     // GET  /v1/bench?team=           200 doc | 404
pub(crate) async fn create_bench(..)  // POST /v1/bench                 201 created | 200 existing, now Running
pub(crate) async fn start_bench(..)   // POST /v1/bench/start?team=     202
pub(crate) async fn stop_bench(..)    // POST /v1/bench/stop?team=      202
pub(crate) async fn bench_session(..) // POST /v1/bench/session?team=   201 {id, token, gateway, expires_at}
pub(crate) async fn attach_bench(..)  // POST /v1/bench/attach?team=    202
pub(crate) async fn detach_bench(..)  // POST /v1/bench/detach?team=    202
fn bench_doc(b: &crd::Bench) -> serde_json::Value; // {id, owner, team, region, model, desiredState, phase, nodeName, conditions}
/// caller, normalized team, and the caller's own bench — or the 404 every other case gets.
async fn my_bench(s: &ApiState, headers: &HeaderMap, team: Option<&str>) -> Result<(Caller, String, Option<crd::Bench>), Response>;
```
Rules, all in `my_bench` so no handler can forget one:
- `caller()`; team trimmed and lowercased; absent or empty → `caller.name` (decision 4).
- `may_allocate_for(s, &caller, &team)` (membership, never the superadmin claim), else 404 "no such team". This is the spec's `may_act(person, team)`.
- GET `crd::bench_id(&caller.name, &team)`; a found object with `spec.owner != caller.name` → 404. No superadmin arm anywhere in this file.

Handlers:
- `create_bench`: `check_region`. Existing → `set_desired::<crd::Bench>(Running)` and 200 with the doc; a different `region` in the body is ignored with `tracing::info!(.., "bench.region.ignored")`. New → `guard_alloc(&s, &caller.name, false, &bench_cost(&PodResources::default()))`; create with `labels(&caller.name, "bench")` plus `TEAM_LABEL`, `image: DEFAULT_BENCH_IMAGE`, `model` defaulting to `DEFAULT_BENCH_MODEL`; spawn the user-key install the way `create_ws` spawns `install_user_key_after_placed` (read its signature; if it is Workspace-bound, generalize its lookup to take the namespace rather than copying it).
- `start_bench`: `guard_alloc` for `spec.resources` minus the reader, then `set_desired(Running)`. `stop_bench`: `set_desired(Stopped)`.
- `bench_session`: phase must be `ready`, else 409 `{"error": "bench is {phase}"}`; `s.jwt.mint_bench_session(&caller.name, &id, &spec.region)`; `gateway = gateway_url(&spec.region, &id)`; response shape identical to `ssh_session` minus `host_key`.
- `attach_bench` / `detach_bench`: the extracted attach function with the bench's namespace and `WORKSPACE_LABEL=id`; patch `spec.attachedEnvironment`.

Quota: add `benches.list(&lp)` to the `try_join!` in `usage`; for each Bench with `spec.owner == owner`, add cpu and memory of `spec.resources` when Running and of `reader_resources()` when Stopped. `bench_cost` lives beside `workspace_cost`.

- [ ] **Failing tests** (`api_bench.rs`), each built with the neighbouring `ssh_session` test's router and route recorder and called with `tower::ServiceExt::oneshot`:
```rust
#[tokio::test]
async fn creating_a_bench_twice_is_one_object_and_starts_it() {
    // first POST -> 201 and one POST benches recorded; seed the store with that object;
    // second POST (desiredState Stopped seeded) -> 200, no second create, one PATCH desiredState Running
}

#[tokio::test]
async fn a_non_member_cannot_see_create_or_tunnel_to_a_teams_bench() {
    // directory: caller not in "acme"; GET, POST, POST /start, POST /session with team=acme -> 404 each; recorder has no benches write
}

#[tokio::test]
async fn a_superadmin_claim_does_not_open_someone_elses_bench() {
    // caller superadmin, not a member; seeded bench owned by bob in acme; GET and POST /session -> 404
}

#[tokio::test]
async fn a_bench_session_is_refused_until_the_bench_is_ready() {
    // phase Starting -> 409; phase Ready -> 201; token verifies with verify_bench_session and names the bench id and region
}

#[tokio::test]
async fn a_running_bench_counts_against_the_person_and_never_the_team() {
    // seed Bench{owner alice, team acme, Running, cpu_limit "1"}; quota::usage(alice).millicores += 1000; usage(acme) unchanged;
    // flip to Stopped: usage(alice) += 50
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
- [ ] Commit: `Serve /v1/bench to the person only: create, start, stop, attach and a tunnel token`.

### Task 8: Gateway resolves a bench to port 7789

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

### Task 9: `kl-connect bench`

**Files:**
- Create: `bins/kl-connect/src/bench.rs`
- Modify: `bins/kl-connect/src/main.rs:8-50` (`mod bench;`, `Cmd::Bench { #[arg(long)] team: Option<String>, #[arg(long, default_value_t = 0)] port: u16, #[arg(long)] start: bool }` and its dispatch), `bins/kl-connect/src/api.rs` (`create_bench`, `get_bench`, `bench_session`), `bins/kl-connect/src/proxy.rs:54-` (split `pump` into `connect(url, token)` and a generic `pump_io`; `proxy` calls both unchanged in behaviour)

**Interfaces:**
```rust
// bench.rs
pub async fn bench(team: Option<&str>, port: u16, start: bool) -> Result<(), String>;
// api.rs
#[derive(Deserialize)] pub struct BenchSession { pub id: String, pub token: String, pub gateway: String, pub expires_at: String }
pub async fn create_bench(cfg: &Config, team: Option<&str>, region: &str) -> Result<serde_json::Value, Error>;
pub async fn get_bench(cfg: &Config, team: Option<&str>) -> Result<serde_json::Value, Error>;
pub async fn bench_session(cfg: &Config, team: Option<&str>) -> Result<BenchSession, Error>;
// proxy.rs
pub(crate) async fn connect(url: &str, token: &str) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, String>;
pub(crate) async fn pump_io<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(ws: WebSocketStream<MaybeTlsStream<TcpStream>>, r: R, w: W) -> Result<(), String>;
```
Behaviour: with `--start`, `create_bench` (region = the config's default region, the one `ws` commands use) and poll `get_bench` until phase `ready` for up to 90 s, printing progress to stderr. Bind `127.0.0.1:{port}`. Print exactly one stdout line, `127.0.0.1:<port>`, and flush. For each accepted connection spawn a task: `bench_session`, `connect(gateway_url(&s.gateway), &s.token)`, `pump_io(ws, read_half, write_half)`. A 401 prints "your login has expired — run `kl-connect login`" and exits non-zero; any other per-connection error is logged to stderr and the listener keeps serving. The `//!` doc says the token never appears in output, as `proxy.rs`'s does.

- [ ] **Failing test** in `bench.rs`:
```rust
#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn each_local_connection_gets_its_own_tunnel_and_token() {
        // One local axum app serves POST /v1/bench/session (an AtomicUsize counter; returns gateway "wss://x/tunnel/bench-1")
        // and GET /tunnel/bench-1 (WebSocket echo of binary frames).
        // KL_CONFIG_DIR = tempdir with config.json pointing api at the app; KL_GATEWAY_OVERRIDE = "ws://127.0.0.1:{app port}".
        // Run bench_on(listener) — the testable core `bench` wraps — on a pre-bound 127.0.0.1:0 listener.
        // Open two TcpStreams; write b"a" / b"b"; read back b"a" / b"b".
        // assert counter == 2
    }
}
```
Extract `async fn bench_on(listener: TcpListener, cfg: Config, team: Option<String>)` so the test needs no stdout parsing; write the body in full.
- [ ] Run: `cargo test -p kl-connect each_local_connection` → FAIL.
- [ ] Implement. No new dependencies (musl build).
- [ ] Run → ok; `cargo test -p kl-connect` (the existing proxy tests prove the split kept behaviour); clippy.
- [ ] Commit: `Open a bench on a local port with kl-connect bench`.

### Task 10: The bench image and its CI job

**Files:**
- Create: `deploy/bench/Dockerfile`, `deploy/bench/harness-bench-stub.sh`
- Modify: `.github/workflows/image.yml` (a build-push step after the workspace image step at `:228-236`, same shape), `.dockerignore` (whitelist `deploy/bench/`; the name whitelist broke the workspace-caches merge)

**Interfaces:**
- Image: `node:22-bookworm-slim`; `npm i -g @mariozechner/pi-coding-agent@<the version in harness/package-lock.json>`; `kl` copied from the `kl-musl` artifact the workflow already downloads; `harness-bench` = `harness/bench/dist/harness-bench` when the other plan has produced it, else the stub (a build stage does `test -f` and copies one or the other); `util-linux` for `flock`, `socat`; user `1000:1000`; `WORKDIR /home/kl`; `ENTRYPOINT []` (the pod sets `command`).
- Stub contract (the same interface the real program honours):
  - `harness-bench [--read-only]`: `exec 9>/bench/.lock; flock -n 9 || { cat /bench/.lock.holder > /dev/termination-log; exit 75; }`; write `$NODE_NAME` to `/bench/.lock.holder`; serve `socat TCP-LISTEN:7789,fork,reuseaddr SYSTEM:'printf "HTTP/1.1 200 OK\r\ncontent-length: N\r\n\r\nok stub MODE"'` where MODE is `read-only` or `running`.
  - `harness-bench --ping`: `socat -u TCP:127.0.0.1:7789 - </dev/null | grep -q ok`.

- [ ] **Failing check** in the dev pod's build path (`deploy/dev-push.sh` or the builder gate): `docker buildx build -f deploy/bench/Dockerfile .` → fails, file not found.
- [ ] Write both files. Build. Then, with a scratch folder mounted at `/bench`: start one container detached with `harness-bench`; `docker exec` it with `harness-bench --ping` → exit 0; start a second container on the same folder → exits 75 and its termination file names the first one's `NODE_NAME`.
- [ ] Add the CI step (tags `ghcr.io/kloudlite/kloudlite-bench:latest` and `:${{ github.sha }}`); add the bench image to `deploy/pin.sh`'s package check (Task 7 added the rewrite).
- [ ] Commit: `Package the bench image with pi, kl and a stub harness-bench`.

### Task 11: The tool server on the pod IP, fenced by its namespace

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

### Task 12: `GET /v1/workspaces/{id}/tools`: the owner's tool server address

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

### Task 13: SLO ids, catalogue, `deploy/slo.md`, probe stage

**Files:**
- Create: `bins/slo/src/stages/bench.rs`
- Modify: `crates/workspaces/src/slo/catalogue.rs` (`CATALOGUE` rows; the stage→ids table after `STAGES` at `:101`), `deploy/slo.md` (rows in catalogue order), `bins/slo/src/stages/mod.rs:36-55` (`pub mod bench;` and the calls from the fast journey's stage 5, the hourly experience stage and the weekly stage), `deploy/k3s/quotas-slo.yaml` (one bench's cpu/memory added to each probe owner)

**Interfaces — ids:**

| id | suite | stage | target | SLI |
|---|---|---|---|---|
| `bench.create` | Fast | 5 · Workspace | avail 99.9 | `POST /v1/bench` answers, and a second POST names the same id |
| `bench.start.p95` | Fast | 5 · Workspace | p95 90 000 ms | A started bench reaches phase `ready` |
| `bench.tunnel` | Fast | 5 · Workspace | bound 20 000 ms | A bench token opens the tunnel and `/healthz` answers through it |
| `bench.stop.readable` | Fast | 5 · Workspace | bound 60 000 ms | A stopped bench answers `/healthz` as `read-only` through the tunnel |
| `bench.session.roundtrip` | Hourly | 14 · Experience | bound 60 000 ms | A session is created, a no-tools prompt answered, and read back from `/sessions/{id}/messages` |
| `bench.exchange.both_views` | Hourly | 14 · Experience | avail 99.9 | An exchange reads back by `?session=` and by `?workspace=` |
| `bench.two_clients` | Hourly | 14 · Experience | avail 99.9 | Two WebSockets on one session see the same events in the same order |
| `bench.survives.reschedule` | Weekly | 12 · Weekly | bound 180 000 ms | After the pod is deleted every session reopens and processes read `lost` |
| `bench.workspace.tool_roundtrip` | Hourly | 14 · Experience | bound 180 000 ms | A workspace session on the bench runs `exec echo` in a workspace through its tool server, and the turn lands under `/bench/workspaces/{ws}/` |

`bench.stop.readable` goes beyond the spec's list: "stopped stops the work, not the history" is otherwise unprobed. `bench.workspace.tool_roundtrip` holds the whole chain the spec's "What runs where" draws: `/v1`'s address, `allow-bench-tools`, the tool server on the pod IP and the thread file. The hourly and weekly ids need the real `harness-bench`; while `/healthz` answers `ok stub …` the stage SKIPS them with the reason "bench image is the stub". A skipped id must reach the run row as skipped, never as passed (the service-intercept merge found skipped ids reading as passed).

- [ ] **Failing test** in `catalogue.rs` (beside `the_catalogue_matches_deploy_slo_md`):
```rust
#[test]
fn every_bench_id_is_catalogued() {
    for id in ["bench.create", "bench.start.p95", "bench.tunnel", "bench.stop.readable",
               "bench.session.roundtrip", "bench.exchange.both_views", "bench.two_clients",
               "bench.survives.reschedule", "bench.workspace.tool_roundtrip"] {
        assert!(find(id).is_some(), "{id} missing from CATALOGUE");
    }
}
```
- [ ] Run: `cargo test -p kloudlite-workspaces every_bench_id` → FAIL.
- [ ] Add the catalogue rows and the matching `deploy/slo.md` rows; run both tests → ok.
- [ ] Implement `stages/bench.rs` with ceilings as constants at the top, each at least its target (the rule in `stages/workspace.rs`'s doc):
  - `fast(ctx)`: create twice (same id) → poll ready → `POST /v1/bench/session` → open the tunnel with the WebSocket client `stages/workspace.rs` uses for `gw.tunnel.p95` and write a raw `GET /healthz HTTP/1.1\r\nhost: bench\r\n\r\n`, expecting `ok` → stop → poll until a fresh tunnel answers `read-only` → start again. The object name is a hash, so the `run-{id}` teardown prefix does not apply: the probe owner's bench is long-lived, and the stage ends with a stop so only the reader stays up.
  - `hourly(ctx)`: skip-or-run the four session ids through a local forward built on `kl-connect`'s `bench_on` (or the `kl-connect` binary the probe image ships for `id.cli.flow`). `bench.workspace.tool_roundtrip`: create a workspace the way `ide_server` does (`bins/slo/src/stages/experience_ws.rs:350`) and wait for `ide.serve.up`'s healthz; `POST /workspaces/{ws}/session` through the forward; on `WS /sessions/w-{ws}/rpc` prompt "Run `echo bench-$(id -un)` with the bash tool."; pass on a `tool_execution_end` whose `toolName` is `bash` and whose result text contains `bench-kl` (the event, never the model's prose); then `GET /workspaces/{ws}/messages` has `total > 0`, and `kubectl exec` in the bench pod finds `/bench/workspaces/{ws}/thread.jsonl` non-empty. Drop the workspace at the end as `ide_server` does.
  - `weekly(ctx)`: delete the bench pod with the probe's kube client (reuse the drill's existing pod-delete grant in `slo-rbac.yaml`; add `benches` to its resource list if the grant is resource-scoped), then the reschedule checks, or skip with the stub reason.
- [ ] Run `cargo test -p kloudlite-slo` and `cargo test -p kloudlite-workspaces`; clippy.
- [ ] Commit: `Probe benches: create, start, tunnel, the stopped reader, the session journey and a workspace's tools`.
- [ ] **End of batch:** `cargo test` for the whole workspace and clippy green in the pod; then `git push origin master` and `git push platform master` as two separate steps.

### Task 14: Ship, pin, roll, apply, verify on the fleet

- [ ] Wait for `image.yml` on the pushed SHA: the test job green and `kloudlite-bench:<sha>` published.
- [ ] `deploy/pin.sh <sha>` (refuses a SHA with no package, the bench image included); commit `Pin every tier to <sha>`; push both remotes.
- [ ] `deploy/roll.sh` for AKS (api, gateway). On the k3s region, by hand per `deploy/k3s/README.md`, in this order: `crds.yaml`, `agent-rbac.yaml`, `agent-admission.yaml`, `api-rbac.yaml`, `gateway.yaml`, `slo-rbac.yaml`, `quotas-slo.yaml`; the agent DaemonSet then rolls on its repin. Do not edit `/work/src` while a ship runs.
- [ ] Direct checks with a minted token, not a wait for the hourly:
```sh
curl -sX POST "$API/v1/bench" -H "authorization: Bearer $T" -d '{"team":"kloudlite","region":"centralindia-k3s"}'
kubectl get benches -o wide                                          # Node set, Phase Ready
kubectl -n "$NS" get pod bench -o jsonpath='{.spec.volumes[*].hostPath.path}'   # includes /homes/.benches/kloudlite/<owner>
kl-connect bench --team kloudlite &                                  # prints 127.0.0.1:PORT
curl -s "127.0.0.1:$PORT/healthz"                                    # ok stub running
curl -sX POST "$API/v1/bench/stop?team=kloudlite" -H "authorization: Bearer $T"
sleep 30; curl -s "127.0.0.1:$PORT/healthz"                          # ok stub read-only
kubectl auth can-i list benches.kloudlite.io --as=system:serviceaccount:kloudlite:kloudlite-admin   # no
WS_IP=$(kubectl -n "$NS" get pod "$WS_POD" -o jsonpath='{.status.podIP}')                      # one of your workspaces in this team
curl -s "$API/v1/workspaces/$WS/tools?team=kloudlite" -H "authorization: Bearer $T"          # {"address":"$WS_IP:7788"}
curl -s "$API/v1/workspaces/$WS/tools" -H "authorization: Bearer $OTHER_MEMBER_T"             # 404
kubectl -n "$NS" exec bench -- node -e "fetch('http://$WS_IP:7788/healthz').then(r=>r.text()).then(console.log)"   # "ok":true
kubectl -n "$ENV_NS" exec "$INTERCEPTING_POD" -- sh -c "timeout 5 nc -z $WS_IP 7788; echo \$?"   # non-zero: an intercepting environment cannot reach the tool port
```
- [ ] Run the fast suite by hand (`deploy/dev/run-job.sh fast`): `bench.create`, `bench.start.p95`, `bench.tunnel`, `bench.stop.readable` pass. The hourly and weekly bench ids, `bench.workspace.tool_roundtrip` among them, show as skipped with "bench image is the stub" until harness-bench ships. `ide.serve.up` and `ide.exec` still pass: the tunnel's loopback path is unchanged.
- [ ] Call it shipped only after the fast run passed on the carrying SHA; record the SHA and the skipped ids on the status board.
