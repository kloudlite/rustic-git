# Phase 2: Direct Container Runtime — Implementation Plan

> **For agentic workers:** execute this with `superpowers:subagent-driven-development` — one
> subagent per `### Task N`, in order. Each task's **Interfaces** block is the contract other
> tasks compile against; do not change a published signature without updating this file.

**Goal.** Delete `docker compose` and the `docker` CLI from the agent. Control the Docker
daemon directly through `bollard`, and make container **labels** the only on-node source of
truth, so `down`/`delete` need no local state at all. This closes audit M1 (EnvDelete leaks
`{pool}/env/{id}` and is non-idempotent), the mount-source half of C1 (`{"folder":"/"}`
bind-mounts the host root), and the env side of H2 (idempotent job replay).

**Architecture.** `crates/workspaces/src/engine/compose.rs` and `bins/agent/src/container.rs`
collapse into one module, `bins/agent/src/runtime/`. `spec.rs` projects an `Environment` (or a
`Workspace`, which is an environment with one service and a fixed mount pair) into
`Vec<ContainerSpec>` and hashes each. `reconcile.rs` diffs that desired set against the
daemon's labelled containers — matching hash means leave alone, differing hash means recreate,
missing means create. `net.rs` owns one user-defined bridge per environment, giving
service-to-service DNS via network aliases. No file is written to the pool disk for any of it.
The `Environment` doc in Cosmos stays the single authority; the daemon holds a labelled
projection; reconciliation compares the two. There is no third copy.

**Tech Stack.** Rust 2021, `bollard` (unix socket, `/var/run/docker.sock`), `sha2` for spec
hashing, `serde_yml` for the API-boundary YAML input format (`serde_yaml` is archived,
RUSTSEC-2024-0320), `tokio` (the agent job loop is already async).

**Spec:** `docs/superpowers/specs/2026-08-25-direct-container-runtime-design.md` — read it
before starting. This plan implements it; it does not re-derive it.

---

## Global Constraints

- `cargo clippy --workspace -- -D warnings` clean at every commit. CI (`image.yml` test job)
  gates on it.
- `cargo test` passes at every commit. Tests needing a live daemon are `#[ignore]`d and named
  in the task so `cargo test -- --ignored` on a docker host runs them.
- **Container names stay byte-identical**: `env-{id}-{service}-1`, `ws-{id}`. Runbooks
  (`sudo docker exec env-{id}-db-1 mongosh ...`) and `tests/ws_e2e.sh` keep working. Names are
  a human affordance only — every lookup goes through labels, never names.
- The agent runs **as root on a btrfs host** and reaches the daemon via
  `/var/run/docker.sock`. No rootless, no Podman, no TCP endpoint.
- Comments explain **WHY**, never what; match the density of `bins/server/src/router/route.rs`.
- Deliberate shortcuts carry `// ponytail: <ceiling and upgrade path>`. The compose-label
  migration branch is one of them and must name its removal trigger.
- Commit subjects are imperative sentence case, no tool attribution.

---

## File Structure

**Created**

| Path | Responsibility |
|---|---|
| `bins/agent/src/runtime/mod.rs` | Daemon connection + version check, label constants, `RtErr` → `EngErr` mapping, `re-export`s. |
| `bins/agent/src/runtime/spec.rs` | `ContainerSpec`, `Environment`/`Workspace` → `Vec<ContainerSpec>`, `mount_source` (the C1 chokepoint), `spec_hash`. |
| `bins/agent/src/runtime/reconcile.rs` | `up` / `down` / `delete` / `start` / `stop` / `is_running` / `list_ours`, all label-driven. |
| `bins/agent/src/runtime/net.rs` | `ensure_network` / `remove_network` for `kloudlite-git-{env_id}`. |
| `bins/agent/tests/runtime_daemon.rs` | `#[ignore]`d integration tests against a real daemon. |

**Modified**

| Path | Change |
|---|---|
| `bins/agent/Cargo.toml` | add `bollard`, `sha2`, `futures`; dev-dep `tempfile` already present. |
| `Cargo.toml` (workspace) | add `bollard` and `serde_yml` to `[workspace.dependencies]`; drop `serde_yaml`. |
| `bins/agent/src/lib.rs` | `mod runtime;` replaces `mod container;`; every `JobKind::Env*` / `Ws*` arm calls `runtime::*`; `env_dir` becomes delete-only; janitor gains `janitor_containers`. |
| `crates/workspaces/src/model.rs` | `Service` gains `ports: Vec<PortMap>`; new `PortMap`; `Environment` gains `published: Vec<Published>`. |
| `crates/workspaces/src/api.rs` | mount + port validation at write time; `POST /v1/environments` accepts `Content-Type: application/yaml`. |
| `crates/workspaces/Cargo.toml` | `serde_yaml` → `serde_yml`. |
| `tests/ws_e2e.sh` | two-service DNS assertion, published-port assertion, compose-dir-gone assertion. |

**Deleted**

| Path | Why |
|---|---|
| `crates/workspaces/src/engine/compose.rs` | Replaced wholesale by `runtime/`. |
| `bins/agent/src/container.rs` | Same; a workspace is a one-service environment. |
| `bins/agent/src/lib.rs`: `compose()`, `docker_stop_name()`, `docker_start_name()` | CLI shell-outs with no callers left. |

---

### Task 1: Dependency and daemon connection

**Files:** `Cargo.toml`, `bins/agent/Cargo.toml`, `bins/agent/src/runtime/mod.rs`

> bollard is **not vendored** in `~/.cargo/registry` on this machine, so every call below is
> written against the **bollard 0.17** API surface (`bollard::Docker`,
> `bollard::container::*Options` structs, `bollard::secret::*` models). If `cargo add` resolves
> a different major, reconcile the option-struct paths before writing code — the shapes moved
> between 0.16 and 0.17.

**Interfaces**

Consumes: nothing.
Produces:

```rust
// bins/agent/src/runtime/mod.rs
pub const L_OWNER: &str = "kloudlite-git.owner";
pub const L_KIND: &str = "kloudlite-git.kind";
pub const L_ID: &str = "kloudlite-git.id";
pub const L_SERVICE: &str = "kloudlite-git.service";
pub const L_SPEC: &str = "kloudlite-git.spec";

/// Minimum daemon API we are willing to talk. `create_network` with `EndpointSettings.aliases`
/// and label filters on `list_containers` both predate this comfortably; the pin exists so a
/// too-old daemon fails at agent STARTUP with a readable message rather than at the first job,
/// where it would look like a job bug.
pub const MIN_API: &str = "1.41";

pub async fn connect() -> Result<Docker, EngErr>;
pub fn err(what: &str, e: bollard::errors::Error) -> EngErr;
/// True when the daemon said "no such container/network" — the tolerance every teardown path
/// needs, replacing the `stderr.contains("No such container")` string match in container.rs.
pub fn is_not_found(e: &bollard::errors::Error) -> bool;
```

- [ ] **Step 1:** Add deps. `bollard = "0.17"` (default features; the unix-socket transport is
      default on unix) and `serde_yml = "0.0.12"` to `[workspace.dependencies]` in the root
      `Cargo.toml`; remove `serde_yaml`. In `bins/agent/Cargo.toml` add
      `bollard = { workspace = true }`, `sha2 = { workspace = true }`,
      `futures = { workspace = true }`. In `crates/workspaces/Cargo.toml` swap
      `serde_yaml = { workspace = true }` for `serde_yml = { workspace = true }` and fix the one
      call site in `compose.rs` — or skip it, since Task 6 deletes that file; if `compose.rs`
      still compiles at this point just leave `serde_yaml` in the workspace table until Task 6.
      Run `cargo build -p kloudlite-git-agent-bin`.
      Commit: `git commit -am "Add bollard and serde_yml, drop serde_yaml"`

- [ ] **Step 2 (failing test):** In `bins/agent/src/runtime/mod.rs` write the module with the
      label constants and a test that only asserts the constant values (this is the contract
      other tasks and every runbook `docker ps --filter` command depend on, so it is worth
      pinning):

      ```rust
      #[cfg(test)]
      mod tests {
          use super::*;

          #[test]
          fn label_names_are_the_documented_contract() {
              assert_eq!(L_OWNER, "kloudlite-git.owner");
              assert_eq!(L_KIND, "kloudlite-git.kind");
              assert_eq!(L_ID, "kloudlite-git.id");
              assert_eq!(L_SERVICE, "kloudlite-git.service");
              assert_eq!(L_SPEC, "kloudlite-git.spec");
          }
      }
      ```

      Add `mod runtime;` to `bins/agent/src/lib.rs`.
      Run: `cargo test -p kloudlite-git-agent-bin runtime::` — fails to compile (no module yet).

- [ ] **Step 3 (implement):**

      ```rust
      //! Direct Docker daemon control. Container LABELS are the only source of truth on this
      //! node: `down`/`delete` are label queries, so they are correct on a fresh agent, after a
      //! pool wipe, and on a retry after a partial failure — the class of teardown bug that came
      //! from depending on a rendered compose file stops being expressible.

      use bollard::Docker;
      use kloudlite_git_workspaces::engine::EngErr;

      pub mod net;
      pub mod reconcile;
      pub mod spec;

      pub const L_OWNER: &str = "kloudlite-git.owner";
      pub const L_KIND: &str = "kloudlite-git.kind";
      pub const L_ID: &str = "kloudlite-git.id";
      pub const L_SERVICE: &str = "kloudlite-git.service";
      pub const L_SPEC: &str = "kloudlite-git.spec";

      pub const MIN_API: &str = "1.41";

      /// The agent runs as root on the btrfs host, so the socket is always readable and always
      /// local — no TCP, no TLS, no rootless socket discovery.
      pub async fn connect() -> Result<Docker, EngErr> {
          let d = Docker::connect_with_unix(
              "/var/run/docker.sock",
              120,
              bollard::API_DEFAULT_VERSION,
          )
          .map_err(|e| err("connect /var/run/docker.sock", e))?;
          let v = d.version().await.map_err(|e| err("daemon version", e))?;
          // Fail here, at startup, with the daemon's own number in the message. A version-too-old
          // failure surfacing at the first EnvUp instead reads as a job bug for an hour.
          match v.api_version.as_deref() {
              Some(api) if api >= MIN_API => Ok(d),
              other => Err(EngErr(format!(
                  "docker daemon API {} is below the required {MIN_API}",
                  other.unwrap_or("unknown")
              ))),
          }
      }

      pub fn err(what: &str, e: bollard::errors::Error) -> EngErr {
          EngErr(format!("{what}: {e}"))
      }

      /// Absent == already gone, not an error: every teardown path is retried, and a container a
      /// previous attempt already removed must not fail the retry.
      pub fn is_not_found(e: &bollard::errors::Error) -> bool {
          matches!(e, bollard::errors::Error::DockerResponseServerError { status_code: 404, .. })
      }
      ```

      Note the string compare on `api_version` is lexical and therefore only correct for
      same-width `1.NN` strings; that is every version Docker has ever shipped.
      `// ponytail: lexical version compare, fine while the daemon stays on 1.NN`.

- [ ] **Step 4:** Run `cargo test -p kloudlite-git-agent-bin runtime::` — passes.
      `cargo clippy --workspace -- -D warnings` — clean.
      Commit: `git commit -am "Add the runtime module skeleton with label constants and a daemon version pin"`

---

### Task 2: Spec construction and the C1 mount chokepoint

**Files:** `bins/agent/src/runtime/spec.rs`

This is the security task. Everything else waits on `mount_source`.

**Interfaces**

Consumes: `runtime::{L_OWNER, L_KIND, L_ID, L_SERVICE, L_SPEC}`,
`kloudlite_git_workspaces::model::{Environment, Service, Mount, PortMap}` (Task 5 adds `PortMap`;
until then, code the `ports` field as `Vec<PortMap>` and land Task 5's model change first if
you prefer a compiling order — the plan orders Task 5 after because its API work depends on
this validator).
Produces:

```rust
// bins/agent/src/runtime/spec.rs
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortSpec {
    pub container: u16,
    pub host: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerSpec {
    pub name: String,
    pub service: String,
    pub image: String,
    pub command: Vec<String>,
    /// BTreeMap, not HashMap: the hash is over this, and HashMap iteration order would make the
    /// same service hash differently on every process start, recreating every container on
    /// every `up`.
    pub env: std::collections::BTreeMap<String, String>,
    /// Already-validated `source:target[:ro]` strings. Sources are built only by `mount_source`.
    pub binds: Vec<String>,
    pub ports: Vec<PortSpec>,
    pub labels: std::collections::BTreeMap<String, String>,
}

/// The ONE place a bind source is constructed. Never accepts a caller-supplied path.
pub fn mount_source(live: &std::path::Path, folder: &str) -> Result<std::path::PathBuf, EngErr>;
pub fn spec_hash(s: &ContainerSpec) -> String;
pub fn env_specs(env: &Environment, live: &std::path::Path) -> Result<Vec<ContainerSpec>, EngErr>;
pub fn ws_spec(ws_id: &str, owner: &str, image: &str, live: &std::path::Path) -> Result<ContainerSpec, EngErr>;
pub fn env_container_name(env_id: &str, service: &str) -> String; // env-{id}-{service}-1
pub fn ws_container_name(ws_id: &str) -> String;                  // ws-{id}
```

- [ ] **Step 1 (failing test — the C1 regression test):** Create `spec.rs` containing only the
      test module below. This test must exist before the code ships; it is the audit's one
      critical finding.

      ```rust
      #[cfg(test)]
      mod tests {
          use super::*;
          use std::path::Path;

          /// C1 regression. `Path::join` DISCARDS the base for an absolute component, so the old
          /// `live.join("volumes").join(&m.folder)` turned `{"folder":"/"}` into a bind of the
          /// host root into a user-chosen image on a root agent. Nothing here may ever escape
          /// `live/volumes/`.
          #[test]
          fn mount_source_rejects_every_escape() {
              let live = Path::new("/pool/vol/env-1/live");
              for bad in ["/", "..", "a/b", "", ".", "../etc", "/etc/passwd", "a:b", "data:ro", "  ", "d\u{0}t"] {
                  assert!(mount_source(live, bad).is_err(), "accepted {bad:?}");
              }
          }

          #[test]
          fn mount_source_accepts_a_plain_segment_and_stays_under_live() {
              let live = Path::new("/pool/vol/env-1/live");
              let got = mount_source(live, "data").unwrap();
              assert_eq!(got, Path::new("/pool/vol/env-1/live/volumes/data"));
              assert!(got.starts_with(live));
          }
      }
      ```

- [ ] **Step 2:** Run `cargo test -p kloudlite-git-agent-bin spec::` — fails to compile.

- [ ] **Step 3 (implement `mount_source`):**

      ```rust
      /// The runtime layer builds mount sources from VALIDATED SEGMENTS ONLY; it never accepts a
      /// caller-supplied path. `valid_segment` already rejects `.`, `..`, `/` and anything
      /// outside `[A-Za-z0-9._-]`, which covers `:` too — and `:` matters separately here
      /// because a bind string is colon-delimited, so a colon-bearing folder would let a caller
      /// append their own `:target:ro` field.
      pub fn mount_source(live: &Path, folder: &str) -> Result<PathBuf, EngErr> {
          if !kloudlite_git_storage::store::valid_segment(folder) {
              return Err(EngErr(format!("invalid volume folder {folder:?}")));
          }
          Ok(live.join("volumes").join(folder))
      }

      /// A mount TARGET is a path inside the container, so it cannot be a segment — but it must
      /// be absolute (a relative target silently resolves against the image's WORKDIR) and free
      /// of `:` (the bind string's own delimiter).
      pub fn mount_target(path: &str) -> Result<&str, EngErr> {
          if !path.starts_with('/') || path.contains(':') {
              return Err(EngErr(format!("mount path must be absolute and contain no ':': {path:?}")));
          }
          Ok(path)
      }
      ```

      `bins/agent/Cargo.toml` needs `kloudlite-git-storage = { path = "../../crates/storage" }`
      (it reaches it transitively today; make it direct).

- [ ] **Step 4:** Run `cargo test -p kloudlite-git-agent-bin spec::` — passes.
      Commit: `git commit -am "Build bind sources only from validated segments"`

- [ ] **Step 5 (failing test — hashing):**

      ```rust
      fn sample() -> ContainerSpec {
          ContainerSpec {
              name: "env-e1-db-1".into(),
              service: "db".into(),
              image: "mongo:7".into(),
              command: vec![],
              env: [("A".to_string(), "1".to_string())].into_iter().collect(),
              binds: vec!["/pool/vol/env-e1/live/volumes/data:/data/db".into()],
              ports: vec![PortSpec { container: 27017, host: None }],
              labels: Default::default(),
          }
      }

      #[test]
      fn spec_hash_is_stable_and_field_sensitive() {
          let a = sample();
          assert_eq!(spec_hash(&a), spec_hash(&a.clone()));
          let mut b = a.clone();
          b.image = "mongo:8".into();
          assert_ne!(spec_hash(&a), spec_hash(&b));
          let mut c = a.clone();
          c.env.insert("B".into(), "2".into());
          assert_ne!(spec_hash(&a), spec_hash(&c));
          let mut d = a.clone();
          d.ports[0].host = Some(8080);
          assert_ne!(spec_hash(&a), spec_hash(&d));
          // Labels are DERIVED from the spec (the spec label is the hash itself), so they must
          // not feed the hash — otherwise no fixed point exists.
          let mut e = a.clone();
          e.labels.insert("kloudlite-git.spec".into(), "whatever".into());
          assert_eq!(spec_hash(&a), spec_hash(&e));
      }
      ```

- [ ] **Step 6:** Run `cargo test -p kloudlite-git-agent-bin spec::` — fails.

- [ ] **Step 7 (implement):**

      ```rust
      /// Drives recreate-vs-leave-alone. Fed by hand rather than by serializing the struct so
      /// that adding a field is a deliberate decision about whether it should cause a restart —
      /// and so `labels` can be excluded (the spec label IS this hash).
      pub fn spec_hash(s: &ContainerSpec) -> String {
          use sha2::{Digest, Sha256};
          let mut h = Sha256::new();
          h.update(&s.image);
          h.update([0]);
          for a in &s.command {
              h.update(a);
              h.update([0]);
          }
          h.update([1]);
          for (k, v) in &s.env {
              h.update(k);
              h.update([0]);
              h.update(v);
              h.update([0]);
          }
          h.update([2]);
          for b in &s.binds {
              h.update(b);
              h.update([0]);
          }
          h.update([3]);
          for p in &s.ports {
              h.update(p.container.to_le_bytes());
              h.update(p.host.unwrap_or(0).to_le_bytes());
              h.update([u8::from(p.host.is_some())]);
          }
          format!("{:x}", h.finalize())
      }
      ```

- [ ] **Step 8:** Run — passes. Commit:
      `git commit -am "Hash a container spec over the fields that require a recreate"`

- [ ] **Step 9 (failing test — projection and naming):**

      ```rust
      #[test]
      fn env_specs_name_containers_exactly_like_compose_did() {
          let env = env_fixture(); // services: "db" (mongo:7, mounts data:/data/db), "app"
          let live = Path::new("/pool/vol/env-e1/live");
          let specs = env_specs(&env, live).unwrap();
          assert_eq!(specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["env-e1-app-1", "env-e1-db-1"]);
          let db = specs.iter().find(|s| s.service == "db").unwrap();
          assert_eq!(db.binds, ["/pool/vol/env-e1/live/volumes/data:/data/db"]);
          assert_eq!(db.labels["kloudlite-git.id"], "env-e1");
          assert_eq!(db.labels["kloudlite-git.kind"], "env");
          assert_eq!(db.labels["kloudlite-git.service"], "db");
          assert_eq!(db.labels["kloudlite-git.owner"], env.owner);
          assert_eq!(db.labels["kloudlite-git.spec"], spec_hash(db));
      }

      #[test]
      fn env_specs_reject_an_escaping_mount() {
          let mut env = env_fixture();
          env.services[0].mounts[0].folder = "/".into();
          assert!(env_specs(&env, Path::new("/pool/vol/env-e1/live")).is_err());
      }

      #[test]
      fn ws_spec_keeps_the_double_bind_and_the_ws_name() {
          let s = ws_spec("w1", "alice", "nginx:alpine", Path::new("/pool/vol/w1/live")).unwrap();
          assert_eq!(s.name, "ws-w1");
          assert_eq!(s.labels["kloudlite-git.kind"], "ws");
          assert_eq!(s.binds, [
              "/pool/vol/w1/live:/workspace",
              "/pool/vol/w1/live:/usr/share/nginx/html:ro",
          ]);
      }
      ```

- [ ] **Step 10:** Run — fails.

- [ ] **Step 11 (implement):** `env_specs` sorts services by name (deterministic order keeps the
      hash-diff and the test stable), calls `mount_source`/`mount_target` per mount, builds the
      label map, then sets `L_SPEC` to `spec_hash(&spec)` last. `ws_spec` builds the same struct
      with `kind=ws`, `service="ws"`, no ports, and the two binds from `container.rs`'s comment —
      carry that comment forward verbatim, it explains WHY the double bind exists (the read-only
      nginx web root is what makes the default image serve the workspace's own files with zero
      configuration).

- [ ] **Step 12:** Run — passes. `cargo clippy --workspace -- -D warnings`.
      Commit: `git commit -am "Project environments and workspaces into labelled container specs"`

---

### Task 3: Per-environment network

**Files:** `bins/agent/src/runtime/net.rs`, `bins/agent/tests/runtime_daemon.rs`

**Interfaces**

Consumes: `runtime::{connect, err, is_not_found, L_ID, L_OWNER}`.
Produces:

```rust
/// `kloudlite-git-{env_id}`, deliberately NOT `env-{id}`: a compose project network from the
/// implementation this replaces is already called `env-{id}_default`-ish, and during the
/// migration window both may exist on the same host.
pub fn net_name(env_id: &str) -> String;
pub async fn ensure_network(d: &Docker, env_id: &str, owner: &str) -> Result<(), EngErr>;
pub async fn remove_network(d: &Docker, env_id: &str) -> Result<(), EngErr>;
```

- [ ] **Step 1 (failing test):**

      ```rust
      #[test]
      fn net_name_cannot_collide_with_a_compose_project_network() {
          assert_eq!(net_name("e1"), "kloudlite-git-e1");
          assert_ne!(net_name("e1"), "env-e1");
      }
      ```
      Run: `cargo test -p kloudlite-git-agent-bin net::` — fails.

- [ ] **Step 2 (implement):**

      ```rust
      use bollard::network::{CreateNetworkOptions, InspectNetworkOptions};

      pub fn net_name(env_id: &str) -> String {
          format!("kloudlite-git-{env_id}")
      }

      /// Idempotent. Create-then-tolerate-409 rather than inspect-then-create: two jobs for the
      /// same environment can overlap, and the check-then-act version loses that race.
      pub async fn ensure_network(d: &Docker, env_id: &str, owner: &str) -> Result<(), EngErr> {
          let name = net_name(env_id);
          let labels: HashMap<&str, &str> =
              [(super::L_ID, env_id), (super::L_OWNER, owner), (super::L_KIND, "net")].into();
          let res = d
              .create_network(CreateNetworkOptions { name: name.as_str(), driver: "bridge", labels, ..Default::default() })
              .await;
          match res {
              Ok(_) => Ok(()),
              Err(bollard::errors::Error::DockerResponseServerError { status_code: 409, .. }) => Ok(()),
              Err(e) => Err(super::err(&format!("create network {name}"), e)),
          }
      }

      /// Absent network == already removed. A network with containers still attached returns 403;
      /// that IS an error worth surfacing, because it means `delete` removed containers it did
      /// not know about — or did not remove them at all.
      pub async fn remove_network(d: &Docker, env_id: &str) -> Result<(), EngErr> {
          let name = net_name(env_id);
          match d.remove_network(&name).await {
              Ok(()) => Ok(()),
              Err(e) if super::is_not_found(&e) => Ok(()),
              Err(e) => Err(super::err(&format!("remove network {name}"), e)),
          }
      }
      ```

- [ ] **Step 3:** Run — passes. Commit:
      `git commit -am "Add a per-environment bridge network for service-to-service DNS"`

---

### Task 4: Reconcile — up / down / delete / start / stop

**Files:** `bins/agent/src/runtime/reconcile.rs`, `bins/agent/tests/runtime_daemon.rs`

This is the ~250-line core. Everything is a label query.

**Interfaces**

Consumes: `runtime::{connect, err, is_not_found, L_*}`, `runtime::spec::*`, `runtime::net::*`.
Produces:

```rust
/// Every published port the reconcile ended up with, read back from the daemon after start so
/// an ephemeral (`host: None`) binding has a concrete number to put on the Environment doc.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Published {
    pub service: String,
    pub container: u16,
    pub host: u16,
}

pub async fn up(d: &Docker, env: &Environment, live: &Path) -> Result<Vec<Published>, EngErr>;
pub async fn up_ws(d: &Docker, ws_id: &str, owner: &str, image: &str, live: &Path) -> Result<(), EngErr>;
pub async fn down(d: &Docker, id: &str) -> Result<(), EngErr>;
pub async fn delete(d: &Docker, id: &str) -> Result<(), EngErr>;
pub async fn start(d: &Docker, id: &str) -> Result<(), EngErr>;
pub async fn stop(d: &Docker, id: &str) -> Result<(), EngErr>;
pub async fn is_running(d: &Docker, id: &str) -> Result<bool, EngErr>;
/// Every container this agent owns, any state — the janitor's input (Task 7).
pub async fn list_ours(d: &Docker) -> Result<Vec<ContainerSummary>, EngErr>;
```

`id` throughout is the LABEL value: `env-{id}` for environments, `ws-{id}` for workspaces —
i.e. exactly the container-name prefix, so callers never construct two different keys.

- [ ] **Step 1 (failing test — the label filter is the whole design, so pin it in a unit test
      that needs no daemon):**

      ```rust
      #[test]
      fn ours_filter_matches_our_label_or_the_legacy_compose_project() {
          let f = ours_filter("env-e1");
          assert_eq!(f["label"], ["kloudlite-git.id=env-e1"]);
          let f = legacy_filter("env-e1");
          assert_eq!(f["label"], ["com.docker.compose.project=env-e1"]);
      }
      ```
      Run: `cargo test -p kloudlite-git-agent-bin reconcile::` — fails.

- [ ] **Step 2 (implement the filters and `list_by`):**

      ```rust
      use bollard::container::ListContainersOptions;
      use bollard::secret::ContainerSummary;

      fn ours_filter(id: &str) -> HashMap<String, Vec<String>> {
          [("label".to_string(), vec![format!("{}={id}", super::L_ID)])].into()
      }

      // ponytail: the compose-labelled fallback exists only so demo-env and mongo-test (created by
      // the compose implementation this replaces) can still be torn down. `up` recreates every
      // environment under our own labels, so after every live environment has been through one
      // EnvUp on this release, delete `legacy_filter` and its two call sites in `down`/`delete`.
      fn legacy_filter(id: &str) -> HashMap<String, Vec<String>> {
          [("label".to_string(), vec![format!("com.docker.compose.project={id}")])].into()
      }

      async fn list_by(d: &Docker, filters: HashMap<String, Vec<String>>) -> Result<Vec<ContainerSummary>, EngErr> {
          d.list_containers(Some(ListContainersOptions { all: true, filters, ..Default::default() }))
              .await
              .map_err(|e| super::err("list containers", e))
      }

      /// Union of our label and the legacy compose one. Teardown must find BOTH during the
      /// migration window; nothing else does.
      async fn list_teardown(d: &Docker, id: &str) -> Result<Vec<ContainerSummary>, EngErr> {
          let mut v = list_by(d, ours_filter(id)).await?;
          v.extend(list_by(d, legacy_filter(id)).await?);
          v.dedup_by(|a, b| a.id == b.id);
          Ok(v)
      }
      ```

- [ ] **Step 3:** Run — passes. Commit:
      `git commit -am "List containers by label, matching the legacy compose project during migration"`

- [ ] **Step 4 (failing daemon test — down needs no local state):** In
      `bins/agent/tests/runtime_daemon.rs`:

      ```rust
      //! Integration tests against a REAL Docker daemon. `#[ignore]`d so `cargo test` on a
      //! machine without one still passes; run them on a docker host with
      //! `cargo test -p kloudlite-git-agent-bin --test runtime_daemon -- --ignored --test-threads=1`.
      //! Single-threaded because they share the daemon's container namespace.

      #[tokio::test]
      #[ignore = "needs a docker daemon"]
      async fn down_needs_no_local_state() {
          let d = runtime::connect().await.unwrap();
          let (env, live) = fixture_env("dns0", &[("a", "alpine:3")]).await;
          runtime::reconcile::up(&d, &env, live.path()).await.unwrap();
          // The whole point: throw away everything the node knows locally, keep only the id.
          drop(live);
          runtime::reconcile::down(&d, "env-dns0").await.unwrap();
          assert!(!runtime::reconcile::is_running(&d, "env-dns0").await.unwrap());
          runtime::reconcile::delete(&d, "env-dns0").await.unwrap();
          // Idempotent: a retried delete job must finish, not fail on "No such container".
          runtime::reconcile::delete(&d, "env-dns0").await.unwrap();
      }
      ```
      Run: `cargo test -p kloudlite-git-agent-bin --test runtime_daemon -- --ignored` — fails.

- [ ] **Step 5 (implement `create_one`, `down`, `delete`, `start`, `stop`, `is_running`):**

      ```rust
      use bollard::container::{
          Config, CreateContainerOptions, RemoveContainerOptions, StartContainerOptions,
          StopContainerOptions,
      };
      use bollard::image::CreateImageOptions;
      use bollard::network::ConnectNetworkOptions;
      use bollard::secret::{EndpointSettings, HostConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum};
      use futures::StreamExt;

      /// Pull only when the image is absent. `create_image` always hits the registry, and an
      /// EnvUp on an unchanged environment should not depend on the network being up.
      async fn pull_if_needed(d: &Docker, image: &str) -> Result<(), EngErr> {
          if d.inspect_image(image).await.is_ok() {
              return Ok(());
          }
          let mut s = d.create_image(
              Some(CreateImageOptions { from_image: image, ..Default::default() }),
              None,
              None,
          );
          while let Some(chunk) = s.next().await {
              chunk.map_err(|e| super::err(&format!("pull {image}"), e))?;
          }
          Ok(())
      }

      fn host_config(s: &ContainerSpec) -> HostConfig {
          let mut ports: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
          for p in &s.ports {
              ports.insert(
                  format!("{}/tcp", p.container),
                  // `host_port: None` == publish on an ephemeral port; read back after start.
                  Some(vec![PortBinding {
                      host_ip: Some("0.0.0.0".into()),
                      host_port: p.host.map(|h| h.to_string()),
                  }]),
              );
          }
          HostConfig {
              binds: Some(s.binds.clone()),
              port_bindings: (!ports.is_empty()).then_some(ports),
              // Same policy the CLI path used for workspaces; an env service that exits on a
              // daemon restart is indistinguishable to a user from one we failed to start.
              restart_policy: Some(RestartPolicy {
                  name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                  ..Default::default()
              }),
              ..Default::default()
          }
      }

      /// Create + connect + start. The network alias is the SERVICE NAME — that single field is
      /// what makes `mongodb://db:27017` resolve from a sibling service, and it is the only thing
      /// compose was contributing that we actually used.
      async fn create_one(d: &Docker, s: &ContainerSpec, net: Option<&str>) -> Result<(), EngErr> {
          pull_if_needed(d, &s.image).await?;
          let mut exposed: HashMap<String, HashMap<(), ()>> = HashMap::new();
          for p in &s.ports {
              exposed.insert(format!("{}/tcp", p.container), HashMap::new());
          }
          let cfg = Config {
              image: Some(s.image.clone()),
              cmd: (!s.command.is_empty()).then(|| s.command.clone()),
              env: Some(s.env.iter().map(|(k, v)| format!("{k}={v}")).collect()),
              labels: Some(s.labels.clone().into_iter().collect()),
              exposed_ports: (!exposed.is_empty()).then_some(exposed),
              host_config: Some(host_config(s)),
              ..Default::default()
          };
          d.create_container(
              Some(CreateContainerOptions { name: s.name.clone(), platform: None }),
              cfg,
          )
          .await
          .map_err(|e| super::err(&format!("create {}", s.name), e))?;
          if let Some(net) = net {
              d.connect_network(
                  net,
                  ConnectNetworkOptions {
                      container: s.name.clone(),
                      endpoint_config: EndpointSettings {
                          aliases: Some(vec![s.service.clone()]),
                          ..Default::default()
                      },
                  },
              )
              .await
              .map_err(|e| super::err(&format!("connect {} to {net}", s.name), e))?;
          }
          d.start_container(&s.name, None::<StartContainerOptions<String>>)
              .await
              .map_err(|e| super::err(&format!("start {}", s.name), e))
      }

      async fn remove_one(d: &Docker, cid: &str) -> Result<(), EngErr> {
          match d
              .remove_container(cid, Some(RemoveContainerOptions { force: true, ..Default::default() }))
              .await
          {
              Ok(()) => Ok(()),
              Err(e) if super::is_not_found(&e) => Ok(()),
              Err(e) => Err(super::err(&format!("remove {cid}"), e)),
          }
      }

      /// Stop everything labelled with this id. No spec, no file, no document required — the id
      /// alone is enough, which is exactly what makes a retried or fresh-agent teardown correct.
      pub async fn down(d: &Docker, id: &str) -> Result<(), EngErr> {
          for c in list_teardown(d, id).await? {
              let Some(cid) = c.id.as_deref() else { continue };
              match d.stop_container(cid, Some(StopContainerOptions { t: 10 })).await {
                  Ok(()) => {}
                  Err(e) if super::is_not_found(&e) => {}
                  // 304 == already stopped. Not an error; a retried EnvDown hits this every time.
                  Err(bollard::errors::Error::DockerResponseServerError { status_code: 304, .. }) => {}
                  Err(e) => return Err(super::err(&format!("stop {cid}"), e)),
              }
          }
          Ok(())
      }

      pub async fn delete(d: &Docker, id: &str) -> Result<(), EngErr> {
          for c in list_teardown(d, id).await? {
              if let Some(cid) = c.id.as_deref() {
                  remove_one(d, cid).await?;
              }
          }
          // Only after the containers are gone: a network with endpoints still attached refuses
          // removal, and that refusal would mask a container we failed to remove.
          if let Some(env_id) = id.strip_prefix("env-") {
              super::net::remove_network(d, env_id).await?;
          }
          Ok(())
      }

      pub async fn start(d: &Docker, id: &str) -> Result<(), EngErr> {
          for c in list_by(d, ours_filter(id)).await? {
              let Some(cid) = c.id.as_deref() else { continue };
              match d.start_container(cid, None::<StartContainerOptions<String>>).await {
                  Ok(()) | Err(bollard::errors::Error::DockerResponseServerError { status_code: 304, .. }) => {}
                  Err(e) if super::is_not_found(&e) => {}
                  Err(e) => return Err(super::err(&format!("start {cid}"), e)),
              }
          }
          Ok(())
      }

      pub async fn stop(d: &Docker, id: &str) -> Result<(), EngErr> {
          down(d, id).await
      }

      /// True if ANY container with this label is running. Replaces the
      /// `docker inspect -f '{{.State.Running}}'` stdout parse with the typed state the daemon
      /// already returns in a list — and a missing container counts as not running, since a
      /// never-started source clones the same way a stopped one does.
      pub async fn is_running(d: &Docker, id: &str) -> Result<bool, EngErr> {
          let mut f = ours_filter(id);
          f.insert("status".into(), vec!["running".into()]);
          Ok(!list_by(d, f).await?.is_empty())
      }

      pub async fn list_ours(d: &Docker) -> Result<Vec<ContainerSummary>, EngErr> {
          list_by(d, [("label".to_string(), vec![super::L_KIND.to_string()])].into()).await
      }
      ```

- [ ] **Step 6:** Run the ignored test on a docker host — passes. Commit:
      `git commit -am "Drive container create, teardown and state through the daemon API"`

- [ ] **Step 7 (failing daemon test — idempotent replay, audit H2):**

      ```rust
      #[tokio::test]
      #[ignore = "needs a docker daemon"]
      async fn up_twice_is_a_no_op_and_a_changed_image_recreates_only_that_service() {
          let d = runtime::connect().await.unwrap();
          let (mut env, live) = fixture_env("idem", &[("a", "alpine:3"), ("b", "alpine:3")]).await;
          runtime::reconcile::up(&d, &env, live.path()).await.unwrap();
          let first = ids_by_service(&d, "env-idem").await;

          // Replay — what a duplicated job does. Must not error with "container already exists",
          // and must not restart anything.
          runtime::reconcile::up(&d, &env, live.path()).await.unwrap();
          assert_eq!(first, ids_by_service(&d, "env-idem").await, "up recreated an unchanged service");

          env.services.iter_mut().find(|s| s.name == "b").unwrap().image = "alpine:3.20".into();
          runtime::reconcile::up(&d, &env, live.path()).await.unwrap();
          let third = ids_by_service(&d, "env-idem").await;
          assert_eq!(first["a"], third["a"], "unchanged service was recreated");
          assert_ne!(first["b"], third["b"], "changed service was not recreated");

          // A service dropped from the desired set is removed.
          env.services.retain(|s| s.name == "a");
          runtime::reconcile::up(&d, &env, live.path()).await.unwrap();
          assert!(!ids_by_service(&d, "env-idem").await.contains_key("b"));

          runtime::reconcile::delete(&d, "env-idem").await.unwrap();
      }
      ```
      Run — fails (`up` unimplemented).

- [ ] **Step 8 (implement `up` / `up_ws`):**

      ```rust
      /// Reconcile, not create. Comparing the desired spec hash against the label on the running
      /// container is what makes a duplicated job a no-op instead of a "container already exists"
      /// failure (audit H2) — and what keeps an unrelated service from restarting when a sibling
      /// changes.
      pub async fn up(d: &Docker, env: &Environment, live: &Path) -> Result<Vec<Published>, EngErr> {
          let id = format!("env-{}", env.id);
          super::net::ensure_network(d, &env.id, &env.owner).await?;
          let desired = super::spec::env_specs(env, live)?;

          let existing = list_by(d, ours_filter(&id)).await?;
          let by_service: HashMap<&str, &ContainerSummary> = existing
              .iter()
              .filter_map(|c| Some((c.labels.as_ref()?.get(super::L_SERVICE)?.as_str(), c)))
              .collect();

          let net = super::net::net_name(&env.id);
          for s in &desired {
              match by_service.get(s.service.as_str()) {
                  Some(c) if c.labels.as_ref().and_then(|l| l.get(super::L_SPEC)).map(String::as_str)
                      == Some(s.labels[super::L_SPEC].as_str()) =>
                  {
                      // Same spec: ensure it is running and leave it alone. Recreating here would
                      // restart a database for no reason on every EnvUp retry.
                      if c.state.as_deref() != Some("running") {
                          if let Some(cid) = c.id.as_deref() {
                              d.start_container(cid, None::<StartContainerOptions<String>>)
                                  .await
                                  .map_err(|e| super::err(&format!("start {cid}"), e))?;
                          }
                      }
                  }
                  Some(c) => {
                      if let Some(cid) = c.id.as_deref() {
                          remove_one(d, cid).await?;
                      }
                      create_one(d, s, Some(&net)).await?;
                  }
                  None => create_one(d, s, Some(&net)).await?,
              }
          }

          // Anything labelled ours whose service left the desired set. Doing this AFTER the
          // creates means a rename (drop b, add c) never has a window with neither present.
          let want: std::collections::HashSet<&str> = desired.iter().map(|s| s.service.as_str()).collect();
          for c in &existing {
              let svc = c.labels.as_ref().and_then(|l| l.get(super::L_SERVICE));
              if svc.is_some_and(|s| !want.contains(s.as_str())) {
                  if let Some(cid) = c.id.as_deref() {
                      remove_one(d, cid).await?;
                  }
              }
          }
          published(d, &id).await
      }

      /// Read the daemon's actual bindings back after start: an ephemeral (`host: None`) port only
      /// has a number once the container is running, and the UI needs it to link to the service.
      async fn published(d: &Docker, id: &str) -> Result<Vec<Published>, EngErr> {
          let mut out = Vec::new();
          for c in list_by(d, ours_filter(id)).await? {
              let Some(svc) = c.labels.as_ref().and_then(|l| l.get(super::L_SERVICE)).cloned() else { continue };
              for p in c.ports.unwrap_or_default() {
                  if let Some(host) = p.public_port {
                      out.push(Published { service: svc.clone(), container: p.private_port as u16, host: host as u16 });
                  }
              }
          }
          out.sort_by(|a, b| (&a.service, a.container).cmp(&(&b.service, b.container)));
          Ok(out)
      }

      /// A workspace is an environment with one service and a fixed mount pair, so it reconciles
      /// through the same path — minus the network, since a workspace has no siblings to resolve.
      pub async fn up_ws(d: &Docker, ws_id: &str, owner: &str, image: &str, live: &Path) -> Result<(), EngErr> {
          let s = super::spec::ws_spec(ws_id, owner, image, live)?;
          let id = format!("ws-{ws_id}");
          match list_by(d, ours_filter(&id)).await?.first() {
              Some(c) if c.labels.as_ref().and_then(|l| l.get(super::L_SPEC)).map(String::as_str)
                  == Some(s.labels[super::L_SPEC].as_str()) => start(d, &id).await,
              Some(c) => {
                  if let Some(cid) = c.id.as_deref() {
                      remove_one(d, cid).await?;
                  }
                  create_one(d, &s, None).await
              }
              None => create_one(d, &s, None).await,
          }
      }
      ```

- [ ] **Step 9:** Run the ignored tests — pass. `cargo clippy --workspace -- -D warnings`.
      Commit: `git commit -am "Reconcile an environment's containers against their spec hashes"`

- [ ] **Step 10 (failing daemon test — two-service DNS):**

      ```rust
      /// The one thing compose genuinely gave us. `b` must resolve `a` by SERVICE name, which is
      /// the network alias set at connect time — not the container name.
      #[tokio::test]
      #[ignore = "needs a docker daemon"]
      async fn two_services_resolve_each_other_by_service_name() {
          let d = runtime::connect().await.unwrap();
          let (env, live) = fixture_env_cmds("dnst", &[
              ("web", "alpine:3", vec!["sh", "-c", "nc -l -p 8080 -e echo hi"]),
              ("client", "alpine:3", vec!["sh", "-c", "sleep 300"]),
          ]).await;
          runtime::reconcile::up(&d, &env, live.path()).await.unwrap();

          // exec `getent hosts web` inside the client — a name lookup, not a connection, so the
          // test does not depend on the server's readiness timing.
          let out = exec_in(&d, "env-dnst-client-1", &["getent", "hosts", "web"]).await;
          assert!(!out.trim().is_empty(), "service name 'web' did not resolve from a sibling: {out:?}");

          runtime::reconcile::delete(&d, "env-dnst").await.unwrap();
      }
      ```

      `exec_in` is a small helper in the test file using
      `d.create_exec(name, CreateExecOptions { cmd: Some(argv), attach_stdout: Some(true), ..Default::default() })`
      then `d.start_exec(&exec.id, None)` and draining the `StartExecResults::Attached` stream.

- [ ] **Step 11:** Run — passes (it should already, if aliases landed in Step 5; if not, that is
      the bug this test exists to catch). Commit:
      `git commit -am "Assert service-name DNS between two services in one environment"`

---

### Task 5: Ports on the model, YAML at the API boundary, write-time validation

**Files:** `crates/workspaces/src/model.rs`, `crates/workspaces/src/api.rs`,
`crates/workspaces/Cargo.toml`

**Interfaces**

Consumes: `runtime::spec::{mount_source, mount_target}` semantics (duplicated as a validator
here — the API crate must not depend on the agent bin, so `api.rs` calls
`kloudlite_git_storage::store::valid_segment` directly, the same predicate).
Produces:

```rust
// crates/workspaces/src/model.rs
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortMap {
    pub container: u16,
    /// `None` == publish on an ephemeral host port, read back after start.
    #[serde(default)]
    pub host: Option<u16>,
}

pub struct Service { /* ...existing... */ #[serde(default)] pub ports: Vec<PortMap> }

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Published { pub service: String, pub container: u16, pub host: u16 }

pub struct Environment { /* ...existing... */ #[serde(default)] pub published: Vec<Published> }
```

`#[serde(default)]` on both new fields is load-bearing: environment documents already in Cosmos
have neither key, and a non-defaulted field would fail to deserialize every existing env.

- [ ] **Step 1 (failing test):** in `crates/workspaces/src/api.rs`'s test module:

      ```rust
      #[test]
      fn env_service_validation_refuses_escaping_mounts_and_bad_targets() {
          for (folder, path) in [("/", "/data"), ("..", "/data"), ("a/b", "/data"), ("", "/data"),
                                 ("a:b", "/data"), ("data", "data"), ("data", "/a:b")] {
              let svc = svc_with_mount(folder, path);
              assert!(validate_services(&[svc]).is_err(), "accepted folder={folder:?} path={path:?}");
          }
          assert!(validate_services(&[svc_with_mount("data", "/data/db")]).is_ok());
      }

      #[test]
      fn env_yaml_rejects_unknown_keys_by_name() {
          let e = parse_env_yaml("services:\n  db:\n    image: mongo:7\n    depends_on: [a]\n").unwrap_err();
          assert!(e.contains("depends_on"), "error must name the refused key: {e}");
      }

      #[test]
      fn env_yaml_parses_the_five_supported_keys() {
          let svcs = parse_env_yaml(
              "services:\n  db:\n    image: mongo:7\n    command: [mongod]\n    environment:\n      A: '1'\n    volumes:\n      - data:/data/db\n    ports:\n      - 27017\n",
          ).unwrap();
          assert_eq!(svcs.len(), 1);
          assert_eq!(svcs[0].name, "db");
          assert_eq!(svcs[0].mounts[0].folder, "data");
          assert_eq!(svcs[0].ports, [PortMap { container: 27017, host: None }]);
      }
      ```
      Run: `cargo test -p kloudlite-git-workspaces api::` — fails.

- [ ] **Step 2 (implement):** add the model fields, then in `api.rs`:

      ```rust
      /// Validate at WRITE time, not at materialization time: a spec that can never be brought up
      /// should be refused by the request that created it, where the user is still watching.
      /// The agent's `runtime::spec` re-validates anyway — this is the friendly half of a rule
      /// that is enforced in both places on purpose.
      fn validate_services(svcs: &[Service]) -> Result<(), String> {
          for s in svcs {
              if !kloudlite_git_storage::store::valid_segment(&s.name) {
                  return Err(format!("invalid service name {:?}", s.name));
              }
              for m in &s.mounts {
                  if !kloudlite_git_storage::store::valid_segment(&m.folder) {
                      return Err(format!("invalid volume folder {:?}", m.folder));
                  }
                  if !m.path.starts_with('/') || m.path.contains(':') {
                      return Err(format!("mount path must be absolute and contain no ':': {:?}", m.path));
                  }
              }
          }
          Ok(())
      }
      ```

      Replace the existing `m.folder.is_empty()` check in `create_env` (and the same shape in
      `clone_env`, which copies `src.services`) with `validate_services(&body.services)`, mapping
      the error to `(StatusCode::BAD_REQUEST, e)`.

      The YAML parser is a `#[serde(deny_unknown_fields)]` shadow struct — serde does the naming
      for free, which is why the error message requirement costs nothing:

      ```rust
      /// Compose-shaped YAML is an INPUT FORMAT ONLY: it is parsed here into `Vec<Service>` and
      /// never persisted, never sent to the agent. `deny_unknown_fields` is the whole point —
      /// silently dropping `depends_on` makes the user believe they expressed ordering they did
      /// not get, which is a support ticket; refusing it is a feature request.
      #[derive(serde::Deserialize)]
      #[serde(deny_unknown_fields)]
      struct YamlFile {
          services: std::collections::BTreeMap<String, YamlService>,
      }

      #[derive(serde::Deserialize)]
      #[serde(deny_unknown_fields)]
      struct YamlService {
          image: String,
          #[serde(default)]
          command: Vec<String>,
          #[serde(default)]
          environment: HashMap<String, String>,
          /// `source:target` where source is a VOLUME FOLDER NAME, never a host path — a compose
          /// file's host-path bind is exactly the C1 hole, so it is not accepted here.
          #[serde(default)]
          volumes: Vec<String>,
          #[serde(default)]
          ports: Vec<String>,
      }

      pub fn parse_env_yaml(src: &str) -> Result<Vec<Service>, String> {
          let f: YamlFile = serde_yml::from_str(src).map_err(|e| e.to_string())?;
          let mut out = Vec::new();
          for (name, s) in f.services {
              let mut mounts = Vec::new();
              for v in &s.volumes {
                  let (folder, path) = v.split_once(':').ok_or_else(|| format!("volume {v:?} must be folder:path"))?;
                  mounts.push(Mount { folder: folder.to_string(), path: path.to_string() });
              }
              let mut ports = Vec::new();
              for p in &s.ports {
                  // "8080:80" == fixed host port; "80" == ephemeral.
                  let pm = match p.split_once(':') {
                      Some((h, c)) => PortMap { container: num(c)?, host: Some(num(h)?) },
                      None => PortMap { container: num(p)?, host: None },
                  };
                  ports.push(pm);
              }
              out.push(Service { name, image: s.image, command: s.command, env: s.environment, mounts, ports });
          }
          validate_services(&out)?;
          Ok(out)
      }
      ```

      Wire it into `create_env`: if the request's `Content-Type` is `application/yaml` or
      `text/yaml`, read the body as a string and `parse_env_yaml` it; otherwise the existing JSON
      path. Keep `NewEnvironment`'s JSON shape unchanged.

- [ ] **Step 3:** Run `cargo test -p kloudlite-git-workspaces` — passes.
      Commit: `git commit -am "Accept compose-shaped YAML as an input format and refuse unknown keys"`

- [ ] **Step 4:** Have `EnvUp`'s done handler write `up`'s `Vec<Published>` onto the
      `Environment` document (the same etag-CAS write the state transition already does — fold it
      into that one update, do not add a second write). Test: an env with
      `ports: [{container: 80}]` comes back from `GET /v1/environments/{id}` with a non-zero
      `published[0].host`. Commit:
      `git commit -am "Surface published ports on the environment document"`

---

### Task 6: Cut the agent over and delete the CLI paths

**Files:** `bins/agent/src/lib.rs`; delete `bins/agent/src/container.rs`,
`crates/workspaces/src/engine/compose.rs`

- [ ] **Step 1:** Hold one `Docker` on the agent's shared state (it is cheap to clone and the
      connection is pooled) — add it beside `Engine` where `spawn_janitor` is wired at
      `bins/agent/src/lib.rs:165`, connecting via `runtime::connect()` during startup so a bad
      daemon fails the process, not the first job.

- [ ] **Step 2:** Rewrite the job arms. Each is a mechanical swap; the `EnvDelete` arm is the one
      with new behaviour:

      ```rust
      JobKind::WsCreate | JobKind::WsRestore | JobKind::WsStart => {
          runtime::reconcile::up_ws(&d, &w.id, &w.owner, &w.image, &engine.pool.live(&w.id)).await.map_err(|e| e.to_string())?;
      }
      JobKind::WsStop  => runtime::reconcile::stop(&d, &format!("ws-{}", w.id)).await...,
      JobKind::WsDelete => { runtime::reconcile::delete(&d, &format!("ws-{}", w.id)).await...; cleanup_local(engine, &w.id); }

      JobKind::EnvUp => { /* subvol provisioning unchanged */ mkdir_env_mounts(&live, &env)?;
          let published = runtime::reconcile::up(&d, &env, &live).await.map_err(|e| e.to_string())?;
          Ok(json!({"published": published})) }
      JobKind::EnvDown => { runtime::reconcile::down(&d, &format!("env-{id}")).await...; engine.push_env(..).await?; }
      JobKind::EnvDelete => {
          runtime::reconcile::delete(&d, &format!("env-{id}")).await...;
          engine.push_env(..).await?;
          cleanup_local(engine, &env.id);
          // Audit M1: every environment the compose implementation ever created leaked its
          // {pool}/env/{id} directory, because cleanup_local only knew about vol/{id}. Nothing
          // writes this directory any more, so absence is the expected case — remove it if it is
          // there and say nothing if it is not.
          // ponytail: delete this once no production pool still has an env/ directory.
          let _ = std::fs::remove_dir_all(env_dir(&engine.pool, &env.id));
      }
      ```

      In the `WsClone` arms, `container::is_running(cname)` becomes
      `runtime::reconcile::is_running(&d, &format!("ws-{}", src.id))`, and the `stop`/`start`
      closures call `runtime::reconcile::{stop,start}` — but note the closures are `&dyn Fn`
      with no `Send` bound and are called from a blocking context (`engine::ops.rs`), so they
      cannot `.await`. Use `tokio::runtime::Handle::current().block_in_place(|| handle.block_on(..))`
      or, simpler, keep the closures synchronous by hoisting the container ids and doing the
      stop/start with `futures::executor::block_on` on the cloned `Docker`.
      `// ponytail: block_on inside the non-Send clone hooks; the real fix is making
      engine::ops' hooks async, which is a bigger change than this task.`

      `stop_project` / `stop_projects` payload keys become the label id (`env-{id}` / `ws-{id}`);
      the api sends the same string it already sends for `stop_project` (`env-{id}` was the
      compose project name), so **no api change is needed** — verify that in
      `crates/workspaces/src/api.rs`'s `clone_env` before assuming it.

- [ ] **Step 3:** Delete `bins/agent/src/container.rs`,
      `crates/workspaces/src/engine/compose.rs` (and its `pub mod compose;`), and the now-unused
      `compose()`, `docker_stop_name()`, `docker_start_name()` helpers. Drop `serde_yaml` from
      the workspace `Cargo.toml`.

- [ ] **Step 4:** `cargo test` and `cargo clippy --workspace -- -D warnings`. Commit:
      `git commit -am "Cut the agent over to the runtime module and delete the docker CLI paths"`

---

### Task 7: Label-driven janitor reclamation

**Files:** `bins/agent/src/lib.rs`

Today the janitor reclaims subvolumes but knows nothing about containers, so a container whose
`Environment` document is gone (a delete that failed after the doc write, a Cosmos rollback)
runs forever. Labels make ownership queryable for the first time.

- [ ] **Step 1 (failing test):** the decision is pure, so test it without a daemon:

      ```rust
      #[test]
      fn janitor_reclaims_only_containers_whose_doc_is_gone() {
          let live: HashSet<String> = ["env-a", "ws-b"].iter().map(|s| s.to_string()).collect();
          let on_node = ["env-a", "env-c", "ws-b", "ws-d"];
          let doomed = orphan_ids(&on_node.map(String::from), &live);
          assert_eq!(doomed, ["env-c", "ws-d"]);
      }
      ```
      Run: `cargo test -p kloudlite-git-agent-bin janitor` — fails.

- [ ] **Step 2 (implement):** `orphan_ids` is a set difference. In `spawn_janitor`'s tick, call
      `runtime::reconcile::list_ours(&d)`, group by the `kloudlite-git.id` label, ask the meta store
      which of those ids still exist, and `runtime::reconcile::delete` the rest.

      **Fail closed:** a store error must skip the sweep entirely, never treat "I could not ask"
      as "the doc is gone". Mirror the keep-biased posture of the registry GC sweep
      (`crates/registry/src/gc.rs`): any uncertainty aborts.

- [ ] **Step 3:** Run — passes. Commit:
      `git commit -am "Reclaim containers whose environment document no longer exists"`

---

### Task 8: e2e

**Files:** `tests/ws_e2e.sh`

- [ ] **Step 1:** The `docker compose version` preflight (line 40) becomes a daemon check —
      `docker version --format '{{.Server.APIVersion}}'` — since compose is no longer required.
      The cleanup trap (line 77) drops its `-f "$ENV_DIR/docker-compose.yml"` teardown for
      `docker rm -f $(docker ps -aq --filter "label=kloudlite-git.id=env-$ENV_ID")`, which needs no
      `ENV_DIR` at all. Delete the `ENV_DIR=` assignment.

- [ ] **Step 2:** Extend the environment create (line ~424) to two services, and assert DNS:

      ```bash
      log "checking service-to-service DNS inside the environment"
      docker exec "env-$ENV_ID-client-1" getent hosts writer \
        || fail "service 'writer' did not resolve by name from a sibling service"
      ```

- [ ] **Step 3:** Assert the published port and the absence of the compose directory:

      ```bash
      log "checking the published port came back on the environment document"
      ENV_PORT=$(curl -fsS "$BASE/v1/environments/$ENV_ID" -H "Authorization: Bearer $USER_TOKEN" \
        | field 'published[0].host')
      [ -n "$ENV_PORT" ] && [ "$ENV_PORT" != "0" ] || fail "no published host port on the environment"

      log "checking no compose directory was written for the environment"
      [ ! -e "$MOUNT/env/$ENV_ID" ] || fail "the runtime wrote a compose directory: $MOUNT/env/$ENV_ID"
      ```

- [ ] **Step 4:** Add the delete case that M1 was about — teardown with no local state:

      ```bash
      log "deleting the environment (teardown must not depend on any local file)"
      rm -rf "$MOUNT/env/$ENV_ID"   # simulate the half-removed directory M1 stranded jobs on
      curl -fsS -X DELETE "$BASE/v1/environments/$ENV_ID" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
      wait_env_state "$ENV_ID" deleted
      [ -z "$(docker ps -aq --filter "label=kloudlite-git.id=env-$ENV_ID")" ] \
        || fail "containers survived the environment delete"
      ```

- [ ] **Step 5:** Script the MongoDB clone-fidelity check performed by hand on 2026-08-25: seed
      the source env's mongo with a document, clone the env, assert the clone's own database has
      the document, write to the clone, assert the write does NOT appear in the source. This is
      the assertion that the clone is a real copy and not a shared subvolume.

- [ ] **Step 6:** Update the final `echo "OK: ..."` line to name the new cases. Run
      `./tests/ws_e2e.sh` on the Linux btrfs VM (it exits 77 on this Mac — a skip, not a pass).
      Commit: `git commit -am "Cover service DNS, published ports and stateless teardown in ws_e2e"`

---

## Done when

- `cargo test` and `cargo clippy --workspace -- -D warnings` are clean.
- `cargo test -p kloudlite-git-agent-bin --test runtime_daemon -- --ignored --test-threads=1`
  passes on a docker host.
- `./tests/ws_e2e.sh` passes on the btrfs VM (exit 0, not 77).
- `grep -rn "docker compose\|serde_yaml" crates bins` returns nothing outside `tests/`.
- The two live production environments (`demo-env`, `mongo-test`) have been through one
  `EnvUp` on the new release, so `legacy_filter` and its `// ponytail:` marker can be removed
  in the following one.
