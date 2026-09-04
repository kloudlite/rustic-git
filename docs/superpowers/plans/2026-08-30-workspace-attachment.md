# Workspace Attachment Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A workspace can be attached to one environment and reach that environment's services by
their bare names, with attach and detach taking effect on a running workspace without a restart.

**Architecture:** `WorkspaceSpec.attached_environment` is the desired state, written only by `/v1`.
The agent renders `/etc/resolv.conf` into an agent-owned host directory that each workspace pod
mounts read-only through a per-namespace `local` PV, and writes two NetworkPolicies to open the
path. Attach and detach are an in-place file write plus two namespaced objects, so nothing restarts.

**Tech Stack:** Rust, kube-rs, Kubernetes (`local` PersistentVolumes, NetworkPolicy, CoreDNS),
btrfs pool on the node.

**Spec:** `docs/superpowers/specs/2026-08-30-workspace-attachment-design.md` — read it first; it
records the seven cluster facts this design rests on and why each one forces a design choice.

## Global Constraints

- One environment per workspace. `attached_environment` is `Option<String>`, never a list.
- **The resolv.conf write is in place (truncate + write). NEVER a rename.** A rename leaves running
  pods reading the stale inode and attachment silently stops working. Verified on the cluster.
- **The file must exist before the pod is created.** A `subPath` whose target is missing is created
  as a directory, which breaks `/etc/resolv.conf` entirely.
- `hostPath` is refused in workspace namespaces (`pod-security.kubernetes.io/enforce: baseline`).
  Host paths reach pods only as `local` PVs.
- One PV + PVC per NAMESPACE (`attach-{ns}` / `attach`), not per workspace — a local PV binds to
  one claim, but one claim serves every pod in the namespace, exactly like `HOME_CLAIM`.
- Only `/v1` writes spec. The agent writes status only; its admission policy forbids the rest.
- Authorization reads `spec.owner` via `may_act_on`, never a label.
- Attachment requires `ws.spec.region == env.spec.region`; a different region is a different cluster.
- Comments explain WHY at the density of `bins/server/src/router/route.rs`. Commit subjects are
  imperative sentence case with no tool attribution.
- `cargo test --workspace --locked` and `cargo clippy --workspace --all-targets --locked --
  -D warnings` are green at the end of every task.

---

### Task 1: The spec field and the regenerated CRDs

**Files:**
- Modify: `crates/workspaces/src/crd.rs` (`WorkspaceSpec`, around line 297)
- Modify: `deploy/k3s/crds.yaml` (regenerated, never hand-edited)
- Test: `crates/workspaces/tests/crd_yaml.rs`

**Interfaces:**
- Produces: `crd::WorkspaceSpec.attached_environment: Option<String>`, serialized as
  `attachedEnvironment` (the struct already carries `#[serde(rename_all = "camelCase")]`).

- [ ] **Step 1: Write the failing test**

Add to `crates/workspaces/tests/crd_yaml.rs`:

```rust
/// The field is optional in the schema: a Workspace written before attachment existed must still
/// validate, and `/v1` creates workspaces without it.
#[test]
fn the_attached_environment_is_an_optional_string() {
    let crd = crd::Workspace::crd();
    let schema = crd.spec.versions[0].schema.as_ref().unwrap().open_api_v3_schema.as_ref().unwrap();
    let props = schema.properties.as_ref().unwrap()["spec"].properties.as_ref().unwrap();
    let field = props.get("attachedEnvironment").expect("attachedEnvironment in the schema");
    assert_eq!(field.type_.as_deref(), Some("string"));
    let required = schema.properties.as_ref().unwrap()["spec"].required.clone().unwrap_or_default();
    assert!(!required.contains(&"attachedEnvironment".to_string()), "must not be required");
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p kloudlite-git-workspaces --test crd_yaml the_attached_environment -- --nocapture`
Expected: FAIL — panics on `expect("attachedEnvironment in the schema")`.

- [ ] **Step 3: Add the field**

In `crates/workspaces/src/crd.rs`, inside `WorkspaceSpec`:

```rust
    /// The environment whose services this workspace resolves by bare name, or `None`.
    ///
    /// One, not a list: bare-name resolution has to be unambiguous, and two attached environments
    /// both exposing `db` would let search-domain order silently pick the winner.
    ///
    /// Written only by `/v1` — the agent's admission policy forbids it writing spec, and a stale
    /// id here is not an error: the reconciler treats a missing or wrong-region environment as
    /// unattached rather than leaving a grant behind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_environment: Option<String>,
```

- [ ] **Step 4: Run it and watch it pass**

Run: `cargo test -p kloudlite-git-workspaces --test crd_yaml the_attached_environment`
Expected: PASS.

- [ ] **Step 5: Regenerate the CRD manifests**

The `crd_yaml` test is its own generator and fails loudly on drift.

Run: `CRD_REGEN=1 cargo test -p kloudlite-git-workspaces --test crd_yaml`
Then: `cargo test -p kloudlite-git-workspaces --test crd_yaml` (must pass with no `CRD_REGEN`)
Then: `git diff --stat deploy/k3s/crds.yaml` — expect the file to have changed.

- [ ] **Step 6: Build the rest of the tree**

Every literal `WorkspaceSpec { .. }` in tests and in `crates/workspaces/src/api.rs` needs the new
field. Run `cargo check --workspace --all-targets` and add `attached_environment: None` at each
site the compiler names.

- [ ] **Step 7: Commit**

```bash
git add crates/workspaces/src/crd.rs crates/workspaces/tests/crd_yaml.rs deploy/k3s/crds.yaml
git add -u
git commit -m "Add the attached environment to a workspace's spec"
```

---

### Task 2: The resolv.conf renderer

**Files:**
- Modify: `crates/workspaces/src/k8s.rs` (new function plus its unit tests)

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces: `k8s::resolv_conf(template: &str, ws_ns: &str, env_ns: Option<&str>) -> String`.

A pure string function so it can be unit-tested without a cluster. The caller supplies the
template — in production the agent's own `/etc/resolv.conf`, which kubelet generated.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` in `crates/workspaces/src/k8s.rs`:

```rust
    const AGENT_RESOLV: &str = "search kube-system.svc.cluster.local svc.cluster.local cluster.local node.example.net\nnameserver 10.43.0.10\noptions ndots:5\n";

    /// Unattached: the workspace's own namespace leads, and everything the agent's file said about
    /// nameserver, ndots and the node's suffix is carried through untouched.
    #[test]
    fn an_unattached_resolv_conf_is_what_kubelet_would_have_written() {
        let got = resolv_conf(AGENT_RESOLV, "ws-acme", None);
        assert_eq!(
            got,
            "search ws-acme.svc.cluster.local svc.cluster.local cluster.local node.example.net\nnameserver 10.43.0.10\noptions ndots:5\n"
        );
    }

    /// Attached: the environment's namespace goes FIRST, so a name the environment defines wins
    /// over one in the workspace's own namespace.
    #[test]
    fn an_attached_resolv_conf_searches_the_environment_first() {
        let got = resolv_conf(AGENT_RESOLV, "ws-acme", Some("env-abc"));
        assert_eq!(
            got.lines().next().unwrap(),
            "search env-abc.svc.cluster.local ws-acme.svc.cluster.local svc.cluster.local cluster.local node.example.net"
        );
        assert!(got.contains("nameserver 10.43.0.10"), "the nameserver is inherited");
        assert!(got.ends_with('\n'), "resolv.conf is line-oriented; the last line must be terminated");
    }

    /// A template with no search line at all still yields a usable file rather than a malformed one.
    #[test]
    fn a_template_without_a_search_line_gains_one() {
        let got = resolv_conf("nameserver 10.43.0.10\n", "ws-acme", Some("env-abc"));
        assert_eq!(
            got,
            "search env-abc.svc.cluster.local ws-acme.svc.cluster.local svc.cluster.local cluster.local\nnameserver 10.43.0.10\n"
        );
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p kloudlite-git-workspaces resolv_conf`
Expected: FAIL — `cannot find function 'resolv_conf'`.

- [ ] **Step 3: Implement it**

Add to `crates/workspaces/src/k8s.rs`:

```rust
/// The `/etc/resolv.conf` a workspace pod gets, rendered from the AGENT's own file.
///
/// Templated rather than synthesised: the agent is not `hostNetwork`, so kubelet wrote its file
/// with the cluster nameserver, `options ndots:5` and the node's DNS suffix already in it. Copying
/// those means they can never drift from what the cluster actually uses; only the search line —
/// the one thing that is per-pod — is replaced.
///
/// The environment's namespace goes first so a service it defines wins over a same-named service
/// in the workspace's own namespace.
pub fn resolv_conf(template: &str, ws_ns: &str, env_ns: Option<&str>) -> String {
    let mut search = String::from("search ");
    if let Some(env) = env_ns {
        search.push_str(&format!("{env}.svc.cluster.local "));
    }
    search.push_str(&format!("{ws_ns}.svc.cluster.local svc.cluster.local cluster.local"));
    // Whatever the node appends after the cluster domains (a cloud's internal zone) is carried
    // over verbatim: it is how a pod resolves node-local names and we have no business guessing it.
    if let Some(tail) = template
        .lines()
        .find(|l| l.starts_with("search "))
        .and_then(|l| l.split_once(" cluster.local"))
        .map(|(_, rest)| rest.trim_end())
        .filter(|rest| !rest.is_empty())
    {
        search.push_str(tail);
    }
    let rest: Vec<&str> = template.lines().filter(|l| !l.starts_with("search ")).collect();
    let mut out = search;
    for line in rest {
        out.push('\n');
        out.push_str(line);
    }
    out.push('\n');
    out
}
```

- [ ] **Step 4: Run them and watch them pass**

Run: `cargo test -p kloudlite-git-workspaces resolv_conf`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/k8s.rs
git commit -m "Render a workspace's resolv.conf from the agent's own"
```

---

### Task 3: The attach mount — names, paths and the pod volume

**Files:**
- Modify: `crates/workspaces/src/k8s.rs` (names, host paths, `workspace_pod`'s volume and mount)

**Interfaces:**
- Consumes: `k8s::PodContext` (existing).
- Produces:
  - `k8s::ATTACH_CLAIM: &str = "attach"`
  - `k8s::attach_pv_name(ns: &str) -> String` → `attach-{ns}`
  - `k8s::attach_root(pool: &str) -> String` → `{pool}/attach`
  - `k8s::attach_dir(pool: &str, ws_id: &str) -> String` → `{pool}/attach/{ws_id}`
  - `k8s::attach_file(pool: &str, ws_id: &str) -> String` → `{pool}/attach/{ws_id}/resolv.conf`
  - `workspace_pod` mounts the claim at `/etc/resolv.conf` with `subPath: {ws_id}/resolv.conf`.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` in `crates/workspaces/src/k8s.rs`:

```rust
    /// Per NAMESPACE, like the home claim: a local PV binds to one claim, but one claim serves
    /// every pod in the namespace. The per-workspace part is the subPath, not the object.
    #[test]
    fn the_attach_claim_is_one_per_namespace() {
        assert_eq!(attach_pv_name("ws-acme"), "attach-ws-acme");
        assert_eq!(ATTACH_CLAIM, "attach");
        assert_eq!(attach_root("/pool"), "/pool/attach");
        assert_eq!(attach_file("/pool", "ws-1"), "/pool/attach/ws-1/resolv.conf");
    }

    /// The mount is what makes attachment live: the agent rewrites the host file and the running
    /// pod sees it. Read-only so the person in the workspace cannot point their own DNS elsewhere.
    #[test]
    fn a_workspace_pod_mounts_its_own_resolv_conf() {
        let spec = ws_spec();
        let pod = workspace_pod(&spec, "ws-1", &ctx(), None);
        let podspec = pod.spec.unwrap();
        let vol = podspec.volumes.unwrap().into_iter().find(|v| v.name == "attach").expect("attach volume");
        assert_eq!(vol.persistent_volume_claim.unwrap().claim_name, ATTACH_CLAIM);
        let mount = podspec.containers[0]
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|m| m.mount_path == "/etc/resolv.conf")
            .expect("resolv.conf mount");
        assert_eq!(mount.sub_path.as_deref(), Some("ws-1/resolv.conf"));
        assert_eq!(mount.read_only, Some(true));
    }
```

`ws_spec()` and `ctx()` are the existing helpers in that test module; if `ws_spec()` does not exist
under that name, use whatever the neighbouring `workspace_pod` tests already build their spec with.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p kloudlite-git-workspaces attach_claim_is_one_per_namespace mounts_its_own_resolv`
Expected: FAIL — `cannot find function 'attach_pv_name'`.

- [ ] **Step 3: Implement the names**

Add next to `home_pv_name` in `crates/workspaces/src/k8s.rs`:

```rust
/// The claim every workspace pod in a namespace mounts to get its `/etc/resolv.conf`.
///
/// One claim per namespace, like `HOME_CLAIM` and unlike the Nix store's per-workspace pair: a
/// local PV binds to exactly one claim, but a claim may be mounted by every pod in its namespace,
/// and the per-workspace part is the `subPath`. One writer (the agent) and read-only consumers is
/// what makes sharing it safe — two writers over one host path would be a silent data race.
pub const ATTACH_CLAIM: &str = "attach";

/// The local PV behind `ATTACH_CLAIM` in `ns`. Cluster-scoped, so the namespace is in the name.
pub fn attach_pv_name(ns: &str) -> String {
    format!("attach-{ns}")
}

/// The agent-owned directory holding one rendered `resolv.conf` per workspace. Outside any user
/// volume on purpose: it is platform state, so it is never in a snapshot and never pushed.
pub fn attach_root(pool: &str) -> String {
    format!("{pool}/attach")
}

pub fn attach_dir(pool: &str, ws_id: &str) -> String {
    format!("{}/{ws_id}", attach_root(pool))
}

pub fn attach_file(pool: &str, ws_id: &str) -> String {
    format!("{}/resolv.conf", attach_dir(pool, ws_id))
}
```

- [ ] **Step 4: Mount it in the pod**

In `workspace_pod`, alongside the existing `nix` volume, add the volume:

```rust
    let attach_volume = Volume {
        name: "attach".to_string(),
        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
            claim_name: ATTACH_CLAIM.to_string(),
            read_only: Some(true),
        }),
        ..Default::default()
    };
```

and, in the workspace container's `volume_mounts`:

```rust
        // Mounting over `/etc/resolv.conf` overrides the file the container runtime injects, which
        // is the only way to change a pod's DNS after it is running: `dnsConfig` is immutable on a
        // live pod. `subPath` so one claim serves every workspace in the namespace.
        VolumeMount {
            name: "attach".into(),
            mount_path: "/etc/resolv.conf".into(),
            sub_path: Some(format!("{id}/resolv.conf")),
            read_only: Some(true),
            ..Default::default()
        },
```

- [ ] **Step 5: Run them and watch them pass**

Run: `cargo test -p kloudlite-git-workspaces`
Expected: PASS. Other `workspace_pod` tests that count volumes or mounts may need their expected
counts updated — update the count, never the assertion's meaning.

- [ ] **Step 6: Commit**

```bash
git add crates/workspaces/src/k8s.rs
git commit -m "Mount each workspace's own resolv.conf from an agent-owned claim"
```

---

### Task 4: The two NetworkPolicies

**Files:**
- Modify: `crates/workspaces/src/k8s.rs` (two builders plus unit tests)

**Interfaces:**
- Consumes: `k8s::policy` (private helper, already there), `WORKSPACE_LABEL`.
- Produces:
  - `k8s::attach_egress(ws_ns, ws_id, env_ns, owner, owner_ref) -> NetworkPolicy`
  - `k8s::attach_ingress(env_ns, ws_ns, ws_id, owner, owner_ref) -> NetworkPolicy`
  - `k8s::attach_policy_name(ws_id: &str) -> String` → `attach-{ws_id}`

- [ ] **Step 1: Write the failing tests**

```rust
    /// The grant selects the POD, never the namespace: an owner's workspaces share a namespace, so
    /// a namespace-wide rule would open every workspace they have to the environment.
    #[test]
    fn the_attachment_egress_selects_one_workspace_pod() {
        let p = attach_egress("ws-acme", "ws-1", "env-abc", "acme", &owner_ref());
        assert_eq!(p.metadata.name.as_deref(), Some("attach-ws-1"));
        assert_eq!(p.metadata.namespace.as_deref(), Some("ws-acme"));
        let spec = serde_json::to_value(p.spec.unwrap()).unwrap();
        assert_eq!(spec["podSelector"]["matchLabels"][WORKSPACE_LABEL], "ws-1");
        assert_eq!(spec["policyTypes"], serde_json::json!(["Egress"]));
        assert_eq!(
            spec["egress"][0]["to"][0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"],
            "env-abc"
        );
    }

    /// The environment side names both the namespace and the pod: a namespace selector alone would
    /// admit every workspace of every owner who happens to share that namespace.
    #[test]
    fn the_attachment_ingress_names_the_namespace_and_the_pod() {
        let p = attach_ingress("env-abc", "ws-acme", "ws-1", "acme", &owner_ref());
        assert_eq!(p.metadata.namespace.as_deref(), Some("env-abc"));
        let spec = serde_json::to_value(p.spec.unwrap()).unwrap();
        let from = &spec["ingress"][0]["from"][0];
        assert_eq!(from["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"], "ws-acme");
        assert_eq!(from["podSelector"]["matchLabels"][WORKSPACE_LABEL], "ws-1");
        assert_eq!(spec["policyTypes"], serde_json::json!(["Ingress"]));
    }
```

Use whatever `owner_ref()` helper the neighbouring policy tests in that module already use.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p kloudlite-git-workspaces attachment_egress attachment_ingress`
Expected: FAIL — `cannot find function 'attach_egress'`.

- [ ] **Step 3: Implement them**

```rust
/// Both halves of an attachment grant share this name, one in each namespace, so a detach can
/// delete them by name without a lookup.
pub fn attach_policy_name(ws_id: &str) -> String {
    format!("attach-{ws_id}")
}

/// Lets one workspace pod reach the environment's namespace.
///
/// Egress needs its own rule because `allow_internet_egress` deliberately excludes RFC 1918, so
/// the pod network is unreachable by default — the environment's ClusterIP included.
pub fn attach_egress(
    ws_ns: &str,
    ws_id: &str,
    env_ns: &str,
    owner: &str,
    owner_ref: &OwnerReference,
) -> NetworkPolicy {
    policy(
        &attach_policy_name(ws_id),
        ws_ns,
        owner,
        owner_ref,
        json!({
            "podSelector": { "matchLabels": { WORKSPACE_LABEL: ws_id } },
            "policyTypes": ["Egress"],
            "egress": [{
                "to": [{ "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": env_ns } } }],
            }],
        }),
    )
}

/// Lets the environment accept that one workspace pod.
///
/// `from` names the namespace AND the pod, in one element so they are ANDed: as two elements they
/// would be ORed, which would admit every pod in the workspace namespace and every pod anywhere
/// carrying that label.
pub fn attach_ingress(
    env_ns: &str,
    ws_ns: &str,
    ws_id: &str,
    owner: &str,
    owner_ref: &OwnerReference,
) -> NetworkPolicy {
    policy(
        &attach_policy_name(ws_id),
        env_ns,
        owner,
        owner_ref,
        json!({
            "podSelector": {},
            "policyTypes": ["Ingress"],
            "ingress": [{
                "from": [{
                    "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": ws_ns } },
                    "podSelector": { "matchLabels": { WORKSPACE_LABEL: ws_id } },
                }],
            }],
        }),
    )
}
```

- [ ] **Step 4: Run them and watch them pass**

Run: `cargo test -p kloudlite-git-workspaces attachment_egress attachment_ingress`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/k8s.rs
git commit -m "Add the two policies an attachment opens"
```

---

### Task 5: The reconciler — write the file, ensure the claim, apply the grants

**Files:**
- Modify: `bins/agent/src/controller.rs` (`apply_workspace`, before pod creation)
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: `k8s::{resolv_conf, attach_pv_name, ATTACH_CLAIM, attach_root, attach_dir, attach_file,
  attach_egress, attach_ingress, attach_policy_name}`, `ensure_storage`, `crd::env_namespace`,
  `crd::ws_namespace`.
- Produces: `controller::write_resolv_conf(pool, ws_id, ws_ns, env_ns) -> Result<(), ReconcileErr>`.

- [ ] **Step 1: Write the failing tests**

In `bins/agent/tests/reconcile.rs`, following the existing reconcile-test shape:

```rust
/// Attaching writes both halves of the grant. The file itself is asserted by the engine tests —
/// here what matters is that the reconcile reaches the policies at all.
#[tokio::test]
async fn an_attached_workspace_gets_both_halves_of_the_grant() {
    let (ctx, rec) = ctx_full_default();
    let mut w: crd::Workspace = serde_json::from_value(ws_json_ready()).unwrap();
    w.spec.attached_environment = Some("env-abc".into());
    kloudlite_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();

    let egress = rec.sent_to("POST", "/apis/networking.k8s.io/v1/namespaces/ws-acme/networkpolicies");
    assert!(egress.iter().any(|b| b["metadata"]["name"] == "attach-ws-1"), "workspace-side policy");
    let ingress = rec.sent_to("POST", "/apis/networking.k8s.io/v1/namespaces/env-abc/networkpolicies");
    assert!(ingress.iter().any(|b| b["metadata"]["name"] == "attach-ws-1"), "environment-side policy");
}

/// A stale id is not an error. `/v1` clears the field when an environment is deleted, but a crash
/// mid-delete must degrade to "not attached" rather than leaving a grant pointing at nothing.
#[tokio::test]
async fn a_workspace_attached_to_a_missing_environment_reconciles_unattached() {
    let (ctx, rec) = ctx_full_with_missing_env();
    let mut w: crd::Workspace = serde_json::from_value(ws_json_ready()).unwrap();
    w.spec.attached_environment = Some("env-gone".into());
    kloudlite_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();

    let posts = rec.sent_to("POST", "/apis/networking.k8s.io/v1/namespaces/env-gone/networkpolicies");
    assert!(posts.is_empty(), "no grant for an environment that is not there");
    let status = rec.sent("PATCH", WS_STATUS);
    let cond = &status.last().expect("a status write")["status"]["conditions"];
    assert!(
        cond.as_array().unwrap().iter().any(|c| c["type"] == "Attached" && c["reason"] == "EnvironmentNotFound"),
        "the refusal is reported, not silent"
    );
}

/// A different region is a different cluster: no route, no DNS. Refused by the reconciler as well
/// as by `/v1`, because a spec can arrive by any path.
#[tokio::test]
async fn a_cross_region_attachment_is_refused() {
    let (ctx, rec) = ctx_full_with_env_in_region("other-region");
    let mut w: crd::Workspace = serde_json::from_value(ws_json_ready()).unwrap();
    w.spec.attached_environment = Some("env-abc".into());
    kloudlite_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();

    let posts = rec.sent_to("POST", "/apis/networking.k8s.io/v1/namespaces/env-abc/networkpolicies");
    assert!(posts.is_empty(), "no grant across a region boundary");
    let status = rec.sent("PATCH", WS_STATUS);
    let cond = &status.last().expect("a status write")["status"]["conditions"];
    assert!(cond.as_array().unwrap().iter().any(|c| c["reason"] == "RegionMismatch"));
}
```

The three `ctx_*` helpers do not exist yet. Build them from the existing `ctx_full`, adding the
`Environment` the fake API server should answer with (or a 404). Follow how the existing tests
register objects with the recorder — do not invent a new fixture style.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p kloudlite-git-agent-bin attached`
Expected: FAIL — the helpers and the behaviour do not exist.

- [ ] **Step 3: Write the file writer**

In `bins/agent/src/controller.rs`:

```rust
/// Render this workspace's `/etc/resolv.conf` into the agent-owned attach directory.
///
/// IN PLACE, never via a rename. The pod bind-mounts this file by inode, so replacing it with
/// `rename(2)` — the usual way to write a file atomically — leaves every running pod reading the
/// OLD inode and attachment silently stops working. Verified on a live cluster; do not "fix" this
/// into an atomic write.
///
/// Before the pod, never after: a `subPath` whose target does not exist is created as a DIRECTORY,
/// and a directory at `/etc/resolv.conf` breaks every name lookup in the workspace.
pub(crate) fn write_resolv_conf(
    pool: &str,
    ws_id: &str,
    ws_ns: &str,
    env_ns: Option<&str>,
) -> Result<(), ReconcileErr> {
    let dir = k8s::attach_dir(pool, ws_id);
    std::fs::create_dir_all(&dir).map_err(|e| ReconcileErr(format!("attach dir {dir}: {e}")))?;
    let path = k8s::attach_file(pool, ws_id);
    // A directory here is the failure above having already happened once; clear it rather than
    // leaving the workspace with no DNS for as long as the pod lives.
    if std::fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false) {
        std::fs::remove_dir_all(&path).map_err(|e| ReconcileErr(format!("attach file {path}: {e}")))?;
    }
    let template = std::fs::read_to_string("/etc/resolv.conf")
        .map_err(|e| ReconcileErr(format!("reading the agent's resolv.conf: {e}")))?;
    let body = k8s::resolv_conf(&template, ws_ns, env_ns);
    std::fs::write(&path, body).map_err(|e| ReconcileErr(format!("writing {path}: {e}")))
}
```

`std::fs::write` truncates and writes the existing inode, which is exactly what is wanted here.

- [ ] **Step 4: Wire it into `apply_workspace`**

Before the pod is created — next to the existing `ensure_storage` calls for the live and Nix
claims — add the attach claim and resolve the attachment:

```rust
    ensure_storage(
        &ns,
        &k8s::attach_pv_name(&ns),
        k8s::ATTACH_CLAIM,
        &k8s::attach_root(&ctx.pool),
        "ReadOnlyMany",
        1,
        &w.spec.owner,
        &pod_ctx,
        ctx,
    )
    .await?;

    // Resolve the attachment before writing anything: a missing or cross-region environment is
    // reported and treated as unattached, never as a half-applied grant.
    let (env_ns, attached) = match w.spec.attached_environment.as_deref() {
        None => (None, None),
        Some(env_id) => {
            let envs: Api<crd::Environment> = Api::all(ctx.client.clone());
            match envs.get_opt(env_id).await? {
                None => (None, Some(("EnvironmentNotFound", env_id.to_string()))),
                Some(e) if e.spec.region != w.spec.region => {
                    (None, Some(("RegionMismatch", env_id.to_string())))
                }
                Some(_) => (Some(crd::env_namespace(env_id)), None),
            }
        }
    };
    write_resolv_conf(&ctx.pool, &id, &ns, env_ns.as_deref())?;
```

Then apply or remove the two policies:

```rust
    let policies: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &ns);
    match env_ns.as_deref() {
        Some(env) => {
            ensure(&policies, &k8s::attach_egress(&ns, &id, env, &w.spec.owner, &pod_ctx.owner_ref), ctx).await?;
            // The environment-side half cannot be owned by this Workspace: an ownerReference may
            // not cross namespaces. It is owned by the ENVIRONMENT instead, so deleting the
            // environment collects it, and detach deletes it by name.
            let env_obj: Api<crd::Environment> = Api::all(ctx.client.clone());
            if let Some(e) = env_obj.get_opt(env.trim_start_matches("env-")).await? {
                let env_ref = k8s::owner_ref_of_kind(&e, "Environment");
                let in_env: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), env);
                ensure(&in_env, &k8s::attach_ingress(env, &ns, &id, &w.spec.owner, &env_ref), ctx).await?;
            }
        }
        None => {
            delete_ignoring_404(&policies, &k8s::attach_policy_name(&id)).await?;
        }
    }
```

Use the existing `ensure`, `delete_ignoring_404` and `owner_ref_of_kind` helpers; if
`owner_ref_of_kind` has a different signature, match it rather than changing it.

- [ ] **Step 5: Report it in status**

Where `ws_conditions` builds the condition list, add an `Attached` condition: `True` with the
environment id as the message when `env_ns` is `Some`, `False` with the reason from `attached` when
it is a refusal, and absent when the workspace is not attached at all.

- [ ] **Step 6: Run the tests and watch them pass**

Run: `cargo test -p kloudlite-git-agent-bin`
Expected: PASS, including the three new tests.

- [ ] **Step 7: Commit**

```bash
git add bins/agent/src/controller.rs bins/agent/tests/reconcile.rs
git commit -m "Apply a workspace's attachment on reconcile"
```

---

### Task 6: Detach cleanup and the workspace finalizer

**Files:**
- Modify: `bins/agent/src/controller.rs` (the workspace finalizer path)
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: everything from Task 5.
- Produces: no new public names.

- [ ] **Step 1: Write the failing test**

```rust
/// Deleting a workspace takes its attach directory and the environment-side policy with it. The
/// workspace-side policy is collected by its ownerReference; the environment-side one cannot be,
/// because an ownerReference may not cross namespaces.
#[tokio::test]
async fn deleting_an_attached_workspace_removes_the_environment_side_grant() {
    let (ctx, rec) = ctx_full_default();
    let mut w: crd::Workspace = serde_json::from_value(ws_json_ready()).unwrap();
    w.spec.attached_environment = Some("env-abc".into());
    w.metadata.deletion_timestamp = Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(chrono::Utc::now()));
    kloudlite_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();

    let deletes = rec.sent_to("DELETE", "/apis/networking.k8s.io/v1/namespaces/env-abc/networkpolicies/attach-ws-1");
    assert_eq!(deletes.len(), 1, "the environment-side policy is deleted by name");
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p kloudlite-git-agent-bin deleting_an_attached_workspace`
Expected: FAIL — no DELETE is sent.

- [ ] **Step 3: Implement it**

In the workspace's finalizer branch, before the volume teardown it already does:

```rust
    // The environment-side policy and the attach directory are the two things nothing else
    // collects: the policy lives in another namespace, and the directory is on the node's pool.
    if let Some(env_id) = w.spec.attached_environment.as_deref() {
        let env_ns = crd::env_namespace(env_id);
        let in_env: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &env_ns);
        delete_ignoring_404(&in_env, &k8s::attach_policy_name(&id)).await?;
    }
    let dir = k8s::attach_dir(&ctx.pool, &id);
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(dir = %dir, error = %e, "removing the attach directory");
        }
    }
```

- [ ] **Step 4: Run it and watch it pass**

Run: `cargo test -p kloudlite-git-agent-bin`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/controller.rs bins/agent/tests/reconcile.rs
git commit -m "Clean up an attachment when its workspace goes"
```

---

### Task 7: The `/v1` attach and detach routes

**Files:**
- Modify: `crates/workspaces/src/api.rs` (router, two handlers, and `delete_env`)
- Test: `crates/workspaces/tests/api_user.rs`

**Interfaces:**
- Consumes: `crd::WorkspaceSpec.attached_environment`, `my_ws`, `find_env`, `may_act_on`.
- Produces: `POST /v1/workspaces/{id}/attach` taking `{"environment": "env-…"}`, and
  `POST /v1/workspaces/{id}/detach`.

- [ ] **Step 1: Write the failing tests**

In `crates/workspaces/tests/api_user.rs`, in the style of the neighbouring workspace-route tests:

```rust
/// Attaching writes spec and nothing else — the agent converges the rest.
#[tokio::test]
async fn attaching_sets_the_spec_field() {
    let (app, rec) = user_app_with(vec![ws_obj("ws-1", "alice", "r1"), env_obj("env-1", "alice", "r1")]);
    let r = post(&app, "/v1/workspaces/ws-1/attach", json!({"environment": "env-1"})).await;
    assert_eq!(r.status(), 202);
    let patch = rec.last_patch("/apis/kloudlite-git.io/v1alpha1/workspaces/ws-1");
    assert_eq!(patch["spec"]["attachedEnvironment"], "env-1");
    assert!(patch["status"].is_null(), "the API writes spec only");
}

/// A different region is a different cluster: there is no route and no DNS between them, so this
/// is refused before anything is written rather than failing later in a reconcile.
#[tokio::test]
async fn attaching_across_regions_is_refused() {
    let (app, rec) = user_app_with(vec![ws_obj("ws-1", "alice", "r1"), env_obj("env-1", "alice", "r2")]);
    let r = post(&app, "/v1/workspaces/ws-1/attach", json!({"environment": "env-1"})).await;
    assert_eq!(r.status(), 409);
    assert!(rec.patches().is_empty(), "nothing is written on a refusal");
}

/// An environment the caller has no part in is a 404, not a 403: the same answer as one that does
/// not exist, so the route cannot be used to discover other people's environments.
#[tokio::test]
async fn attaching_someone_elses_environment_is_not_found() {
    let (app, _rec) = user_app_with(vec![ws_obj("ws-1", "alice", "r1"), env_obj("env-1", "bob", "r1")]);
    let r = post(&app, "/v1/workspaces/ws-1/attach", json!({"environment": "env-1"})).await;
    assert_eq!(r.status(), 404);
}

/// Detach is idempotent: it is the state the caller wants, not an event.
#[tokio::test]
async fn detaching_twice_is_accepted_twice() {
    let (app, _rec) = user_app_with(vec![ws_obj("ws-1", "alice", "r1")]);
    assert_eq!(post(&app, "/v1/workspaces/ws-1/detach", json!({})).await.status(), 202);
    assert_eq!(post(&app, "/v1/workspaces/ws-1/detach", json!({})).await.status(), 202);
}
```

Use the fixture helpers that already exist in that file; if `env_obj` does not exist, build it the
way the environment tests in `api_teams.rs` build theirs.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p kloudlite-git-workspaces --test api_user attach`
Expected: FAIL — 404 from the router, the routes do not exist.

- [ ] **Step 3: Add the routes**

In `router`, next to the `start`/`stop` lines:

```rust
        .route("/v1/workspaces/{id}/attach", post(attach_ws))
        .route("/v1/workspaces/{id}/detach", post(detach_ws))
```

- [ ] **Step 4: Implement the handlers**

```rust
#[derive(serde::Deserialize)]
struct AttachBody {
    environment: String,
}

/// Attach this workspace to an environment, so its services resolve by bare name.
///
/// A merge patch on the one field: this handler was sent one field and must not claim ownership of
/// a spec the caller never wrote — the same reason `set_desired` is a merge patch.
async fn attach_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<AttachBody>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    let w = my_ws(&s, &owner, &id).await?;
    // `find_env` answers 404 for an environment the caller has no part in, which is what keeps this
    // route from being a way to discover other people's environments.
    let e = find_env(&s, &owner, &body.environment).await?;
    if e.spec.region != w.spec.region {
        return Err((
            StatusCode::CONFLICT,
            "the environment is in another region, which is another cluster",
        )
            .into_response());
    }
    let api: Api<crd::Workspace> = Api::all(kube(&s)?.clone());
    let patch = serde_json::json!({"spec": {"attachedEnvironment": body.environment}});
    api.patch(&id, &PatchParams::default(), &Patch::Merge(&patch)).await.map_err(kube_err)?;
    Ok(StatusCode::ACCEPTED.into_response())
}

/// Detach. Idempotent: a workspace that is not attached is already in the state being asked for.
async fn detach_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    my_ws(&s, &owner, &id).await?;
    let api: Api<crd::Workspace> = Api::all(kube(&s)?.clone());
    // `null` is how a merge patch REMOVES a key. Setting it to "" would leave the reconciler
    // resolving an environment named "".
    let patch = serde_json::json!({"spec": {"attachedEnvironment": serde_json::Value::Null}});
    api.patch(&id, &PatchParams::default(), &Patch::Merge(&patch)).await.map_err(kube_err)?;
    Ok(StatusCode::ACCEPTED.into_response())
}
```

- [ ] **Step 5: Clear the field when an environment is deleted**

In `delete_env`, after the delete succeeds — only `/v1` may write spec, so this cannot be the
agent's job:

```rust
    // Only `/v1` writes spec, so clearing the attachment is this handler's job. Best-effort: the
    // reconciler treats a missing environment as unattached anyway, so a failure here degrades to
    // a stale field rather than a dangling grant.
    let wss: Api<crd::Workspace> = Api::all(c.clone());
    if let Ok(list) = wss.list(&ListParams::default()).await {
        for w in list.items.iter().filter(|w| w.spec.attached_environment.as_deref() == Some(id.as_str())) {
            let patch = serde_json::json!({"spec": {"attachedEnvironment": serde_json::Value::Null}});
            if let Err(e) = wss.patch(&w.name_any(), &PatchParams::default(), &Patch::Merge(&patch)).await {
                tracing::warn!(workspace = %w.name_any(), error = %e, "clearing an attachment");
            }
        }
    }
```

- [ ] **Step 6: Run them and watch them pass**

Run: `cargo test -p kloudlite-git-workspaces --test api_user`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/workspaces/src/api.rs crates/workspaces/tests/api_user.rs
git commit -m "Attach and detach a workspace over /v1"
```

---

### Task 8: The end-to-end proof and the docs

**Files:**
- Modify: `tests/ws_e2e.sh`
- Modify: `CLAUDE.md` ("Workspaces and environments")
- Modify: `README.md` (the workspaces section, if it describes environments)

**Interfaces:** none.

- [ ] **Step 1: Add the e2e phase**

In `tests/ws_e2e.sh`, after an environment is running and a workspace exists, following the file's
existing helper style:

```bash
# Attachment: the one place the whole path is exercised together — the rendered file, the subPath
# mount, both NetworkPolicies and CoreDNS. Everything below this line needs a real cluster.
say "attach: the workspace resolves an environment service by name"
kl_api POST "/v1/workspaces/$WS_ID/attach" "{\"environment\":\"$ENV_ID\"}"
# No restart: the pod that was already running must see it, so do NOT recreate it here.
for i in $(seq 1 30); do
  if ws_exec "$WS_ID" "getent hosts db" >/dev/null 2>&1; then break; fi
  sleep 2
done
ws_exec "$WS_ID" "getent hosts db" || fail "the attached environment's service does not resolve"

say "detach: it stops resolving"
kl_api POST "/v1/workspaces/$WS_ID/detach" "{}"
for i in $(seq 1 30); do
  if ! ws_exec "$WS_ID" "getent hosts db" >/dev/null 2>&1; then break; fi
  sleep 2
done
ws_exec "$WS_ID" "getent hosts db" && fail "the service still resolves after a detach"
```

Match the script's actual helper names (`kl_api`, `ws_exec`, `say`, `fail`) — if they differ, use
what is there rather than adding new ones.

- [ ] **Step 2: Run it where it can run**

Run: `./tests/ws_e2e.sh`
Expected on this Mac: exit 77 (a prerequisite is missing) — that is a skip, not a pass. The
attachment phase only runs on a Linux node with btrfs and a reachable cluster.

- [ ] **Step 3: Document it**

In `CLAUDE.md`, in "Workspaces and environments", add a paragraph after the DNS sentence:

```markdown
A workspace may be attached to ONE environment (`Workspace.spec.attachedEnvironment`, written only
by `/v1`), and then resolves that environment's services by bare name. The mechanism is a
`/etc/resolv.conf` the agent renders per workspace into `{pool}/attach/{ws}/resolv.conf` and every
pod mounts read-only through one per-namespace `local` PV (`attach-{ns}`/`attach`) with a
`subPath` — `dnsConfig` is immutable on a running pod, so the mount is what makes attach and detach
take effect without a restart. That file is written IN PLACE and never renamed: the pod holds the
inode, so a rename would leave it reading the old file forever. Two NetworkPolicies named
`attach-{ws}` open the path, selecting the workspace POD (siblings share a namespace); the
environment-side one is owned by the Environment because an ownerReference cannot cross namespaces.
```

- [ ] **Step 4: Commit**

```bash
git add tests/ws_e2e.sh CLAUDE.md README.md
git commit -m "Prove attachment end to end and write down how it works"
```

---

## Self-review

**Spec coverage.** Field and CRD → Task 1. resolv.conf rendering → Task 2. The mount, PV and claim
→ Task 3. Both policies → Task 4. Reconcile, ordering, the three refusal reasons and status →
Task 5. Finalizer cleanup → Task 6. Routes, region check, authorization and clearing on
environment delete → Task 7. E2E and docs → Task 8. The lifecycle table's "environment stopped"
row needs no code: the search domain names a namespace, not a pod, so a stopped environment simply
stops answering.

**Not covered on purpose.** No web UI — the spec does not ask for one. No `ExternalName` mirrors,
no multi-attach, no cross-region attach; all three are in the spec's "not in scope".

**Type consistency.** `attached_environment` (Rust) ↔ `attachedEnvironment` (JSON) throughout;
`attach_policy_name` produces the one name used by both halves and by both delete paths;
`ATTACH_CLAIM` is the claim name in Task 3's pod volume and Task 5's `ensure_storage` call;
`attach_dir`/`attach_file` are used by Task 5's writer and Task 6's cleanup.

**Known soft spot for the implementer.** Task 5's helper names (`ensure`, `delete_ignoring_404`,
`owner_ref_of_kind`, `ws_conditions`) and Task 7's fixtures are quoted from the tree as it stands.
If a signature differs, match the tree — do not reshape the tree to match this plan.
