# Host-Mount Storage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Workspace, environment and home storage is mounted from the node's filesystem directly,
deleting the PersistentVolume/PersistentVolumeClaim layer and everything that existed to make
binding safe.

**Architecture:** Pod volumes become `hostPath` with an explicit `type`; placement moves from the
PV's `nodeAffinity` to a `kubernetes.io/hostname` `nodeSelector` on the pod. `ensure_storage`, the
bind gate and the storage class are removed.

**Tech Stack:** Rust, kube-rs, Kubernetes hostPath volumes, Pod Security Admission.

**Spec:** `docs/superpowers/specs/2026-08-30-hostpath-storage-design.md` — read it first. It carries
why the PV layer contributes nothing (static, agent-chosen paths, cosmetic capacity) and the two
costs this accepts.

## Global Constraints

- **`type` is mandatory on every `hostPath`.** `Directory` for directories, `File` for
  `resolv.conf`. An untyped `hostPath` CREATES a missing path as an empty directory, which is how a
  pod on the wrong node silently presents a wiped workspace. This is the one place the change is
  safer than what it replaces; never emit an untyped `hostPath`.
- **`nodeSelector`, never `nodeName`.** `nodeName` bypasses the scheduler and with it resource
  packing, taints and admission. The hostname selector is added ALONGSIDE the existing
  `rustic-git.io/{role}` selector and toleration — it does not replace them.
- **Host paths are agent-constructed only.** Every path is built from `PodContext.pool` and an id.
  No user-supplied value ever reaches a `hostPath.path`. `validate_mount` and its `valid_segment`
  check on `Mount::folder` stay EXACTLY as they are — that check is what keeps a user's folder name
  a single safe segment, and it is load-bearing regardless of how the mount is expressed.
- `automountServiceAccountToken: false` and `hardened()` stay on every pod. Only the namespace's
  PSA floor moves.
- The attach `resolv.conf` is still written IN PLACE and never renamed — a pod holds the inode.
- Comments explain WHY at the density of `bins/server/src/router/route.rs`. Delete comments that
  describe machinery being removed rather than leaving them to rot.
- Commit subjects imperative sentence case, no tool attribution.
- Prefix cargo runs with `CARGO_INCREMENTAL=0`, run them in the FOREGROUND with a long timeout.
- `CARGO_INCREMENTAL=0 cargo test --workspace --locked` and
  `CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets --locked -- -D warnings` green.
  `tests/routing.rs` has a known pre-existing flake under parallel load — re-run it alone.

---

### Task 1: Mount host paths instead of claims

**Files:**
- Modify: `crates/workspaces/src/k8s.rs` — the four volume builders (`claim_volume` `:829`,
  `nix_volume` `:729`, `home_volume` `:811`, `attach_volume` `:740`), their mounts in
  `workspace_pod` (`:1000-1015`), and the service pod's mounts (`:1097`)
- Test: `mod tests` in `crates/workspaces/src/k8s.rs`

**Interfaces:**
- Consumes: `PodContext.pool` and `PodContext.node_name` (both already threaded to every builder),
  `live_path(pool, id)`, `NIX_ROOT`, `attach_file(pool, ws_id)`, `packages::PROFILE_MOUNT`.
- Produces: the four builders take the arguments they need to compute a path. `claim_volume(id)`
  becomes `live_volume(pool, id)`; `nix_volume(id)` becomes `nix_volume()`; `home_volume()` becomes
  `home_volume(pool, owner_home_id)`; `attach_volume()` becomes `attach_volume(pool, ws_id)`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/workspaces/src/k8s.rs`:

```rust
    /// Storage is mounted from the node, not claimed. Every source carries an explicit `type`:
    /// an untyped hostPath creates a missing path as an empty directory, which is a wiped
    /// workspace rather than a failed mount.
    #[test]
    fn every_volume_is_a_typed_host_path() {
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx()).unwrap();
        let vols = p.spec.as_ref().unwrap().volumes.as_ref().unwrap();
        assert!(
            vols.iter().all(|v| v.persistent_volume_claim.is_none()),
            "no pod claims a PVC any more"
        );
        for v in vols.iter().filter(|v| v.host_path.is_some()) {
            let h = v.host_path.as_ref().unwrap();
            assert!(h.type_.is_some(), "hostPath {:?} must declare a type", v.name);
            assert!(h.path.starts_with('/'), "hostPath {:?} must be absolute", v.name);
        }
    }

    /// The four paths are the ones `ensure_storage` used to hand the PV builder.
    #[test]
    fn the_host_paths_are_the_ones_the_pv_layer_used() {
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx()).unwrap();
        let vols = p.spec.as_ref().unwrap().volumes.as_ref().unwrap();
        let path = |n: &str| {
            vols.iter().find(|v| v.name == n).unwrap_or_else(|| panic!("no {n} volume"))
                .host_path.as_ref().unwrap().path.clone()
        };
        assert_eq!(path("live"), live_path("/mnt/wspool", "ws-1"));
        assert_eq!(path("nix"), NIX_ROOT);
        assert_eq!(path("attach"), attach_file("/mnt/wspool", "ws-1"));
    }

    /// Placement is the pod's own now that no PV carries node affinity, and it is ADDED to the
    /// role selector rather than replacing it.
    #[test]
    fn the_pod_selects_its_node_by_hostname() {
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx()).unwrap();
        let s = p.spec.unwrap();
        let sel = s.node_selector.expect("a node selector");
        assert_eq!(sel.get("kubernetes.io/hostname").map(String::as_str), Some("session-0"));
        assert_eq!(sel.get("rustic-git.io/session").map(String::as_str), Some("true"));
        assert!(s.node_name.is_none(), "the scheduler still places the pod");
    }
```

Use the file's real spec/context fixtures — `ws_spec()` and `ctx()` are named as this plan needs
them; match whatever the neighbouring tests already call.

- [ ] **Step 2: Run them and watch them fail**

Run: `CARGO_INCREMENTAL=0 cargo test -p rustic-git-workspaces host_path`
Expected: FAIL — the volumes are still `persistentVolumeClaim` and there is no hostname selector.

- [ ] **Step 3: Rewrite the four volume builders**

```rust
/// A typed host directory. `Directory` rather than the default: an untyped `hostPath` CREATES a
/// missing path as an empty directory, so a pod that lands where its subvolume is not would start
/// with a blank home and no error. Typed, the kubelet refuses the mount and the pod says so.
fn host_dir(name: &str, path: String, read_only: bool) -> Volume {
    Volume {
        name: name.to_string(),
        host_path: Some(HostPathVolumeSource { path, type_: Some("Directory".into()) }),
        ..Default::default()
    }
}

/// The workspace's own subvolume.
fn live_volume(pool: &str, id: &str) -> Volume {
    host_dir("live", live_path(pool, id), false)
}

/// The store, read-only. Mounted at its root because the profile lives under it too; the
/// individual mounts below pick the two subdirectories the pod may see.
fn nix_volume() -> Volume {
    host_dir("nix", NIX_ROOT.to_string(), true)
}

/// The owner's persistent home. `home_id` is `crd::home_volume_name(owner)` — always via the
/// function, never formatted here.
fn home_volume(pool: &str, home_id: &str) -> Volume {
    host_dir("home", live_path(pool, home_id), false)
}

/// This workspace's rendered `resolv.conf`. A FILE, and mounted as one: the agent rewrites it in
/// place precisely because the pod holds the inode.
fn attach_volume(pool: &str, ws_id: &str) -> Volume {
    Volume {
        name: "attach".to_string(),
        host_path: Some(HostPathVolumeSource {
            path: attach_file(pool, ws_id),
            type_: Some("File".into()),
        }),
        ..Default::default()
    }
}
```

Add the `HostPathVolumeSource` import and drop `PersistentVolumeClaimVolumeSource`.

- [ ] **Step 4: Point the mounts at the deeper paths**

`subPath` existed only because one claim served several purposes; a host path names the
subdirectory directly. In `workspace_pod`, replace the nix and attach mounts:

```rust
                // The store and THIS workspace's profile only — `/nix` itself holds every other
                // workspace's profile and the daemon socket, so the pod never sees its root.
                VolumeMount { name: "nix".to_string(), mount_path: "/nix/store".to_string(), sub_path: Some("store".to_string()), read_only: Some(true), ..Default::default() },
                VolumeMount { name: "nix".to_string(), mount_path: crate::packages::PROFILE_MOUNT.to_string(), sub_path: Some(format!("var/rustic/profiles/{id}")), read_only: Some(true), ..Default::default() },
                // Mounting over `/etc/resolv.conf` is the only way to change a live pod's DNS —
                // `dnsConfig` is immutable once it is running. The volume IS the file now, so no
                // subPath: the agent rewrites it in place and the pod sees the change.
                VolumeMount {
                    name: "attach".into(),
                    mount_path: "/etc/resolv.conf".into(),
                    read_only: Some(true),
                    ..Default::default()
                },
```

The two nix mounts KEEP their `subPath` — the volume is `/nix` and they select within it, which is
the same shape as before. Only the attach mount loses one, because its volume is now the file.

The user-mount loop at `:1097` also keeps `sub_path: Some(format!("volumes/{}", m.folder))`: the
`live` volume is the subvolume root and the mount selects within it, exactly as today. Do not
inline `m.folder` into a host path — `valid_segment` guards it as a path SEGMENT, and moving it
into a `hostPath.path` would change what that check is protecting.

- [ ] **Step 5: Add the hostname selector**

In `placement`, which already sets the role selector and toleration, take the node and add it:

```rust
fn placement(spec: &mut PodSpec, role: &str, node: &str) {
    // Two selectors, two jobs: the role key says "session pods run on session nodes" (and a
    // single-node install may carry both role labels), the hostname pins this pod to the node
    // holding its subvolume. That pin used to come from the PV's `nodeAffinity`; with the volumes
    // mounted from the host there is no PV to carry it, and an unpinned pod would mount an empty
    // directory on the wrong node.
    spec.node_selector = Some(BTreeMap::from([
        (format!("rustic-git.io/{role}"), "true".to_string()),
        ("kubernetes.io/hostname".to_string(), node.to_string()),
    ]));
```

Leave the toleration and `automount_service_account_token` exactly as they are. Update both
callers to pass `ctx.node_name`, and delete the now-wrong `let _ = ctx.node_name;` line at `:959`
and the `PodContext.pool` doc comment claiming "Only the PV needs it".

- [ ] **Step 6: Run the tests**

Run: `CARGO_INCREMENTAL=0 cargo test -p rustic-git-workspaces`
Expected: PASS. Existing assertions that a pod carries NO `host_path` (`:1652`, `:1659`, `:1905`,
`:2099`) now assert the opposite of the design — invert them to assert no `persistent_volume_claim`.
That is the one case in this plan where changing an assertion is correct, because the rule it
encoded is the rule being replaced. Any OTHER assertion that has to change: stop and report it.

- [ ] **Step 7: Commit**

```bash
git add crates/workspaces/src/k8s.rs
git commit -m "Mount workspace storage from the node instead of claiming it"
```

---

### Task 2: Open the namespace security floor

**Files:**
- Modify: `crates/workspaces/src/k8s.rs` — the namespace label builder (`:130`)
- Test: the same `mod tests` (`:1876`)

- [ ] **Step 1: Change the label and its test**

`baseline` forbids `hostPath`, so the namespace floor has to move. Set enforce to `privileged` and
LEAVE `audit` and `warn` at `restricted`:

```rust
    // `privileged` because these pods mount host paths: their storage IS the node's filesystem,
    // and `baseline` forbids `hostPath` outright. This is the price of removing the PV layer, and
    // it is namespace-wide — the guarantee that nothing here names an arbitrary host path is now
    // our code's, not the API server's. `audit` and `warn` stay at `restricted` so the gap keeps
    // showing up in audit rather than going quiet.
    l.insert("pod-security.kubernetes.io/enforce".into(), "privileged".into());
```

Update the assertion at `:1876` to expect `privileged`, and leave the `audit` assertion alone.

- [ ] **Step 2: Verify existing namespaces get re-stamped**

The agent must APPLY these labels on reconcile, not only at create, or namespaces made before this
change keep `baseline` and every pod in them is rejected. Confirm the namespace path is an apply
(the same way `heal_labels` re-stamps CRD labels). If it is create-only, make it an apply and say
so in the report — this is the difference between a clean roll and every existing workspace failing
to start.

- [ ] **Step 3: Test and commit**

```bash
CARGO_INCREMENTAL=0 cargo test -p rustic-git-workspaces
git add crates/workspaces/src/k8s.rs
git commit -m "Drop the workspace namespace floor to privileged for host mounts"
```

---

### Task 3: Delete the storage layer

**Files:**
- Modify: `bins/agent/src/controller.rs` — `ensure_storage` (`:1689`), its three call sites
  (`:2073`, `:2085`, `:2558`), the bind gate (`:2229`), `first_unbound_claim` (`:2303`),
  `BIND_POLL` (`:47`)
- Modify: `bins/agent/src/binding.rs` — the two call sites (`:122`, `:129`)
- Modify: `crates/workspaces/src/k8s.rs` — `local_pv`, `claim`, `STORAGE_CLASS`, `claim_name`,
  `nix_claim_name`, `nix_pv_name`, `pv_name`, `home_pv_name`, `attach_pv_name`, `HOME_CLAIM`,
  `ATTACH_CLAIM`
- Test: `bins/agent/tests/reconcile.rs`

- [ ] **Step 1: Write the failing test**

```rust
/// Storage is the node's filesystem now: a reconcile writes no PersistentVolume and no claim.
#[tokio::test]
async fn a_workspace_reconcile_writes_no_storage_objects() {
    let (ctx, rec) = ctx_ready();
    let w: crd::Workspace = serde_json::from_value(ws_json_ready()).unwrap();
    rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert!(
        rec.calls().iter().all(|c| !c.1.contains("persistentvolume")),
        "no PV or PVC traffic at all: {:?}", rec.calls()
    );
    assert!(rec.calls().iter().any(|c| c.0 == "POST" && c.1.contains("/pods")), "the pod is created");
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `CARGO_INCREMENTAL=0 cargo test -p rustic-git-agent-bin no_storage_objects`
Expected: FAIL — `ensure_storage` still writes both halves.

- [ ] **Step 3: Delete `ensure_storage` and its five call sites**

Remove the function and every call. The workspace and environment reconciles simply stop calling
it; `binding.rs`'s `ensure_home_claims` loop over namespaces has no remaining body, so remove the
loop and any argument that only fed it. Keep everything the loop was NOT doing — the home Volume
authoring above it is unrelated and stays.

- [ ] **Step 4: Delete the bind gate**

Remove the `if w.spec.desired_state == DesiredState::Running && pods.get_opt(&id).await?.is_none()`
gate, `first_unbound_claim`, `BIND_POLL`, and the `WaitingForStorage` condition it wrote. There is
nothing to wait for: a `hostPath` needs no binding, and a missing directory is a mount failure the
pod reports rather than a race the controller has to prevent.

Do NOT remove the home-readiness gate (`HomeNotReady`) — that one is about the home Volume being
materialized, which is still real and still required.

- [ ] **Step 5: Delete the builders**

From `k8s.rs`: `local_pv`, `claim`, `STORAGE_CLASS`, `pv_name`, `claim_name`, `nix_pv_name`,
`nix_claim_name`, `home_pv_name`, `attach_pv_name`, `HOME_CLAIM`, `ATTACH_CLAIM`, and their tests.
Keep `live_path`, `attach_root`, `attach_dir`, `attach_file`, `NIX_ROOT` — the pods use them now.

Let the compiler find the rest; delete unused imports rather than leaving them.

- [ ] **Step 6: Full suite and clippy**

Run: `CARGO_INCREMENTAL=0 cargo test --workspace --locked`
Then: `CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 7: Commit**

```bash
git add bins/agent/src/controller.rs bins/agent/src/binding.rs crates/workspaces/src/k8s.rs bins/agent/tests/reconcile.rs
git commit -m "Delete the PersistentVolume layer and its bind gate"
```

---

### Task 4: Drop the storage permissions and the class

**Files:**
- Modify: `deploy/k3s/agent-rbac.yaml` — the header table IS the role; both must change together
- Delete: the `rustic-git-local` StorageClass manifest (find it under `deploy/`)

- [ ] **Step 1: Remove the rules**

Delete the `persistentvolumes` and `persistentvolumeclaims` rules and their rows in the header
table. Nothing in the agent touches either resource after Task 3.

- [ ] **Step 2: Delete the StorageClass**

`no-provisioner` and nothing binds through it any more. Remove the manifest and any reference to it
in `deploy/k3s/README.md`.

- [ ] **Step 3: Note the cutover**

No migration code and no ordered sweep: the operator deletes the old objects and lets the
controller rebuild. Add to `deploy/k3s/README.md`:

```markdown
### Cutover off PersistentVolumes

Pod volumes cannot be patched, so pods built against PVCs are deleted and recreated in the new
shape. After rolling the agent:

    kubectl delete pods -A -l rustic-git.io/kind=workspace
    kubectl delete pods -A -l rustic-git.io/kind=environment
    kubectl delete pvc -A -l rustic-git.io/owner
    kubectl delete pv -l rustic-git.io/owner

Each running workspace restarts once. Nothing on disk is touched: the subvolumes the PVs pointed at
are the same ones the pods now mount directly.
```

- [ ] **Step 4: Commit**

```bash
git add deploy/
git commit -m "Remove the storage class and the agent's volume permissions"
```

---

## Self-review

**Spec coverage.** Host paths with explicit types (Task 1), placement on the pod (Task 1), subPath
collapse (Task 1 Step 4 — narrower than the spec implied: only the attach mount loses one, the nix
and user mounts keep theirs because their volume is a root they select within), PSA floor (Task 2),
the deletion list (Tasks 3 and 4), migration (Task 4 Step 3, as a runbook rather than code).

**Deviation from the spec, deliberate.** The spec described the orphan sweep as keep-biased code in
the janitor. There is no sweep at all: the cutover is delete-and-recreate by hand. Sweeping in code
would require `list`/`delete` on the two resources Task 4 removes from the ClusterRole, and would
be dead the moment it ran once.

**Not covered on purpose.** The post-deploy measurement (a clone with no binding term, three
repeats; a wrong-node pod failing its mount rather than starting empty) is a deploy verification,
not a task.

**Type consistency.** `live_volume(pool, id)`, `nix_volume()`, `home_volume(pool, home_id)`,
`attach_volume(pool, ws_id)`, `placement(spec, role, node)`. All arguments come from `PodContext`,
which already carries `pool` and `node_name` to every builder.

**Known soft spot.** Task 2 Step 2 is a check, not an edit: if the namespace path turns out to be
create-only, existing namespaces keep `baseline` and every pod in them is rejected after the roll.
The implementer is told to make it an apply and report it. This is the single most likely way this
change breaks a live cluster, which is why it is its own step rather than a line in Task 1.

---

### Task 5: Put the pod-level fence back as admission policy

**Files:**
- Create: `deploy/k3s/workspace-admission.yaml` — a `ValidatingAdmissionPolicy` + binding
- Modify: `deploy/k3s/README.md` — the file table, the fresh-cluster apply line, and the upgrade section
- Modify: `crates/workspaces/src/k8s.rs` — the `namespace()` and `hardened()` comments, which currently say the namespace enforces nothing

**Why this exists.** Task 2 dropped the namespace floor from `baseline` to `privileged` because
`baseline` forbids `hostPath`. But `baseline` was also the only thing refusing `hostNetwork`,
`hostPID`, `hostIPC`, privileged containers and dangerous capabilities — and `hostNetwork` in
particular puts a pod on the node's network, where NetworkPolicy does not reach, which is what every
isolation guarantee in this system rests on. Nothing exploits that today: users never author a Pod,
only the agent's ServiceAccount may create pods in these namespaces, and every container gets
`hardened()`. This restores the property as an enforced fence rather than a property of our code, so
a future bug in the pod builders is refused rather than shipped.

`deploy/k3s/agent-admission.yaml` is the model for style and comment density — read it first. It
constrains the agent's CRD writes and matches pods **not at all**, which is why this is a new file
rather than a rule added there.

**Interfaces:**
- Consumes: the `rustic-git.io/kind` label on namespaces (`workspace` / `environment`), written by
  `k8s::namespace`.
- Produces: no Rust API. One manifest, applied alongside `agent-rbac.yaml`.

- [ ] **Step 1: Write the policy**

Constrain pods in workspace and environment namespaces. Refuse:

- `spec.hostNetwork`, `spec.hostPID`, `spec.hostIPC` (set, or true)
- any container, init container or ephemeral container with
  `securityContext.privileged` or `securityContext.allowPrivilegeEscalation` true
- any `hostPath` volume whose `path` is not under an allowed prefix

The last one is the important half and the reason this is worth doing: it restores "the API server
refuses an arbitrary host path" rather than "our code does not construct one". Allowed prefixes are
the btrfs pool root and the Nix store — on this cluster `/wspool-prod` and `/nix`. Require a `/`
after the prefix so `/wspool-prod-evil` does not pass a naive `startsWith`, and refuse any path
containing `..`.

Bind with `validationActions: ["Deny"]` and a `namespaceSelector` on
`rustic-git.io/kind in (workspace, environment)`.

Two things this must NOT catch, and a test of whether the selector is right: the agent's own
DaemonSet (privileged, hostPath, in `kube-system`) and the gateway. Both live outside the selected
namespaces. Verify by reading their manifests, and say in your report which namespaces each is in.

Set `failurePolicy: Fail`, matching `agent-admission.yaml`.

- [ ] **Step 2: Note the honest limit of the namespace selector**

If a namespace ever lacked the `rustic-git.io/kind` label, the policy would not apply to it. Write
that down in the file's header comment rather than leaving it implicit. It is acceptable because the
same code path (`k8s::namespace`, server-side applied on every binding reconcile) writes both that
label and the PSA level — a namespace missing the label is a namespace that never got `privileged`
either, so it refuses hostPath by PSA instead. Say exactly that; do not claim the selector is a
security boundary on its own. This repo's rule is that labels are views, never authorization — the
distinction here is that the label selects SCOPE, and the fence is the policy.

- [ ] **Step 3: Correct the two comments Task 2 left overstated**

`crates/workspaces/src/k8s.rs`: `namespace()`'s comment says the floor moved to `privileged` and
that the guarantee "is now our code's, not the API server's". With this task that is no longer true
— point at `workspace-admission.yaml`. `hardened()`'s comment likewise says it is the only thing
constraining the pod. Both should now say what actually enforces what: PSA admits the pod,
`workspace-admission.yaml` refuses the dangerous fields, `hardened()` sets the container's own
context.

- [ ] **Step 4: Document it**

Add the file to `deploy/k3s/README.md`'s table, to the fresh-cluster apply command, and to step 2
of the upgrade section (it applies with `agent-rbac.yaml`, same as `agent-admission.yaml`).

**Ordering matters and is the opposite of the RBAC's.** This policy refuses `hostNetwork` and
constrains `hostPath` — it does not require the new agent. It can be applied BEFORE the rollout and
is safest there, since it is a deny-only fence and old agents create no pod that violates it.
Say so explicitly, because a reader who has just internalised "RBAC goes last" will otherwise assume
the same for this.

- [ ] **Step 5: Verify**

There is no cluster here and no Rust to run for the manifest itself. Verify by:
- `python3 -c "import yaml,sys; list(yaml.safe_load_all(open('deploy/k3s/workspace-admission.yaml')))"`
- checking each CEL expression by hand against `k8s::workspace_pod`'s real output — every mount it
  emits must PASS. Walk the four volumes (`live`, `nix`, `home`, `attach`) and both nix subPath
  mounts explicitly in your report. A policy that refuses our own pods is worse than no policy.
- `CARGO_INCREMENTAL=0 cargo test -p rustic-git-workspaces --locked` for the comment edits.

- [ ] **Step 6: Commit**

```bash
git add deploy/k3s/workspace-admission.yaml deploy/k3s/README.md crates/workspaces/src/k8s.rs
git commit -m "Refuse hostNetwork and stray host paths by admission policy"
```
