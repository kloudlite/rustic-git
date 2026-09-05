# Workspace Packages Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A workspace's `spec.packages` names nixpkgs packages; the owning node builds one Nix profile per workspace on a host Nix store before the pod starts, and the pod sees them on `PATH` read-only.

**Architecture:** A `nix-daemon` container joins the agent DaemonSet with the host's `/nix` mounted (the store is seeded from the image on first run, so the agent container uses the same `nix` binary from the host store). The Workspace reconciler gains one step between the PV/PVC and the pod: validate and hash `spec.packages`, and if the node's profile out-link does not match, run `nix build` of a `buildEnv` on a blocking thread and publish it by rename. The pod mounts `/nix/store` and its own profile through a second, read-only local PV.

**Tech Stack:** Rust (kube-rs controller, `serde_yaml` 0.9 already in the lock), Nix 2.24 (`nixos/nix` image, `nix-command flakes`), k8s local PVs, Next.js web (read-only projection).

**Spec:** `docs/superpowers/specs/2026-08-28-workspace-packages-design.md`

## Global Constraints

- The truth is `spec.packages` on the Workspace CR (written by `/v1` create and `PATCH /v1/workspaces/{id}`; edited in the web UI). No file in the repo.
- Attribute grammar: `^[A-Za-z0-9_][A-Za-z0-9_.+-]*$`, ≤ 64 chars, ≤ 100 entries, no duplicates. Validated at the API (422 naming the entry) AND by the reconciler before rendering.
- Profiles live at `/nix/var/kloudlite/profiles/{id}` (GC root out-link); in-flight build at `{id}.building`; publish = `rename`.
- `nix` is exec'd with an argv, never through a shell; the expression is rendered from validated names as a Nix list literal. Deadline `WS_NIX_TIMEOUT` (default 1200 s).
- Pod mounts: PVC `nix-{id}` → `/nix/store` (subPath `store`) and `/nix/profile` (subPath `var/kloudlite/profiles/{id}`), both `readOnly: true`, `ReadOnlyMany`. Never a hostPath in a tenant pod; PSA `baseline` unchanged.
- Env in the pod: `PATH=/nix/profile/bin:<image PATH or /usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin>`, `NIX_PROFILE=/nix/profile`, `MANPATH=/nix/profile/share/man:`, `XDG_DATA_DIRS=/nix/profile/share:/usr/local/share:/usr/share`.
- `status.packages { observed, observedHash, profile, nixpkgs }` and condition `PackagesReady` with reasons `Built | Building | BuildFailed | NoNix`.
- A failed or invalid build keeps the previous profile; a workspace with no profile at all waits (no pod) with the condition set.
- Comments explain WHY (house style); deliberate shortcuts carry `// ponytail:`. Commit subjects imperative sentence case, no tool attribution.

---

## File map

| File | Responsibility |
|---|---|
| `crates/workspaces/src/packages.rs` (new) | Pure: `validate_list`, `validate_attr`, `hash`, `expression`, `path_env`. No IO, no Nix. |
| `crates/workspaces/src/crd.rs` | `spec.packages` on `WorkspaceSpec`; `PackagesStatus` on `WorkspaceStatus`. |
| `deploy/k3s/crds.yaml` | regenerated. |
| `crates/workspaces/src/k8s.rs` | `nix_pv`, `nix_claim`, `nix_volume`; mounts + env on `workspace_pod`. |
| `bins/agent/src/nix.rs` (new) | The runner: `Nix` trait (`build`, `ping`, `collect_garbage`), `RealNix` (exec), profile paths, `publish`, `remove_profile`. |
| `bins/agent/src/controller.rs` | `ensure_profile` step in `apply_workspace`; `wake_workspace`; out-link removal in `cleanup_volume`. |
| `bins/agent/src/lib.rs` | `Ctx.nix`, env config, janitor GC beat. |
| `bins/agent/tests/reconcile.rs` | reconcile tests with a fake `Nix`. |
| `deploy/k3s/agent-daemonset.yaml`, `deploy/k3s/nix-conf.yaml` (new) | daemon container, store seed init container, `/nix` hostPath, ConfigMap. |
| `crates/workspaces/src/api.rs`, `model.rs` | `packages` on create, `PATCH /v1/workspaces/{id}`, `packages` + `packages_status` on the doc. |
| `web/apps/web/src/lib/api.ts`, `components/app/workspace-list.tsx`, `workspaces/actions.ts` | packages input on create, packages editor per row, status. |
| `tests/ws_e2e.sh` | packages phase via the api. |

---

### Task 1: `packages.rs` — parse, validate, hash, render

**Files:**
- Create: `crates/workspaces/src/packages.rs`
- Modify: `crates/workspaces/src/lib.rs` (add `pub mod packages;`)
- Modify: `Cargo.toml` (workspace deps: `serde_yaml = "0.9"`), `crates/workspaces/Cargo.toml` (`serde_yaml = { workspace = true }`)

**Interfaces:**
- Produces:
  ```rust
  pub const FILE_NAME: &str = "kloudlite.yaml";
  pub const MAX_FILE_BYTES: usize = 64 * 1024;
  pub struct Packages { pub packages: Vec<String>, pub nixpkgs: Option<String> }
  pub enum FileError { TooLarge(usize), Yaml(String), Attr(String), Pin(String), TooMany(usize), Duplicate(String) }
  impl std::fmt::Display for FileError
  pub fn parse_file(bytes: &[u8]) -> Result<Packages, FileError>   // empty/absent `packages` → empty Vec
  pub fn validate_attr(s: &str) -> Result<(), FileError>
  pub fn hash(pin: &str, packages: &[String]) -> String            // "sha256:<hex>", order-independent
  pub fn expression(pin: &str, id: &str, packages: &[String]) -> String
  pub fn path_env(image_path: Option<&str>) -> String
  ```

- [ ] **Step 1: Write the failing tests** (bottom of `packages.rs`, `#[cfg(test)] mod tests`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_file_parses_and_unknown_keys_are_ignored() {
        let p = parse_file(b"packages:\n  - nodejs_20\n  - python3Packages.requests\nports: [3000]\n").unwrap();
        assert_eq!(p.packages, ["nodejs_20", "python3Packages.requests"]);
        assert_eq!(p.nixpkgs, None);
    }

    #[test]
    fn a_missing_packages_key_is_an_empty_list() {
        assert!(parse_file(b"name: demo\n").unwrap().packages.is_empty());
        assert!(parse_file(b"").unwrap().packages.is_empty());
    }

    #[test]
    fn the_attribute_grammar_refuses_anything_that_could_be_code() {
        for bad in ["$(id)", "a b", "a\"b", "a;b", "(x)", "-lead", "", &"x".repeat(65)] {
            assert!(validate_attr(bad).is_err(), "{bad:?} must be refused");
        }
        for ok in ["hello", "nodejs_20", "python3Packages.requests", "gcc-wrapper", "libc++"] {
            assert!(validate_attr(ok).is_ok(), "{ok:?} must pass");
        }
    }

    #[test]
    fn the_file_is_untrusted() {
        assert!(matches!(parse_file(&vec![b' '; MAX_FILE_BYTES + 1]), Err(FileError::TooLarge(_))));
        assert!(matches!(parse_file(b"packages: !!binary abc\n"), Err(FileError::Yaml(_))));
        assert!(matches!(parse_file(b"packages: &a [hello]\nother: *a\n"), Err(FileError::Yaml(_))));
        assert!(matches!(parse_file(b"packages: [hello, hello]\n"), Err(FileError::Duplicate(_))));
        let many: Vec<String> = (0..101).map(|i| format!("p{i}")).collect();
        let yaml = format!("packages: [{}]\n", many.join(", "));
        assert!(matches!(parse_file(yaml.as_bytes()), Err(FileError::TooMany(101))));
        assert!(matches!(parse_file(b"packages: [hello]\nnixpkgs: github:evil/nixpkgs/abc\n"), Err(FileError::Pin(_))));
        let pin = "github:NixOS/nixpkgs/".to_string() + &"a".repeat(40);
        assert_eq!(parse_file(format!("packages: [hello]\nnixpkgs: {pin}\n").as_bytes()).unwrap().nixpkgs.as_deref(), Some(pin.as_str()));
    }

    #[test]
    fn the_hash_is_order_independent_and_pin_sensitive() {
        let a = hash("github:NixOS/nixpkgs/aaaa", &["go".into(), "jq".into()]);
        let b = hash("github:NixOS/nixpkgs/aaaa", &["jq".into(), "go".into()]);
        let c = hash("github:NixOS/nixpkgs/bbbb", &["go".into(), "jq".into()]);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("sha256:"));
    }

    #[test]
    fn the_expression_is_a_list_literal_never_interpolated_text() {
        let e = expression("github:NixOS/nixpkgs/aaaa", "ws-1", &["go".into(), "python3Packages.requests".into()]);
        assert_eq!(
            e,
            "let pkgs = import (builtins.getFlake \"github:NixOS/nixpkgs/aaaa\") { }; in pkgs.buildEnv { name = \"ws-ws-1-env\"; paths = [ pkgs.go pkgs.python3Packages.requests ]; }"
        );
        let empty = expression("github:NixOS/nixpkgs/aaaa", "ws-1", &[]);
        assert!(empty.contains("paths = [  ];"));
    }

    #[test]
    fn path_env_prepends_the_profile_and_falls_back_to_a_sane_default() {
        assert_eq!(path_env(Some("/opt/bin:/usr/bin")), "/nix/profile/bin:/opt/bin:/usr/bin");
        assert_eq!(path_env(None), "/nix/profile/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p kloudlite-workspaces packages`
Expected: compile error, `packages` module not found.

- [ ] **Step 3: Implement**

`Cargo.toml` (workspace root, under `[workspace.dependencies]`): `serde_yaml = "0.9"`. `crates/workspaces/Cargo.toml`: `serde_yaml = { workspace = true }`. `crates/workspaces/src/lib.rs`: `pub mod packages;`.

```rust
//! The package list a workspace declares in its repository, and everything the reconciler needs
//! derived from it. Pure on purpose: this module never touches the disk or Nix, so every rule
//! about what a file may say is testable without either.
//!
//! The file is read from a user-writable subvolume by a process that is root on the host, so it
//! is treated as hostile input end to end: bounded in size, parsed as data, and every attribute
//! name checked against a grammar that cannot spell code before it is ever rendered into an
//! expression.

use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const FILE_NAME: &str = "kloudlite.yaml";
pub const MAX_FILE_BYTES: usize = 64 * 1024;
pub const MAX_PACKAGES: usize = 100;
pub const MAX_ATTR_LEN: usize = 64;
/// Inside the pod, where the workspace's own profile is mounted.
pub const PROFILE_MOUNT: &str = "/nix/profile";
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Packages {
    pub packages: Vec<String>,
    pub nixpkgs: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FileError {
    TooLarge(usize),
    Yaml(String),
    Attr(String),
    Pin(String),
    TooMany(usize),
    Duplicate(String),
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileError::TooLarge(n) => write!(f, "{FILE_NAME} is {n} bytes; the limit is {MAX_FILE_BYTES}"),
            FileError::Yaml(e) => write!(f, "{FILE_NAME}: {e}"),
            FileError::Attr(a) => write!(f, "{FILE_NAME}: {a:?} is not a package attribute name"),
            FileError::Pin(p) => write!(f, "{FILE_NAME}: nixpkgs must be github:NixOS/nixpkgs/<commit>, not {p:?}"),
            FileError::TooMany(n) => write!(f, "{FILE_NAME}: {n} packages; the limit is {MAX_PACKAGES}"),
            FileError::Duplicate(a) => write!(f, "{FILE_NAME}: {a:?} is listed twice"),
        }
    }
}

#[derive(Deserialize, Default)]
struct Raw {
    #[serde(default)]
    packages: Vec<String>,
    #[serde(default)]
    nixpkgs: Option<String>,
}

pub fn parse_file(bytes: &[u8]) -> Result<Packages, FileError> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(FileError::TooLarge(bytes.len()));
    }
    let text = std::str::from_utf8(bytes).map_err(|e| FileError::Yaml(e.to_string()))?;
    // Tags and anchors are the two YAML features that make a document more than data. serde_yaml
    // resolves anchors before we see them, so they are refused by text: an alias-free file has no
    // `&`/`*` at a value's head and no `!` tag anywhere.
    if text.lines().any(|l| {
        let t = l.trim_start();
        t.contains("!!") || t.contains(": &") || t.contains(": *") || t.starts_with("- &") || t.starts_with("- *") || t.contains(" !")
    }) {
        return Err(FileError::Yaml("tags and anchors are not allowed".into()));
    }
    let raw: Raw = if text.trim().is_empty() {
        Raw::default()
    } else {
        serde_yaml::from_str(text).map_err(|e| FileError::Yaml(e.to_string()))?
    };
    if raw.packages.len() > MAX_PACKAGES {
        return Err(FileError::TooMany(raw.packages.len()));
    }
    let mut seen = std::collections::HashSet::new();
    for p in &raw.packages {
        validate_attr(p)?;
        if !seen.insert(p.as_str()) {
            return Err(FileError::Duplicate(p.clone()));
        }
    }
    if let Some(pin) = &raw.nixpkgs {
        validate_pin(pin)?;
    }
    Ok(Packages { packages: raw.packages, nixpkgs: raw.nixpkgs })
}

pub fn validate_attr(s: &str) -> Result<(), FileError> {
    let mut chars = s.chars();
    let ok_first = chars.next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    let ok_rest = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-'));
    if ok_first && ok_rest && s.len() <= MAX_ATTR_LEN {
        Ok(())
    } else {
        Err(FileError::Attr(s.to_string()))
    }
}

fn validate_pin(p: &str) -> Result<(), FileError> {
    let rev = p.strip_prefix("github:NixOS/nixpkgs/").ok_or_else(|| FileError::Pin(p.to_string()))?;
    if rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) {
        Ok(())
    } else {
        Err(FileError::Pin(p.to_string()))
    }
}

/// What the profile on disk IS: the pin and the sorted list. Sorted so a reordered file is not a
/// rebuild; pinned so a rolled nixpkgs is.
pub fn hash(pin: &str, packages: &[String]) -> String {
    let mut sorted: Vec<&str> = packages.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let mut h = Sha256::new();
    h.update(pin.as_bytes());
    for p in sorted {
        h.update(b"\n");
        h.update(p.as_bytes());
    }
    format!("sha256:{:x}", h.finalize())
}

/// The whole expression `nix build --expr` evaluates. Names arrive validated (`validate_attr`)
/// and are emitted as `pkgs.<name>` inside a list literal — there is no string context in the
/// expression a name could escape into.
pub fn expression(pin: &str, id: &str, packages: &[String]) -> String {
    let paths: Vec<String> = packages.iter().map(|p| format!("pkgs.{p}")).collect();
    format!(
        "let pkgs = import (builtins.getFlake \"{pin}\") {{ }}; in pkgs.buildEnv {{ name = \"ws-{id}-env\"; paths = [ {} ]; }}",
        paths.join(" ")
    )
}

/// The image's own PATH is unknown to us at apply time — the kubelet only merges env on top of
/// the image's — so the container gets an explicit one: profile first, then a default that every
/// Debian/Alpine image already has.
pub fn path_env(image_path: Option<&str>) -> String {
    format!("{PROFILE_MOUNT}/bin:{}", image_path.unwrap_or(DEFAULT_PATH))
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p kloudlite-workspaces packages`
Expected: 7 passed. Also `cargo clippy -p kloudlite-workspaces -- -D warnings` clean.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/workspaces/Cargo.toml crates/workspaces/src/lib.rs crates/workspaces/src/packages.rs
git commit -m "Parse and validate a workspace's kloudlite.yaml package list"
```

---

### Task 2: `status.packages` on the Workspace CRD

**Files:**
- Modify: `crates/workspaces/src/crd.rs` (`WorkspaceStatus`, ~line 333)
- Regenerate: `deploy/k3s/crds.yaml`

**Interfaces:**
- Produces:
  ```rust
  #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
  #[serde(rename_all = "camelCase")]
  pub struct PackagesStatus {
      #[serde(default, skip_serializing_if = "Vec::is_empty")] pub observed: Vec<String>,
      #[serde(default, skip_serializing_if = "Option::is_none")] pub observed_hash: Option<String>,
      #[serde(default, skip_serializing_if = "Option::is_none")] pub profile: Option<String>,
      #[serde(default, skip_serializing_if = "Option::is_none")] pub nixpkgs: Option<String>,
  }
  // on WorkspaceStatus:
  #[serde(default, skip_serializing_if = "Option::is_none")] pub packages: Option<PackagesStatus>,
  pub const PACKAGES_READY: &str = "PackagesReady";
  ```

- [ ] **Step 1: Write the failing test** (in `crd.rs` tests module)

```rust
#[test]
fn workspace_status_carries_packages_and_omits_it_when_unset() {
    let st = WorkspaceStatus::default();
    assert!(!serde_json::to_string(&st).unwrap().contains("packages"));
    let st = WorkspaceStatus {
        packages: Some(PackagesStatus { observed: vec!["go".into()], observed_hash: Some("sha256:x".into()), profile: None, nixpkgs: None }),
        ..Default::default()
    };
    let v = serde_json::to_value(&st).unwrap();
    assert_eq!(v["packages"]["observed"][0], "go");
    assert_eq!(v["packages"]["observedHash"], "sha256:x");
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p kloudlite-workspaces workspace_status_carries_packages` → compile error.

- [ ] **Step 3: Implement** — add `PackagesStatus` (doc comment: what the FILE says as of the last pass vs. what the profile on disk IS — `observed` is the list, `observed_hash` the idempotency key) and the `packages` field on `WorkspaceStatus`; `pub const PACKAGES_READY: &str = "PackagesReady";` next to the other condition names.

- [ ] **Step 4: Regenerate and test**

Run: `CRD_REGEN=1 cargo test -p kloudlite-workspaces --test crd_yaml && cargo test -p kloudlite-workspaces`
Expected: `deploy/k3s/crds.yaml` changes (Workspace status schema gains `packages`); all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/crd.rs deploy/k3s/crds.yaml
git commit -m "Report a workspace's package profile on its status"
```

---

### Task 3: The nix PV, claim and pod mounts

**Files:**
- Modify: `crates/workspaces/src/k8s.rs` (`local_pv` ~300, `claim` ~344, `claim_volume` ~420, `workspace_pod` ~549, tests ~1300)

**Interfaces:**
- Consumes: `packages::{PROFILE_MOUNT, path_env}` (Task 1).
- Produces:
  ```rust
  pub const NIX_ROOT: &str = "/nix";
  pub fn nix_pv_name(id: &str) -> String            // "nix-{id}"
  pub fn nix_claim_name(id: &str) -> String         // "nix-{id}"
  pub fn nix_pv(id: &str, owner: &str, ctx: &PodContext) -> PersistentVolume
  pub fn nix_claim(ns: &str, id: &str, owner: &str, owner_ref: &OwnerReference) -> PersistentVolumeClaim
  ```
  `workspace_pod` gains the two read-only mounts, the `nix` volume, and the env from the constraints.

- [ ] **Step 1: Write the failing tests** (k8s.rs tests module)

```rust
#[test]
fn a_workspace_pod_mounts_the_store_and_only_its_own_profile_read_only() {
    let p = workspace_pod(&ws_spec(), "ws-1", &ctx(), None);
    let c = &p.spec.as_ref().unwrap().containers[0];
    let mounts = c.volume_mounts.as_ref().unwrap();
    let store = mounts.iter().find(|m| m.mount_path == "/nix/store").expect("store mount");
    assert_eq!(store.read_only, Some(true));
    assert_eq!(store.sub_path.as_deref(), Some("store"));
    assert_eq!(store.name, "nix");
    let prof = mounts.iter().find(|m| m.mount_path == "/nix/profile").expect("profile mount");
    assert_eq!(prof.read_only, Some(true));
    assert_eq!(prof.sub_path.as_deref(), Some("var/kloudlite/profiles/ws-1"));
    assert!(!mounts.iter().any(|m| m.mount_path == "/nix"), "never the whole store tree: other profiles and the daemon socket live there");
    let env = c.env.as_ref().unwrap();
    let get = |k: &str| env.iter().find(|e| e.name == k).and_then(|e| e.value.clone()).unwrap();
    assert!(get("PATH").starts_with("/nix/profile/bin:"));
    assert_eq!(get("NIX_PROFILE"), "/nix/profile");
    let vols = p.spec.as_ref().unwrap().volumes.as_ref().unwrap();
    let nix = vols.iter().find(|v| v.name == "nix").unwrap();
    assert_eq!(nix.persistent_volume_claim.as_ref().unwrap().claim_name, "nix-ws-1");
    assert_eq!(nix.persistent_volume_claim.as_ref().unwrap().read_only, Some(true));
    assert!(vols.iter().all(|v| v.host_path.is_none()), "workspace pod must mount a claim, not a hostPath");
}

#[test]
fn the_nix_pv_is_read_only_and_pinned_to_the_node() {
    let pv = nix_pv("ws-1", "acme", &ctx());
    let spec = pv.spec.unwrap();
    assert_eq!(spec.local.as_ref().unwrap().path, "/nix");
    assert_eq!(spec.access_modes.as_deref(), Some(&["ReadOnlyMany".to_string()][..]));
    assert_eq!(spec.persistent_volume_reclaim_policy.as_deref(), Some("Retain"));
    let term = &spec.node_affinity.unwrap().required.unwrap().node_selector_terms[0];
    assert_eq!(term.match_expressions.as_ref().unwrap()[0].values.as_ref().unwrap()[0], ctx().node_name);
    let c = nix_claim("ws-acme", "ws-1", "acme", &owner_ref());
    let cs = c.spec.unwrap();
    assert_eq!(cs.volume_name.as_deref(), Some("nix-ws-1"));
    assert_eq!(cs.access_modes.as_deref(), Some(&["ReadOnlyMany".to_string()][..]));
}
```

(`ws_spec()`, `ctx()`, `owner_ref()` are the existing test helpers in that module — reuse them; if `ws_spec` is named differently there, use the helper the `a_workspace_pod_double_mounts_its_volume` test uses.)

- [ ] **Step 2: Run to verify they fail** — `cargo test -p kloudlite-workspaces k8s::tests` → compile errors (`nix_pv` missing).

- [ ] **Step 3: Implement**

```rust
/// The host Nix store, exposed to a workspace the same way its subvolume is: a local PV names the
/// host path, the pod names a claim. A local PV binds to exactly one claim, so it is one per
/// workspace even though every one of them points at the same `/nix` — PV objects are cheap and
/// the alternative is a hostPath, which PSA `baseline` forbids for good reason.
pub const NIX_ROOT: &str = "/nix";

pub fn nix_pv_name(id: &str) -> String { format!("nix-{id}") }
pub fn nix_claim_name(id: &str) -> String { format!("nix-{id}") }

pub fn nix_pv(id: &str, owner: &str, ctx: &PodContext) -> PersistentVolume {
    PersistentVolume {
        metadata: ObjectMeta {
            name: Some(nix_pv_name(id)),
            labels: Some(labels(owner, "volume")),
            owner_references: Some(vec![ctx.owner_ref.clone()]),
            ..Default::default()
        },
        spec: Some(PersistentVolumeSpec {
            // Capacity is a required field with no meaning here: the store is shared and read-only.
            capacity: Some(BTreeMap::from([("storage".to_string(), Quantity("1Gi".to_string()))])),
            access_modes: Some(vec!["ReadOnlyMany".to_string()]),
            persistent_volume_reclaim_policy: Some("Retain".to_string()),
            storage_class_name: Some(STORAGE_CLASS.to_string()),
            local: Some(LocalVolumeSource { path: NIX_ROOT.to_string(), ..Default::default() }),
            node_affinity: Some(VolumeNodeAffinity {
                required: Some(k8s_openapi::api::core::v1::NodeSelector {
                    node_selector_terms: vec![NodeSelectorTerm {
                        match_expressions: Some(vec![NodeSelectorRequirement {
                            key: "kubernetes.io/hostname".to_string(),
                            operator: "In".to_string(),
                            values: Some(vec![ctx.node_name.to_string()]),
                        }]),
                        ..Default::default()
                    }],
                }),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub fn nix_claim(ns: &str, id: &str, owner: &str, owner_ref: &OwnerReference) -> PersistentVolumeClaim {
    PersistentVolumeClaim {
        metadata: meta(&nix_claim_name(id), Some(ns), owner, "volume", owner_ref),
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadOnlyMany".to_string()]),
            storage_class_name: Some(STORAGE_CLASS.to_string()),
            volume_name: Some(nix_pv_name(id)),
            resources: Some(VolumeResourceRequirements {
                requests: Some(BTreeMap::from([("storage".to_string(), Quantity("1Gi".to_string()))])),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn nix_volume(id: &str) -> Volume {
    Volume {
        name: "nix".to_string(),
        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource { claim_name: nix_claim_name(id), read_only: Some(true) }),
        ..Default::default()
    }
}
```

In `workspace_pod`: append to `volume_mounts`
```rust
                // The store, and THIS workspace's profile only. Subpaths of one read-only claim:
                // `/nix` itself holds every other workspace's profile and the daemon socket.
                VolumeMount { name: "nix".to_string(), mount_path: "/nix/store".to_string(), sub_path: Some("store".to_string()), read_only: Some(true), ..Default::default() },
                VolumeMount { name: "nix".to_string(), mount_path: crate::packages::PROFILE_MOUNT.to_string(), sub_path: Some(format!("var/kloudlite/profiles/{id}")), read_only: Some(true), ..Default::default() },
```
replace `env: Some(vec![git_ssh_command()])` with
```rust
            env: Some(vec![
                git_ssh_command(),
                EnvVar { name: "PATH".into(), value: Some(crate::packages::path_env(None)), ..Default::default() },
                EnvVar { name: "NIX_PROFILE".into(), value: Some(crate::packages::PROFILE_MOUNT.into()), ..Default::default() },
                EnvVar { name: "MANPATH".into(), value: Some(format!("{}/share/man:", crate::packages::PROFILE_MOUNT)), ..Default::default() },
                EnvVar { name: "XDG_DATA_DIRS".into(), value: Some(format!("{}/share:/usr/local/share:/usr/share", crate::packages::PROFILE_MOUNT)), ..Default::default() },
            ]),
```
and `volumes: Some(vec![claim_volume(id), nix_volume(id), user_key_volume(init.is_some())])`.

`path_env(None)`: the image's PATH is not readable at apply time; the default covers Debian and Alpine images. `// ponytail: an image with a non-standard PATH loses it; read it from the image config via the registry if that ever matters.`

- [ ] **Step 4: Run** — `cargo test -p kloudlite-workspaces k8s && cargo clippy -p kloudlite-workspaces -- -D warnings`. Expected: all pass, including `a_workspace_pod_double_mounts_its_volume` (it counts claims named `live`; the new mounts are named `nix`).

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/k8s.rs
git commit -m "Mount the host Nix store and the workspace's own profile read-only in its pod"
```

---

### Task 3b: `spec.packages` on the CRD; drop the file parser

**Files:**
- Modify: `crates/workspaces/src/packages.rs` (remove `parse_file`, `FILE_NAME`, `MAX_FILE_BYTES`, `Packages`, the YAML deps; keep the rest), `crates/workspaces/Cargo.toml` + root `Cargo.toml` (remove `serde_yaml`), `crates/workspaces/src/crd.rs` (`WorkspaceSpec`)
- Regenerate: `deploy/k3s/crds.yaml`

**Interfaces:**
- Produces:
  ```rust
  // packages.rs
  pub enum PackageError { Attr(String), TooMany(usize), Duplicate(String) }   // Display as before
  pub fn validate_attr(s: &str) -> Result<(), PackageError>
  pub fn validate_list(list: &[String]) -> Result<(), PackageError>          // grammar + ≤100 + no duplicates
  pub fn hash(pin: &str, packages: &[String]) -> String
  pub fn expression(pin: &str, id: &str, packages: &[String]) -> String
  pub fn path_env(image_path: Option<&str>) -> String
  pub const PROFILE_MOUNT: &str = "/nix/profile";
  // crd.rs WorkspaceSpec
  #[serde(default, skip_serializing_if = "Vec::is_empty")] pub packages: Vec<String>,
  ```

- [ ] **Step 1: Tests** — in `packages.rs`, delete every `parse_file` test; keep grammar/hash/expression/path_env tests; add:
```rust
#[test]
fn a_list_is_validated_as_a_whole() {
    assert!(validate_list(&["hello".into(), "jq".into()]).is_ok());
    assert!(matches!(validate_list(&["hello".into(), "hello".into()]), Err(PackageError::Duplicate(_))));
    let many: Vec<String> = (0..101).map(|i| format!("p{i}")).collect();
    assert!(matches!(validate_list(&many), Err(PackageError::TooMany(101))));
    assert!(matches!(validate_list(&["$(id)".into()]), Err(PackageError::Attr(_))));
}
```
  in `crd.rs` tests: `WorkspaceSpec` round-trips `packages` and omits it when empty (mirror `workspace_status_carries_packages_and_omits_it_when_unset`).
- [ ] **Step 2: Run** — expect compile failures (`validate_list` missing).
- [ ] **Step 3: Implement** — rename `FileError`→`PackageError` (drop `TooLarge`, `Yaml`, `Pin`), remove the YAML parse and the pin validation, add `validate_list`, remove `serde_yaml` from both Cargo.toml files (`cargo update -p serde_yaml` is NOT needed — the lock keeps it for other deps), update the module doc (the list now arrives from the API/CR, validated twice because an object can be written by kubectl). Add `packages` to `WorkspaceSpec` with a doc comment (the truth; a clone copies it; a restore never touches it). Regenerate crds.yaml.
- [ ] **Step 4: Run** — `cargo test -p kloudlite-workspaces && cargo clippy -p kloudlite-workspaces -- -D warnings && CRD_REGEN=1 cargo test -p kloudlite-workspaces --test crd_yaml && cargo test -p kloudlite-workspaces --test crd_yaml`.
- [ ] **Step 5: Commit** — `git commit -m "Carry a workspace's package list on its spec"`.

---

### Task 4: The Nix runner### Task 4: The Nix runner (`bins/agent/src/nix.rs`)

**Files:**
- Create: `bins/agent/src/nix.rs`
- Modify: `bins/agent/src/lib.rs` (`pub mod nix;`, config + `Ctx.nix`), `bins/agent/src/controller.rs` (`Ctx` field only)

**Interfaces:**
- Consumes: `packages::expression`.
- Produces:
  ```rust
  pub const PROFILES_DIR: &str = "/nix/var/kloudlite/profiles";
  pub fn profile_path(id: &str) -> PathBuf            // PROFILES_DIR/{id}
  pub fn building_path(id: &str) -> PathBuf           // PROFILES_DIR/{id}.building
  pub trait Nix: Send + Sync {
      /// `nix build --expr <expr> -o <out_link>`; Ok(()) once the out-link exists.
      fn build(&self, expr: &str, out_link: &Path, timeout: Duration) -> Result<(), String>;
      /// `nix store ping`.
      fn ping(&self) -> Result<(), String>;
      /// `nix-collect-garbage`; returns bytes freed as nix reports them (0 if unparseable).
      fn collect_garbage(&self) -> Result<u64, String>;
  }
  pub struct RealNix { pub bin: PathBuf }              // /nix/var/nix/profiles/default/bin
  pub fn publish(id: &str) -> std::io::Result<()>      // rename building → profile
  pub fn remove_profile(id: &str) -> std::io::Result<()>  // both links, ignore NotFound
  pub fn profile_exists(id: &str) -> bool              // symlink exists AND its target exists
  pub fn nixpkgs_pin() -> String                        // WS_NIXPKGS, required at boot (lib.rs checks)
  pub fn build_timeout() -> Duration                    // WS_NIX_TIMEOUT secs, default 1200
  ```

- [ ] **Step 1: Write the failing tests** (in `nix.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The runner with the profile dir redirected: `PROFILES_DIR` is a const, so the fs helpers
    /// take a root. Tests pass a tempdir; production passes `PROFILES_DIR`.
    #[test]
    fn publish_renames_the_building_link_over_the_profile() {
        let dir = tempfile::tempdir().unwrap();
        let target_a = dir.path().join("a"); std::fs::create_dir(&target_a).unwrap();
        let target_b = dir.path().join("b"); std::fs::create_dir(&target_b).unwrap();
        std::os::unix::fs::symlink(&target_a, dir.path().join("ws-1")).unwrap();
        std::os::unix::fs::symlink(&target_b, dir.path().join("ws-1.building")).unwrap();
        publish_in(dir.path(), "ws-1").unwrap();
        assert_eq!(std::fs::read_link(dir.path().join("ws-1")).unwrap(), target_b);
        assert!(!dir.path().join("ws-1.building").exists());
    }

    #[test]
    fn a_dangling_profile_link_does_not_count_as_existing() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(dir.path().join("gone"), dir.path().join("ws-1")).unwrap();
        assert!(!profile_exists_in(dir.path(), "ws-1"));
        std::fs::create_dir(dir.path().join("gone")).unwrap();
        assert!(profile_exists_in(dir.path(), "ws-1"));
        remove_profile_in(dir.path(), "ws-1").unwrap();
        assert!(!dir.path().join("ws-1").exists());
        remove_profile_in(dir.path(), "ws-1").unwrap(); // idempotent
    }

    #[test]
    fn the_real_runner_execs_an_argv_with_no_shell() {
        // A fake `nix` that records its argv proves the expression travels as ONE argument.
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin"); std::fs::create_dir(&bin).unwrap();
        let log = dir.path().join("argv.log");
        std::fs::write(bin.join("nix"), format!("#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> {}; done\nln -s /tmp \"$4\" 2>/dev/null; exit 0\n", log.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("nix"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let nix = RealNix { bin: bin.clone() };
        let out = dir.path().join("out");
        nix.build("let x = \"$(id); rm -rf /\"; in x", &out, Duration::from_secs(5)).unwrap();
        let argv = std::fs::read_to_string(&log).unwrap();
        assert!(argv.contains("let x = \"$(id); rm -rf /\"; in x\n"), "the expression is one argv element: {argv}");
        assert!(argv.contains("--expr\n") && argv.contains("--no-link\n") == false);
    }

    #[test]
    fn a_build_that_outlives_its_deadline_is_an_error_not_a_hang() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin"); std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("nix"), "#!/bin/sh\nsleep 5\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("nix"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let nix = RealNix { bin };
        let started = std::time::Instant::now();
        let err = nix.build("1", &dir.path().join("out"), Duration::from_millis(300)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(err.contains("timed out"), "{err}");
    }
}
```

(`tempfile` is already a dev-dependency of the agent crate if `bins/agent/Cargo.toml` lists it; otherwise add `tempfile = "3"` under `[dev-dependencies]`.)

- [ ] **Step 2: Run to verify they fail** — `cargo test -p kloudlite-agent-bin nix::` → compile error.

- [ ] **Step 3: Implement**

```rust
//! The agent's one Nix client: builds a workspace's profile through the host daemon, publishes it
//! by rename, and collects garbage. Behind a trait so the reconciler is tested with a fake — a
//! real `nix` needs a daemon and a store, which a unit test must not.
//!
//! The binary comes from the HOST store (`/nix/var/nix/profiles/default/bin`, seeded by the
//! DaemonSet's init container from the `nixos/nix` image), not from the agent image: a `nix` that
//! lives outside the store it talks to cannot exist, and shipping a second store just to hold the
//! client is what the seed step avoids.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

pub const PROFILES_DIR: &str = "/nix/var/kloudlite/profiles";
const DEFAULT_TIMEOUT_SECS: u64 = 1200;

pub fn profile_path(id: &str) -> PathBuf { Path::new(PROFILES_DIR).join(id) }
pub fn building_path(id: &str) -> PathBuf { Path::new(PROFILES_DIR).join(format!("{id}.building")) }

pub fn nixpkgs_pin() -> String {
    std::env::var("WS_NIXPKGS").unwrap_or_default()
}

pub fn build_timeout() -> Duration {
    Duration::from_secs(std::env::var("WS_NIX_TIMEOUT").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_TIMEOUT_SECS))
}

pub trait Nix: Send + Sync {
    fn build(&self, expr: &str, out_link: &Path, timeout: Duration) -> Result<(), String>;
    fn ping(&self) -> Result<(), String>;
    fn collect_garbage(&self) -> Result<u64, String>;
}

pub struct RealNix {
    pub bin: PathBuf,
}

impl RealNix {
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(self.bin.join("nix"));
        c.args(args)
            .env("NIX_REMOTE", "daemon")
            .env("NIX_CONFIG", "experimental-features = nix-command flakes")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }

    /// Run with a deadline: `wait_timeout` is not in std, so poll `try_wait` at 200 ms. A kill on
    /// the deadline is what stops a stalled substituter holding the reconciler's blocking thread.
    fn run(&self, mut c: Command, timeout: Duration) -> Result<String, String> {
        let mut child = c.spawn().map_err(|e| format!("spawn nix: {e}"))?;
        let started = std::time::Instant::now();
        loop {
            match child.try_wait().map_err(|e| e.to_string())? {
                Some(status) => {
                    let out = child.wait_with_output().map_err(|e| e.to_string())?;
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    if status.success() {
                        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
                    }
                    // The last lines are the ones that name the attribute or the disk; the
                    // hundreds above them are download progress.
                    let tail: Vec<&str> = stderr.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect();
                    return Err(tail.join("\n"));
                }
                None if started.elapsed() > timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("nix timed out after {}s", timeout.as_secs()));
                }
                None => std::thread::sleep(Duration::from_millis(200)),
            }
        }
    }
}

impl Nix for RealNix {
    fn build(&self, expr: &str, out_link: &Path, timeout: Duration) -> Result<(), String> {
        // `--impure` for `builtins.getFlake` on a pinned ref; the expression is ONE argv element.
        let link = out_link.to_string_lossy().into_owned();
        let c = self.cmd(&["build", "--impure", "--expr", expr, "-o", &link]);
        self.run(c, timeout).map(|_| ())
    }
    fn ping(&self) -> Result<(), String> {
        self.run(self.cmd(&["store", "ping"]), Duration::from_secs(10)).map(|_| ())
    }
    fn collect_garbage(&self) -> Result<u64, String> {
        let mut c = Command::new(self.bin.join("nix-collect-garbage"));
        c.env("NIX_REMOTE", "daemon").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = self.run(c, Duration::from_secs(3600))?;
        // "… 1234567 bytes freed" — best effort; the number is only for the log line.
        Ok(out.split_whitespace().rev().skip_while(|w| *w != "bytes").nth(1).and_then(|n| n.parse().ok()).unwrap_or(0))
    }
}

pub fn publish(id: &str) -> std::io::Result<()> { publish_in(Path::new(PROFILES_DIR), id) }
pub fn remove_profile(id: &str) -> std::io::Result<()> { remove_profile_in(Path::new(PROFILES_DIR), id) }
pub fn profile_exists(id: &str) -> bool { profile_exists_in(Path::new(PROFILES_DIR), id) }

/// `rename` over the live link: atomic, and the pod's `/nix/profile` bind of the old target keeps
/// working until its next path lookup — which is how a running workspace gains a tool without a
/// restart.
pub fn publish_in(root: &Path, id: &str) -> std::io::Result<()> {
    std::fs::rename(root.join(format!("{id}.building")), root.join(id))
}

pub fn remove_profile_in(root: &Path, id: &str) -> std::io::Result<()> {
    for p in [root.join(id), root.join(format!("{id}.building"))] {
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// A link whose target is gone (a GC that ran with the root missing, a wiped store) is a missing
/// profile: mounting it would give the pod an empty `bin`.
pub fn profile_exists_in(root: &Path, id: &str) -> bool {
    std::fs::metadata(root.join(id)).is_ok()
}
```

In `bins/agent/src/lib.rs`: `pub mod nix;`; in the boot path, `let nix: Arc<dyn nix::Nix> = Arc::new(nix::RealNix { bin: "/nix/var/nix/profiles/default/bin".into() });`, refuse to start when `WS_NIXPKGS` is empty (same shape as the `NODE_NAME` check: `return Err("WS_NIXPKGS is required: the nixpkgs pin every profile on this node is built against".into())`), `std::fs::create_dir_all(nix::PROFILES_DIR)` (log a warning, not a failure, if `/nix` is absent — the daemon container seeds it). Add `pub nix: Arc<dyn nix::Nix>` to `Ctx` in `controller.rs` and wherever `Ctx` is constructed (lib.rs and the test helper in `bins/agent/tests/reconcile.rs` — give the test helper a `FakeNix`, defined in Task 5).

- [ ] **Step 4: Run** — `cargo test -p kloudlite-agent-bin nix:: && cargo clippy -p kloudlite-agent-bin -- -D warnings`. Expected: 4 pass. (The `reconcile.rs` tests fail to compile until Task 5 adds `nix` to the test `Ctx` — do that minimal edit here with a `FakeNix` that returns `Ok` for everything, and let Task 5 grow it.)

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/nix.rs bins/agent/src/lib.rs bins/agent/src/controller.rs bins/agent/tests/reconcile.rs bins/agent/Cargo.toml
git commit -m "Give the agent a Nix runner that builds and publishes profiles by rename"
```

---

### Task 5: The profile step in the Workspace reconciler

**Files:**
- Modify: `bins/agent/src/controller.rs` (`apply_workspace` after the `claim` ensure ~line 1226; `cleanup_volume` ~703; `Ctx` wake channel; workspace `Controller` `reconcile_on`)
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: Tasks 1–4 (`packages::{validate_list, hash, expression}`, `crd::{PackagesStatus, PACKAGES_READY}`, `nix::*`).
- Produces: `async fn ensure_profile(w, vol_id, gen, prev, ctx) -> Result<Option<Action>, ReconcileErr>` — `None` = profile is current, go on to the pod; `Some(action)` = wrote status, return it. `Ctx.wake_workspace: UnboundedSender<ObjectRef<crd::Workspace>>`.

- [ ] **Step 1: Write the failing tests** (`bins/agent/tests/reconcile.rs`; follow the file's mock-server pattern — `Route`, `rec.calls()`, `rec.sent(..)`, the existing `Ctx` test constructor)

```rust
/// A fake `Nix` that records what it was asked to build and answers as told.
struct FakeNix { builds: Mutex<Vec<(String, PathBuf)>>, answer: Mutex<Result<(), String>> }
impl kloudlite_agent::nix::Nix for FakeNix {
    fn build(&self, expr: &str, out: &Path, _: Duration) -> Result<(), String> {
        self.builds.lock().unwrap().push((expr.to_string(), out.to_path_buf()));
        let r = self.answer.lock().unwrap().clone();
        if r.is_ok() { std::fs::create_dir_all(out.parent().unwrap()).unwrap(); let _ = std::os::unix::fs::symlink("/tmp", out); }
        r
    }
    fn ping(&self) -> Result<(), String> { Ok(()) }
    fn collect_garbage(&self) -> Result<u64, String> { Ok(0) }
}

#[tokio::test]
async fn a_workspace_builds_its_profile_from_its_spec_before_its_pod() {
    let (ctx, rec, fake) = ws_ctx_with_nix().await;          // helper: mocked Ctx + FakeNix, WS_PROFILES_DIR → tempdir, WS_NIXPKGS set
    let ws = ready_workspace("ws-1", vec!["hello".into()]);  // Volume mock answers ready; spec.packages as given
    let _ = apply_workspace(&ws, &ctx).await.unwrap();
    let builds = fake.builds.lock().unwrap().clone();
    assert_eq!(builds.len(), 1);
    assert!(builds[0].0.contains("paths = [ pkgs.hello ];"), "{}", builds[0].0);
    assert!(builds[0].1.ends_with("ws-1.building"));
    let calls = rec.calls();
    let built = calls.iter().position(|c| c.contains("/status")).unwrap();
    let pod = calls.iter().position(|c| c.starts_with("POST") && c.contains("/pods")).unwrap();
    assert!(built < pod, "status (Building/Built) before the pod is created: {calls:?}");
    let st = rec.sent("PATCH", WS_STATUS)[..].last().unwrap().clone();
    assert_eq!(st["status"]["packages"]["observed"][0], "hello");
    assert_eq!(st["status"]["conditions"].as_array().unwrap().iter().find(|c| c["type"] == "PackagesReady").unwrap()["reason"], "Built");
}

#[tokio::test]
async fn a_matching_hash_and_present_link_skip_the_build() {
    let (ctx, _rec, fake) = ws_ctx_with_nix().await;
    let mut ws = ready_workspace("ws-1", vec!["hello".into()]);
    let pin = std::env::var("WS_NIXPKGS").unwrap();
    ws.status.as_mut().unwrap().packages = Some(kloudlite_workspaces::crd::PackagesStatus {
        observed: vec!["hello".into()],
        observed_hash: Some(kloudlite_workspaces::packages::hash(&pin, &["hello".into()])),
        profile: None, nixpkgs: Some(pin),
    });
    std::os::unix::fs::symlink("/tmp", kloudlite_agent::nix::profile_path("ws-1")).unwrap();
    let _ = apply_workspace(&ws, &ctx).await.unwrap();
    assert!(fake.builds.lock().unwrap().is_empty(), "nothing to build");
}

#[tokio::test]
async fn a_failed_build_keeps_the_old_profile_and_says_why() {
    let (ctx, rec, fake) = ws_ctx_with_nix().await;
    std::os::unix::fs::symlink("/tmp", kloudlite_agent::nix::profile_path("ws-1")).unwrap();
    *fake.answer.lock().unwrap() = Err("error: attribute 'nodejs_99' missing".into());
    let ws = ready_workspace("ws-1", vec!["nodejs_99".into()]);
    let _ = apply_workspace(&ws, &ctx).await.unwrap();
    let st = rec.sent("PATCH", WS_STATUS)[..].last().unwrap().clone();
    let c = st["status"]["conditions"].as_array().unwrap().iter().find(|c| c["type"] == "PackagesReady").unwrap().clone();
    assert_eq!(c["reason"], "BuildFailed");
    assert!(c["message"].as_str().unwrap().contains("nodejs_99"));
    assert!(kloudlite_agent::nix::profile_exists("ws-1"), "the previous profile is untouched");
    assert!(rec.calls().iter().any(|c| c.starts_with("POST") && c.contains("/pods")), "the pod still runs on the old profile");
}

#[tokio::test]
async fn an_invalid_spec_entry_never_reaches_nix() {
    let (ctx, rec, fake) = ws_ctx_with_nix().await;
    let ws = ready_workspace("ws-1", vec!["$(id)".into()]);   // written past the API, e.g. kubectl
    let _ = apply_workspace(&ws, &ctx).await.unwrap();
    assert!(fake.builds.lock().unwrap().is_empty());
    let st = rec.sent("PATCH", WS_STATUS)[..].last().unwrap().clone();
    assert_eq!(st["status"]["conditions"].as_array().unwrap().iter().find(|c| c["type"] == "PackagesReady").unwrap()["reason"], "BuildFailed");
    assert!(!rec.calls().iter().any(|c| c.starts_with("POST") && c.contains("/pods")), "no profile ever existed, so no pod");
}
```

Test plumbing this task adds: `ws_ctx_with_nix()` builds the usual mocked `Ctx` with `nix: Arc<FakeNix>` (returning the `Arc<FakeNix>` too), sets `WS_PROFILES_DIR` to a tempdir and `WS_NIXPKGS` to `github:NixOS/nixpkgs/` + 40 `a`s. `ready_workspace(id, packages)` returns a Workspace whose Volume mock answers `phase: ready` with `spec.packages` set; `WS_STATUS` is the status PATCH path constant.

- [ ] **Step 2: Run to verify they fail** — `cargo test -p kloudlite-agent-bin --test reconcile profile` → failures.

- [ ] **Step 3: Implement**

`nix.rs` (small addition for tests): `pub fn profiles_dir() -> PathBuf { std::env::var("WS_PROFILES_DIR").map(PathBuf::from).unwrap_or_else(|_| PROFILES_DIR.into()) }`, and make `profile_path`/`building_path`/`publish`/`remove_profile`/`profile_exists` go through it. (`// ponytail: an env override for the tests; a field on RealNix if a second caller appears.`)

`controller.rs`:

```rust
/// Bring this workspace's Nix profile up to date with `spec.packages`, and say so on status.
/// `None` means the profile is current and the pod may be (re)started; `Some(action)` means
/// status was written and the pass ends here — a build in flight, or a build that failed with
/// no profile to fall back on.
///
/// Runs on EVERY pass, which is what makes packages present after a restore, a clone, a move or
/// an agent restart: each of those arrives with a spec whose hash does not match the profile
/// this node has (or with no profile at all), and the pod is not applied until it does.
async fn ensure_profile(
    w: &crd::Workspace,
    id: &str,
    gen: i64,
    prev: &crd::WorkspaceStatus,
    ctx: &Arc<Ctx>,
) -> Result<Option<Action>, ReconcileErr> {
    use kloudlite_workspaces::packages;
    let uid = w.uid().unwrap_or_default();
    let key = format!("profile:{uid}");

    // A finished build: publish it and record what it is. A running one: say so and wait.
    let finished = {
        let mut running = ctx.running.lock().unwrap_or_else(|p| p.into_inner());
        match running.get(&key) {
            Some((_, h)) if h.is_finished() => running.remove(&key),
            Some(_) => {
                write_ws_status(w, packages_status(prev, prev.packages.clone(), "Building", "taking the profile through nix", false, gen), ctx).await?;
                return Ok(Some(Action::requeue(TICK)));
            }
            None => None,
        }
    };

    // Validated again here: the API validates, but an object can be written by kubectl or a
    // restored backup, and a name that is not an attribute must never reach an expression.
    if let Err(e) = packages::validate_list(&w.spec.packages) {
        let has = crate::nix::profile_exists(id);
        write_ws_status(w, packages_status(prev, prev.packages.clone(), "BuildFailed", &e.to_string(), has, gen), ctx).await?;
        // Only a spec edit fixes this, and that is an event.
        return Ok(if has { None } else { Some(Action::await_change()) });
    }
    let pin = crate::nix::nixpkgs_pin();
    let hash = packages::hash(&pin, &w.spec.packages);
    let observed = crd::PackagesStatus {
        observed: w.spec.packages.clone(),
        observed_hash: Some(hash.clone()),
        profile: Some(crate::nix::profile_path(id).to_string_lossy().into_owned()),
        nixpkgs: Some(pin.clone()),
    };

    if let Some((_, handle)) = finished {
        let outcome = handle.await.unwrap_or_else(|e| Err(format!("build panicked: {e}")));
        match outcome {
            Ok(_) => {
                tokio::task::spawn_blocking({ let id = id.to_string(); move || crate::nix::publish(&id) })
                    .await
                    .map_err(|e| ReconcileErr(format!("publish panicked: {e}")))?
                    .map_err(|e| ReconcileErr(format!("publish profile: {e}")))?;
            }
            Err(e) => {
                let has = crate::nix::profile_exists(id);
                write_ws_status(w, packages_status(prev, Some(observed), "BuildFailed", &e, has, gen), ctx).await?;
                return Ok(if has { None } else { Some(Action::requeue(RETRY)) });
            }
        }
    }

    let current = prev.packages.as_ref().and_then(|p| p.observed_hash.as_deref()) == Some(hash.as_str())
        && crate::nix::profile_exists(id);
    if current {
        return Ok(None);
    }
    // A fresh link on disk whose hash status does not yet record (the publish above, or a restart
    // between publish and status): record it without building again.
    if finished.is_some() && crate::nix::profile_exists(id) {
        write_ws_status(w, packages_status(prev, Some(observed), "Built", "profile is on disk", true, gen), ctx).await?;
        return Ok(None);
    }

    // Build, on its own thread: `nix` blocks for as long as the substituter takes.
    let expr = packages::expression(&pin, id, &w.spec.packages);
    let out = crate::nix::building_path(id);
    let nix = ctx.nix.clone();
    let timeout = crate::nix::build_timeout();
    let handle = tokio::task::spawn_blocking(move || {
        nix.build(&expr, &out, timeout).map(|()| Done { phase: crd::Phase::Ready, lineage_tip: None, restored_to: None })
    });
    let handle = wake_on_finish(handle, ctx.wake_workspace.clone(), kube::runtime::reflector::ObjectRef::<crd::Workspace>::new(&w.name_any()));
    ctx.running.lock().unwrap_or_else(|p| p.into_inner()).insert(key, (gen, handle));
    write_ws_status(w, packages_status(prev, Some(observed), "Building", "taking the profile through nix", crate::nix::profile_exists(id), gen), ctx).await?;
    Ok(Some(Action::requeue(TICK)))
}

/// Status for the packages step: phase stays what it was (a workspace building a profile is not
/// being CREATED), `observed_generation` stays unset (not converged), the `PackagesReady`
/// condition replaces any earlier one of its type.
fn packages_status(
    prev: &crd::WorkspaceStatus,
    packages: Option<crd::PackagesStatus>,
    reason: &str,
    message: &str,
    ready: bool,
    gen: i64,
) -> crd::WorkspaceStatus {
    let mut conditions: Vec<_> = prev.conditions.iter().filter(|c| c.type_ != crd::PACKAGES_READY).cloned().collect();
    conditions.push(crd::condition(crd::PACKAGES_READY, ready && reason == "Built", reason, message, gen));
    crd::WorkspaceStatus { observed_generation: None, packages, conditions, ..prev.clone() }
}
```

Wire it in `apply_workspace` right after the `k8s::claim` ensure and before `let pods: Api<Pod>`:
```rust
    ensure(&Api::<PersistentVolume>::all(ctx.client.clone()), &k8s::nix_pv(&id, &w.spec.owner, &pod_ctx)).await?;
    ensure(&Api::<PersistentVolumeClaim>::namespaced(ctx.client.clone(), &ns), &k8s::nix_claim(&ns, &id, &w.spec.owner, &owner_ref)).await?;
    if w.spec.desired_state == DesiredState::Running {
        if let Some(action) = ensure_profile(w, &id, gen, &prev, ctx).await? {
            return Ok(action);
        }
    }
```
and carry `packages: prev.packages.clone()` plus the `PackagesReady` condition into the final converged status (keep the packages condition: `conditions: { let mut c: Vec<_> = prev.conditions.iter().filter(|c| c.type_ == crd::PACKAGES_READY).cloned().collect(); c.push(crd::condition("Ready", …)); c }`). The converged return stays `await_change()` — a spec change is a generation event.

`Ctx`: add `pub wake_workspace: UnboundedSender<ObjectRef<crd::Workspace>>` next to `wake_volume`, its receiver into `wakes`, and `.reconcile_on(wake_stream(ws_wakes))` on the workspaces `Controller` (mirror the volumes one).

`cleanup_volume`: after `cleanup_local`, `crate::nix::remove_profile(&id)` (log, don't fail: a missing `/nix` on a node that never built is not an error).

`RETRY` already exists (60 s).

- [ ] **Step 4: Run** — `cargo test -p kloudlite-agent-bin && cargo clippy -p kloudlite-agent-bin -- -D warnings`.

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/controller.rs bins/agent/src/nix.rs bins/agent/tests/reconcile.rs
git commit -m "Build a workspace's Nix profile from its spec before its pod"
```

---

### Task 6: The host store and daemon in the agent DaemonSet

**Files:**
- Create: `deploy/k3s/nix-conf.yaml`
- Modify: `deploy/k3s/agent-daemonset.yaml`, `deploy/k3s/README.md` (apply line)

**Interfaces:**
- Produces: on every pool node, host `/nix` seeded with the `nixos/nix` image's store; `nix-daemon` listening on `/nix/var/nix/daemon-socket/socket`; the agent container with `/nix` mounted, `NIX_REMOTE=daemon`, `WS_NIXPKGS`, `WS_NIX_TIMEOUT`.

- [ ] **Step 1: The ConfigMap** — `deploy/k3s/nix-conf.yaml`

```yaml
# The daemon's whole configuration. Nothing here is user-tunable: substituters and keys are the
# one trust decision in the store, and they are ours.
apiVersion: v1
kind: ConfigMap
metadata:
  name: kloudlite-nix
  namespace: kube-system
data:
  nix.conf: |
    experimental-features = nix-command flakes
    substituters = https://cache.nixos.org
    trusted-public-keys = cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=
    max-jobs = 2
    cores = 2
    # The daemon keeps its own headroom: GC below 5 GB free, up to 20 GB.
    min-free = 5368709120
    max-free = 21474836480
    trusted-users = root
```

- [ ] **Step 2: The DaemonSet** — in `deploy/k3s/agent-daemonset.yaml`:

`initContainers` (before `containers`):
```yaml
      initContainers:
        # "Create a Nix store on the host": the image's own store is copied onto the host's /nix
        # once. After that both containers below mount the HOST store, and the `nix` the agent
        # runs is the one in it — a client that lives outside the store it talks to cannot exist.
        - name: seed-store
          image: nixos/nix:2.24.10
          command: ["/bin/sh", "-c", "if [ ! -e /host-nix/store ]; then cp -a /nix/. /host-nix/; fi; mkdir -p /host-nix/var/kloudlite/profiles"]
          volumeMounts:
            - name: nix
              mountPath: /host-nix
```
new container after `agent`:
```yaml
        - name: nix-daemon
          image: nixos/nix:2.24.10
          command: ["/nix/var/nix/profiles/default/bin/nix-daemon"]
          # Privileged for the build sandbox's user namespaces; the pod already is.
          securityContext:
            privileged: true
          volumeMounts:
            - name: nix
              mountPath: /nix
            - name: nix-conf
              mountPath: /etc/nix
          livenessProbe:
            exec:
              command: ["/nix/var/nix/profiles/default/bin/nix", "store", "ping"]
            initialDelaySeconds: 30
            periodSeconds: 60
```
`agent` container additions — env:
```yaml
            - name: NIX_REMOTE
              value: daemon
            # The nixpkgs every profile on this node is built against. Rolling it rebuilds every
            # workspace's profile on its next pass (a cache download, not a compile).
            - name: WS_NIXPKGS
              value: github:NixOS/nixpkgs/<fill with the nixpkgs-unstable rev current on the day; 40 hex>
            - name: WS_NIX_TIMEOUT
              value: "1200"
```
volumeMounts: `- name: nix\n  mountPath: /nix` and `- name: nix-conf\n  mountPath: /etc/nix`. volumes:
```yaml
        - name: nix
          hostPath:
            path: /nix
            type: DirectoryOrCreate
        - name: nix-conf
          configMap:
            name: kloudlite-nix
```
README apply line: add `-f nix-conf.yaml`.

- [ ] **Step 3: Verify on the dev cluster**

```bash
export KUBECONFIG=.local/k3s.yaml
kubectl apply -f deploy/k3s/nix-conf.yaml -f deploy/k3s/agent-daemonset.yaml   # image tag unchanged for now — this checks the daemon only
kubectl -n kube-system rollout status ds/kloudlite-agent --timeout=300s
P=$(kubectl -n kube-system get pods -l app=kloudlite-agent -o name | head -1)
kubectl -n kube-system exec $P -c nix-daemon -- /nix/var/nix/profiles/default/bin/nix store ping
kubectl -n kube-system exec $P -c agent -- /nix/var/nix/profiles/default/bin/nix build --impure --expr 'let pkgs = import (builtins.getFlake "github:NixOS/nixpkgs/<rev>") { }; in pkgs.hello' -o /nix/var/kloudlite/profiles/smoke && kubectl -n kube-system exec $P -c agent -- /nix/var/kloudlite/profiles/smoke/bin/hello
kubectl -n kube-system exec $P -c agent -- rm /nix/var/kloudlite/profiles/smoke
```
Expected: `Store URL: daemon` from ping; `Hello, world!` from the agent container through the daemon.

- [ ] **Step 4: Commit**

```bash
git add deploy/k3s/nix-conf.yaml deploy/k3s/agent-daemonset.yaml deploy/k3s/README.md
git commit -m "Run a Nix store and daemon on every pool node from the agent DaemonSet"
```

---

### Task 7: Janitor GC

**Files:**
- Modify: `bins/agent/src/lib.rs` (`spawn_janitor` ~158; boot passes `nix`)

**Interfaces:**
- Consumes: `Nix::collect_garbage`.
- Produces: `fn nix_store_bytes(root: &Path) -> u64` (du of `/nix/store`, best effort) and the GC call inside the janitor tick when it exceeds `WS_NIX_GC_HIGH_GB` (default 60).

- [ ] **Step 1: Write the failing test** (lib.rs tests)

```rust
#[test]
fn the_store_gc_threshold_reads_gigabytes_with_a_default() {
    std::env::remove_var("WS_NIX_GC_HIGH_GB");
    assert_eq!(nix_gc_high_bytes(), 60 * 1024 * 1024 * 1024);
    std::env::set_var("WS_NIX_GC_HIGH_GB", "5");
    assert_eq!(nix_gc_high_bytes(), 5 * 1024 * 1024 * 1024);
    std::env::remove_var("WS_NIX_GC_HIGH_GB");
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p kloudlite-agent-bin nix_gc_high` → compile error.

- [ ] **Step 3: Implement** — `spawn_janitor(engine, pool, nix: Arc<dyn nix::Nix>)`; after the existing sweeps:
```rust
            // The store is a per-node cache; the profile out-links are its only roots, so a GC is
            // always safe and the only question is when. Size by `du` of the store dir, best
            // effort — a wrong number costs an early or late GC, never data.
            let used = nix_store_bytes(std::path::Path::new("/nix/store"));
            if used > nix_gc_high_bytes() {
                match nix.collect_garbage() {
                    Ok(freed) => tracing::info!(used, freed, "agent: nix store over threshold, collected garbage"),
                    Err(e) => tracing::warn!(error = %e, "agent: nix-collect-garbage failed"),
                }
            }
```
`nix_gc_high_bytes()` reads `WS_NIX_GC_HIGH_GB` (default 60) × 2^30. `nix_store_bytes` walks the dir with `walkdir` if it is already a dependency, else a recursive `read_dir` summing `metadata().len()` (skip errors). `// ponytail: du of a 60 GB store every 10 min is real IO; `statvfs` of the /nix filesystem is the cheaper signal once /nix is its own mount.`

- [ ] **Step 4: Run** — `cargo test -p kloudlite-agent-bin && cargo clippy -p kloudlite-agent-bin -- -D warnings`.

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/lib.rs
git commit -m "Collect Nix garbage from the janitor when the store grows past its threshold"
```

---

### Task 8: API create/PATCH and web

**Files:**
- Modify: `crates/workspaces/src/model.rs` (`Workspace` ~47), `crates/workspaces/src/api.rs` (create handler, `ws_doc` ~383, new PATCH route)
- Modify: `web/apps/web/src/lib/api.ts` (`ApiWorkspace` ~634, `createWorkspace`, new `setWorkspacePackages`), `web/apps/web/src/components/app/workspace-list.tsx` (create dialog + row), `web/apps/web/src/app/(shell)/[owner]/(org)/workspaces/actions.ts` (new `setPackages` action)

**Interfaces:**
- Produces on the workspace doc:
  ```rust
  #[serde(default)] pub packages: Vec<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")] pub packages_status: Option<PackagesDoc>,
  pub struct PackagesDoc { pub ready: bool, pub reason: String, pub message: String }
  ```
  API: `POST /v1/workspaces` body gains `packages: Vec<String>` (default empty; 422 `{"error": "<PackageError Display>"}` on a bad entry); `PATCH /v1/workspaces/{id}` body `{ "packages": [...] }` → same validation → merge-patches `spec.packages` → 200 with the doc. TS: `packages: string[]; packages_status?: { ready: boolean; reason: string; message: string } | null;`, `setWorkspacePackages(token, id, packages: string[])`.

- [ ] **Step 1: Write the failing tests** (api.rs tests)

```rust
#[test]
fn a_workspace_doc_shows_the_spec_and_the_condition() {
    let mut w = ws_fixture();   // the module's existing Workspace fixture helper
    w.spec.packages = vec!["go".into()];
    w.status = Some(crd::WorkspaceStatus {
        conditions: vec![crd::condition(crd::PACKAGES_READY, false, "BuildFailed", "error: attribute 'jq2' missing", 3)],
        ..Default::default()
    });
    let d = ws_doc(&w, &Default::default());
    assert_eq!(d.packages, ["go"]);
    let ps = d.packages_status.unwrap();
    assert!(!ps.ready);
    assert_eq!(ps.reason, "BuildFailed");
    assert!(ps.message.contains("jq2"));
}
```
plus, in the api's HTTP tests (the file has an in-process router test pattern for create — reuse it): create with `packages: ["$(id)"]` → 422 whose body names `$(id)`; create with `["hello"]` → the created object's `spec.packages == ["hello"]`; PATCH `{ "packages": ["hello","jq"] }` → 200 and the doc echoes both.

- [ ] **Step 2: Run to verify they fail**.

- [ ] **Step 3: Implement** — `model.rs`: fields + `PackagesDoc`. `api.rs`: create request struct gains `#[serde(default)] packages: Vec<String>`; `packages::validate_list` → 422 on Err; written into `WorkspaceSpec.packages`. New handler `patch_workspace_packages` on `PATCH /v1/workspaces/{id}` (owner check via the same helper the other per-workspace handlers use; body `{ packages }`; validate; `Patch::Merge(json!({"spec": {"packages": list}}))` on the Workspace; return the refreshed doc). `ws_doc`: `packages: w.spec.packages.clone()`, `packages_status` from the `PACKAGES_READY` condition. Web: `api.ts` types + `setWorkspacePackages`; `actions.ts` `setPackages` server action (form field `packages` = whitespace/comma-separated text → array; `revalidatePath`); `workspace-list.tsx`: in the create dialog a `packages` Input (placeholder `hello jq nodejs_20`, hint "nixpkgs attribute names — search.nixos.org"); on each row, chips for `w.packages`, the `installing packages…` / `packages: BuildFailed` status with `title={message}`, and a small "Packages" dialog (Input prefilled with the current list, Apply → `setPackages`, closes on success via `useDialogUntilSuccess` — copy the sibling dialogs' shape).

- [ ] **Step 4: Run** — `cargo test -p kloudlite-workspaces && cargo clippy --workspace -- -D warnings && cd web && bunx tsc --noEmit -p apps/web/tsconfig.json && bun run lint`.

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/model.rs crates/workspaces/src/api.rs web/apps/web/src/lib/api.ts web/apps/web/src/components/app/workspace-list.tsx "web/apps/web/src/app/(shell)/[owner]/(org)/workspaces/actions.ts"
git commit -m "Let a workspace's packages be set at create and changed later"
```

---

### Task 9: e2e phase

**Files:**
- Modify: `tests/ws_e2e.sh` (after the seeded-workspace phase; uses `WS_NS`, `api` helper for curl, `fail`)

- [ ] **Step 1: Add the phase** (the script already has a curl helper with the JWT — use it as the other phases do)

```bash
# ── packages: spec.packages becomes tools on PATH, with no restart ─────────────────────────
api PATCH "/v1/workspaces/$WS_ID" '{"packages":["hello"]}' >/dev/null || fail "PATCH packages"
for i in $(seq 1 90); do
  kubectl get workspace "$WS_ID" -o jsonpath='{.status.conditions[?(@.type=="PackagesReady")].reason}' 2>/dev/null | grep -q '^Built$' && break
  sleep 2
  [ "$i" -eq 90 ] && fail "PackagesReady never became Built: $(kubectl get workspace "$WS_ID" -o jsonpath='{.status.conditions}')"
done
kubectl -n "$WS_NS" exec "$WS_ID" -- hello | grep -q 'Hello, world!' || fail "hello is not on PATH in the workspace pod"
POD_UID=$(kubectl -n "$WS_NS" get pod "$WS_ID" -o jsonpath='{.metadata.uid}')
api PATCH "/v1/workspaces/$WS_ID" '{"packages":["hello","jq"]}' >/dev/null || fail "PATCH packages"
for i in $(seq 1 90); do
  kubectl -n "$WS_NS" exec "$WS_ID" -- jq --version >/dev/null 2>&1 && break
  sleep 2
  [ "$i" -eq 90 ] && fail "jq did not appear after PATCH"
done
[ "$(kubectl -n "$WS_NS" get pod "$WS_ID" -o jsonpath='{.metadata.uid}')" = "$POD_UID" ] || fail "the pod was restarted to add a package; the profile swap must be live"
api PATCH "/v1/workspaces/$WS_ID" '{"packages":["jq"]}' >/dev/null || fail "PATCH packages"
for i in $(seq 1 90); do
  kubectl -n "$WS_NS" exec "$WS_ID" -- hello >/dev/null 2>&1 || break
  sleep 2
  [ "$i" -eq 90 ] && fail "hello is still on PATH after being removed"
done
api PATCH "/v1/workspaces/$WS_ID" '{"packages":["$(id)"]}' 2>/dev/null | grep -q 422 || fail "a bad attribute must be a 422"
```
Plus, in the existing clone phase after the clone is ready: `kubectl -n "$WS_NS" exec "$CLONE_ID" -- jq --version >/dev/null || fail "the clone did not build its profile from the copied spec"`.

- [ ] **Step 2: Run** — `./tests/ws_e2e.sh` on the Linux VM (exit 77 = prerequisite missing; not a pass).

- [ ] **Step 3: Commit** — `git commit -m "Exercise workspace packages end to end: spec to PATH, live swap, clone"`.

---

### Task 10: Rollout

- [ ] **Step 1** — `kubectl apply -f deploy/k3s/crds.yaml` (k3s). Additive.
- [ ] **Step 2** — push; wait for `image.yml`; pin `deploy/k3s/agent-daemonset.yaml` to the SHA; `kubectl apply -f deploy/k3s/nix-conf.yaml -f deploy/k3s/agent-rbac.yaml -f deploy/k3s/agent-daemonset.yaml`; `rollout status`. Watch `kubectl -n kube-system logs ds/kloudlite-agent -c agent` for `PackagesReady`; existing workspaces get an empty profile on their next pass and keep their current pods (`// ponytail:` in the spec: a pod created before this roll has no `/nix` until it is recreated).
- [ ] **Step 3** — push the web/api change; pin `deploy/kloudlite.yaml` + `deploy/kloudlite-web.yaml`; apply.
- [ ] **Step 4** — Prove it in production: a repo with `kloudlite.yaml` → workspace → `hello`. Screenshot the row showing the chips.

---

## Self-review

- **Spec coverage:** grammar + list validation (T1, T3b); spec field + status + condition (T2, T3b, T5); PV/claim/mounts/env (T3); runner, argv-not-shell, deadline, publish-by-rename, remove on delete (T4, T5); rebuild on every pass / restore / clone / move (T5 — `ensure_profile` runs each pass and keys on hash + link presence); host store + daemon + seed + conf (T6); GC (T7); API create/PATCH + web (T8); e2e incl. live swap and clone (T9); rollout order (T10). A spec change is a generation event, so the converged path keeps `await_change()`.
- **Placeholders:** the `WS_NIXPKGS` rev in T6 is deliberately "fill with today's rev" — the implementer must pick a concrete 40-hex nixpkgs-unstable commit at execution time and record it in the commit message.
- **Type consistency:** `PackagesStatus{observed, observed_hash, profile, nixpkgs}` (T2) is what T5 writes and T8 reads; `PACKAGES_READY` const (T2) used in T5/T8; `WorkspaceSpec.packages` (T3b) read by T5/T8; `nix::{profile_path, building_path, publish, remove_profile, profile_exists, nixpkgs_pin, build_timeout, Nix, RealNix}` (T4) used in T5/T7; `packages::{PROFILE_MOUNT, validate_list, hash, expression, path_env}` (T1/T3b) used in T3/T5/T8.
