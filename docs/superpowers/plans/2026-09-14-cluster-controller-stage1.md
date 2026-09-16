# Cluster Controller — Stage 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A new leader-elected `kloudlite-controller` Deployment runs in each k3s cluster and becomes the ONE writer of a space's two NetworkPolicies (`space-env` in the space namespace, `space-{ns}` in the environment namespace). The agents stop writing them in the same release. Nothing else moves in stage 1.

**Architecture:** `bins/controller` is a new binary with no disk, no object store and no Azure credential — it talks to the k3s API server and nothing else. It holds a `coordination.k8s.io/v1` Lease named `kloudlite-controller` in `kube-system`; `leaseTransitions` is the epoch and every write checks the epoch it was elected under before it lands. Three reflectors (`SpaceEnvironment`, `Environment`, `Workspace`) feed two reconcilers: one keyed on `SpaceEnvironment` (renders that space's egress half plus the ingress half in the environment it names) and one keyed on `Environment` (prunes `space-*` ingress halves in that namespace that no space points at). Both apply server-side under field manager `kloudlite-controller` with `force`, which is also how the objects the agents already wrote are adopted. An unlisted reflector answers `None` and the pass decides nothing.

**Tech Stack:** Rust 2021, `kube` + `k8s-openapi` (workspace pins; `k8s_openapi::api::coordination::v1::Lease` already ships), `axum` for one `/healthz` route, `tokio`, `tracing`. Tests are ordinary `#[test]`/`#[tokio::test]` in-crate; the lease tests take `now_ms` as a parameter and use no clock at all.

Terms: a **Region** is the unit a team is bound to and may hold SEVERAL k3s clusters; this
controller is per **cluster** — one elected leader per k3s cluster, over that cluster's agents.
"Region" below means only the `Region` CRD, a team's bound region or an Azure region.

**Spec:** `docs/superpowers/specs/2026-09-14-cluster-controller-design.md` (commit `4fee9974`), stage 1 only.

**Rulings (all five settled by the owner — 1 on 2026-09-14, 2–5 on 2026-09-16; the spec's "Open questions" section carries the wording):**

1. **One controller per k3s CLUSTER only.** A Region is the unit a team is bound to and may hold several clusters; the controller is per cluster, over that cluster's agents. AKS has no `Region` CRD, no agents and no `SpaceEnvironment` objects, so nothing is deployed there.
2. **Stage 3 ships as a hard cutover**, no `Mark::Boot` flag. Nothing in this plan adds a flag.
3. **`OwnerKeys` gets no per-node status entries**, in any stage: a `Synced` condition plus the `key.install` log line. Do not touch `keys.rs` in stage 1.
4. **Snapshot retention stays in the agent.**
5. **Bench force-delete stays in the agent.**

## Global Constraints

- **Two writers never coexist for one object.** The agent's space-policy writes are deleted in the SAME release that adds the controller's. Ordering at rollout time: agents roll FIRST (they stop writing), then the controller is applied, then `agent-rbac.yaml`'s narrowed role. Applying the narrowed RBAC before the agent roll 403s a still-writing agent and aborts its whole reconcile — the `networkpolicies: delete` lesson recorded at `deploy/k3s/agent-rbac.yaml:327-330`.
- **Every controller write is epoch-checked.** Read `Ctx::epoch` before the write; if the Lease's `leaseTransitions` has moved past it, demote and write nothing. There is no writer fence under the API server, so the per-object CAS (SSA under our own field manager, or a delete by name) is the backstop — never a blind force over an object we did not derive from spec.
- **An unlisted reflector is UNKNOWN, never empty.** `Ctx::spaces()`/`environments()`/`workspaces()` return `None` until the first list finishes (`store_ready`, copied from `bins/agent/src/controller/mod.rs:447`). A pass that reads `None` renders nothing and deletes nothing.
- **Fan-out is bounded.** One `SpaceEnvironment` event re-renders exactly one space's egress half and at most two environments' ingress halves (the one it names, and the one it named before). Never `all_in_store`. That is the whole point of the move — `bins/agent/src/controller/run.rs:432-436` wakes every environment on every node today.
- **`forget_applied` after every delete.** The `ensure` memory (`APPLY_RESYNC`) is now the truth for the whole cluster, so a child mutated outside `ensure` must be forgotten first or it stays un-reapplied for ten minutes. F1 in the spec is exactly this bug.
- **No new NetworkPolicy shapes.** `crates/workspaces/src/k8s/policies.rs::{space_egress, space_ingress, SPACE_EGRESS_POLICY, space_ingress_name}` are unchanged and stay the single definition; the controller calls them.
- **The controller holds no secret.** No `WS_PEER_SECRET`, no `KLOUDLITE_S3_URL`, no `KLOUDLITE_JWT_SECRET`. Ordinary uid 1001, read-only root, no hostPath, no privileged context.
- **RBAC is the table.** Every verb the controller gains has a row in `deploy/k3s/controller-rbac.yaml`'s header table with its call site, the same contract `agent-rbac.yaml` carries. A call added without a row 403s naming the file.
- Commits: imperative sentence case, no attribution lines, one commit per task, `git push platform <branch>`. Other agents commit concurrently — run `git log -3` and `git status` before each commit and stage ONLY the files this plan names (`git add <paths>`, never `git commit -a`).
- Build and test from `/Volumes/kdisk/rustic-git-wt/desktop-login` (the worktree is on the external disk, so `target/` lands there). `cd` there before every command.

## File Structure

| File | Status | Responsibility |
|---|---|---|
| `bins/controller/Cargo.toml` | create | the `kloudlite-controller` binary + `kloudlite_controller` lib |
| `bins/controller/src/main.rs` | create | log/metrics init, `run(Config::from_env())` |
| `bins/controller/src/lib.rs` | create | `Config`, `run`: client, settings, health listener, election loop, reconcilers |
| `bins/controller/src/lease.rs` | create | the pure lease state machine + its kube I/O; unit tests with a fake clock |
| `bins/controller/src/ctx.rs` | create | `Ctx`: client, holder, epoch, reflectors, `applied` map, settings |
| `bins/controller/src/space.rs` | create | the two reconcilers and the derived-set renderer |
| `bins/controller/src/health.rs` | create | one axum route, `/healthz` |
| `Cargo.toml` (root) | modify | workspace member |
| `Dockerfile` | modify | `controller` stage |
| `.github/workflows/image.yml` | modify | build the bin, glibc check, artifact, push `kloudlite-controller` |
| `deploy/pin.sh` | modify | pin `kloudlite-controller` in `k3s/controller.yaml` |
| `deploy/k3s/controller.yaml` | create | Deployment (1 replica, `Recreate`, `kube-system`) + ServiceAccount |
| `deploy/k3s/controller-rbac.yaml` | create | ClusterRole + binding + the `kube-system` Role for the Lease |
| `deploy/k3s/agent-rbac.yaml` | modify | remove the space-policy rows from the table; narrow `networkpolicies` |
| `deploy/k3s/README.md` | modify | apply order for the mixed-build cutover |
| `crates/workspaces/src/api/workloads.rs` | modify | `KNOWN_PER_REGION` + `namespace()` gain the controller |
| `bins/agent/src/controller/space.rs` | modify | delete every policy write; keep resolv.conf + the `Attached` condition |
| `bins/agent/src/controller/environment/mod.rs` | modify | `prune_attach_grants` stops touching `space-*` |
| `bins/agent/src/controller/run.rs` | modify | drop the `SpaceEnvironment` → every-environment fan-out watch |
| `bins/agent/tests/reconcile/attachment.rs` | modify | policy assertions become "the agent wrote nothing here" |
| `bins/agent/tests/reconcile/agent_writes_no_space_policies.rs` | create | the mixed-build guard |
| `bins/agent/tests/reconcile/main.rs` | modify | declare the new module |
| `bins/slo/src/stages/controller.rs` | create | the seven `ctl.*` probe steps |
| `bins/slo/src/stages/mod.rs` | modify | declare and run the stage |
| `crates/workspaces/src/slo/catalogue.rs` | modify | seven rows |
| `deploy/slo.md` | modify | the same seven rows |
| `crates/workspaces/src/history/watch.rs` | modify | Lease holder transitions → `controller.leader` rows |
| `CLAUDE.md` | modify | a "Cluster controller" paragraph; edits to the agent paragraph |

**Not in this plan (blocked):** the intercept objects. See Task 8.

---

### Task 1: The controller crate and the lease state machine

**Files:**
- Create: `bins/controller/Cargo.toml`, `bins/controller/src/lease.rs`, `bins/controller/src/lib.rs` (module declarations only in this task), `bins/controller/src/main.rs` (stub)
- Modify: `Cargo.toml` (root, `members`)

**Interfaces:**
- Produces: `pub const LEASE_NAME: &str = "kloudlite-controller"`, `pub const LEASE_NAMESPACE: &str = "kube-system"`, `pub const TTL: Duration = 15s`, `pub const RENEW: Duration = 5s`; `pub struct View { pub holder: String, pub transitions: u32, pub renewed_ms: u64 }`; `pub enum Step { Acquire { epoch: u32 }, Renew { epoch: u32 }, Wait }`; `pub fn decide(now_ms: u64, me: &str, cur: Option<&View>) -> Step`.

- [ ] **Step 1: Write the failing test**

Append to `bins/controller/src/lease.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const TTL_MS: u64 = TTL.as_millis() as u64;
    fn v(holder: &str, transitions: u32, renewed_ms: u64) -> View {
        View { holder: holder.into(), transitions, renewed_ms }
    }

    /// No object at all: the first pod to look takes it, and the epoch starts at 1 so "epoch 0"
    /// can never be a real elected term.
    #[test]
    fn an_absent_lease_is_acquired_at_epoch_one() {
        assert_eq!(decide(1_000, "ctl-a", None), Step::Acquire { epoch: 1 });
    }

    /// Ours and live: renew under the SAME epoch. A renew that advanced the epoch would fence
    /// our own in-flight writes every five seconds.
    #[test]
    fn our_own_live_lease_is_renewed_under_the_same_epoch() {
        let cur = v("ctl-a", 3, 10_000);
        assert_eq!(decide(10_000 + TTL_MS - 1, "ctl-a", Some(&cur)), Step::Renew { epoch: 3 });
    }

    /// Somebody else's and still live: wait. This is what the TTL MEANS — never the clock's
    /// opinion of who is healthier.
    #[test]
    fn a_live_lease_held_by_another_pod_is_never_taken() {
        let cur = v("ctl-b", 3, 10_000);
        assert_eq!(decide(10_000 + TTL_MS - 1, "ctl-a", Some(&cur)), Step::Wait);
    }

    /// Expired on the TAKER's own read: take it, epoch advanced, which is what demotes the old
    /// holder's next write.
    #[test]
    fn an_expired_lease_is_taken_with_the_next_epoch() {
        let cur = v("ctl-b", 3, 10_000);
        assert_eq!(decide(10_000 + TTL_MS + 1, "ctl-a", Some(&cur)), Step::Acquire { epoch: 4 });
    }

    /// Our own, expired (a long stall, a paused process): re-acquire rather than renew — the
    /// epoch must advance, because any other pod was free to take it during the gap.
    #[test]
    fn our_own_expired_lease_is_re_acquired_not_renewed() {
        let cur = v("ctl-a", 3, 10_000);
        assert_eq!(decide(10_000 + TTL_MS + 1, "ctl-a", Some(&cur)), Step::Acquire { epoch: 4 });
    }

    /// Exactly at the boundary the lease is still LIVE: the expiry is `renewed + duration`, and
    /// treating `==` as expired would let two pods take it in the same millisecond.
    #[test]
    fn the_expiry_boundary_is_still_held() {
        let cur = v("ctl-b", 1, 10_000);
        assert_eq!(decide(10_000 + TTL_MS, "ctl-a", Some(&cur)), Step::Wait);
    }

    /// A holder whose `renewTime` is in the FUTURE (a skewed peer) is treated as live, never as
    /// expired: a subtraction that underflowed would read as "expired long ago" and steal it.
    #[test]
    fn a_future_renew_time_reads_as_live() {
        let cur = v("ctl-b", 1, 20_000);
        assert_eq!(decide(10_000, "ctl-a", Some(&cur)), Step::Wait);
    }

    /// An epoch check refuses a write made under a term that has since ended.
    #[test]
    fn a_demoted_epoch_may_not_write() {
        assert!(may_write(4, Some(&v("ctl-a", 4, 0))));
        assert!(!may_write(3, Some(&v("ctl-b", 4, 0))));
        assert!(!may_write(3, None));
    }
}
```

- [ ] **Step 2: Run the test — it must fail to compile (nothing exists yet)**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo test -p kloudlite-controller-bin --lib lease 2>&1 | tail -20
```

- [ ] **Step 3: Create the crate**

`bins/controller/Cargo.toml`:

```toml
[package]
name = "kloudlite-controller-bin"
version = "0.1.0"
edition = "2021"
license = "SSPL-1.0"

[lib]
name = "kloudlite_controller"
path = "src/lib.rs"

[[bin]]
name = "kloudlite-controller"
path = "src/main.rs"

[dependencies]
tracing = { workspace = true }
metrics = { workspace = true }
kloudlite-core = { path = "../../crates/core" }
kloudlite-workspaces = { path = "../../crates/workspaces" }
kube = { workspace = true }
k8s-openapi = { workspace = true }
# One route, `/healthz`. No TLS, no ws, no object store: this process talks to the API server only.
axum = { workspace = true }
tokio = { workspace = true }
futures = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }

[dev-dependencies]
kloudlite-workspaces = { path = "../../crates/workspaces", features = ["testkit"] }
```

Add `"bins/controller"` to the root `Cargo.toml`'s `members` list, next to `"bins/gateway"`.

- [ ] **Step 4: Write `bins/controller/src/lease.rs`** (above the test module)

```rust
//! Leader election for the cluster controller, on a `coordination.k8s.io/v1` Lease.
//!
//! The same semantics as `crates/storage/src/ownership/lease.rs` — "the store is the arbiter,
//! never the clock and never the ordinal" — with the API server's resourceVersion CAS playing
//! `UpdateVersion` and `leaseTransitions` playing the epoch. Not that module, because it needs an
//! `ObjectStore`: an S3 or Azure credential in a process whose whole design is "the API server is
//! the only dependency", and a second failure domain for no gain.
//!
//! The epoch is the FENCING TOKEN, and it is the thing not to lose. Every write checks the epoch
//! it was elected under (`may_write`); a write that finds a newer `leaseTransitions` demotes
//! rather than finishing. Unlike SlateDB there is no writer fence underneath, so each object's own
//! CAS (a server-side apply under our field manager) is the backstop.

use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use kube::api::{Patch, PatchParams, PostParams};
use kube::{Api, ResourceExt};
use std::time::Duration;

/// One object, one name, one namespace: the cluster's controller lease.
pub const LEASE_NAME: &str = "kloudlite-controller";
pub const LEASE_NAMESPACE: &str = "kube-system";

/// Expiry. A holder that stops renewing is replaced this long after its last `renewTime`, so a
/// crashed controller costs one TTL of convergence and nothing else — every object it owns is
/// level-triggered and already applied.
pub const TTL: Duration = Duration::from_secs(15);
/// Three renews per TTL, `LEADER_RENEW`'s rule: two may be lost to a blip without a handover.
pub const RENEW: Duration = Duration::from_secs(5);

/// What the object says, reduced to the three fields the decision reads.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub holder: String,
    pub transitions: u32,
    pub renewed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Acquire { epoch: u32 },
    Renew { epoch: u32 },
    Wait,
}

/// The whole state machine, clock injected. Every branch is a test above.
pub fn decide(now_ms: u64, me: &str, cur: Option<&View>) -> Step {
    let ttl = TTL.as_millis() as u64;
    match cur {
        None => Step::Acquire { epoch: 1 },
        Some(c) => {
            // `saturating_sub` on the OTHER side of the comparison: a holder whose renewTime is in
            // the future (a skewed peer) must read as live, not as expired an epoch ago.
            let live = now_ms <= c.renewed_ms.saturating_add(ttl);
            match (live, c.holder == me) {
                // Ours and live: same epoch. Advancing it here would fence our own writes.
                (true, true) => Step::Renew { epoch: c.transitions },
                (true, false) => Step::Wait,
                // Expired, ours or not: a takeover is a takeover, and the epoch advances so the
                // previous holder's next write demotes.
                (false, _) => Step::Acquire { epoch: c.transitions + 1 },
            }
        }
    }
}

/// May a write made under `epoch` still land? Read immediately before the write.
pub fn may_write(epoch: u32, cur: Option<&View>) -> bool {
    cur.is_some_and(|c| c.transitions == epoch)
}

pub fn view(l: &Lease) -> Option<View> {
    let s = l.spec.as_ref()?;
    Some(View {
        holder: s.holder_identity.clone().unwrap_or_default(),
        // A Lease written without it is epoch 0; `decide` advances from there like any other.
        transitions: s.lease_transitions.unwrap_or(0) as u32,
        renewed_ms: s.renew_time.as_ref().map(|t| t.0.timestamp_millis() as u64).unwrap_or(0),
    })
}

pub async fn read(api: &Api<Lease>) -> kube::Result<Option<Lease>> {
    api.get_opt(LEASE_NAME).await
}

/// Write the step. Pinned to the resourceVersion we read (`replace`), so two pods that both
/// decided `Acquire` from the same read cannot both win — the loser gets a 409 and waits.
pub async fn write(api: &Api<Lease>, me: &str, step: &Step, cur: Option<&Lease>, now: k8s_openapi::chrono::DateTime<k8s_openapi::chrono::Utc>) -> kube::Result<Option<Lease>> {
    let epoch = match step {
        Step::Wait => return Ok(cur.cloned()),
        Step::Acquire { epoch } | Step::Renew { epoch } => *epoch,
    };
    let spec = LeaseSpec {
        holder_identity: Some(me.to_string()),
        lease_duration_seconds: Some(TTL.as_secs() as i32),
        lease_transitions: Some(epoch as i32),
        renew_time: Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime(now)),
        acquire_time: matches!(step, Step::Acquire { .. })
            .then(|| k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime(now)),
        ..Default::default()
    };
    match cur {
        Some(existing) => {
            let mut next = existing.clone();
            next.spec = Some(spec);
            // `replace` and not a patch: the resourceVersion the object carries IS the CAS, and a
            // merge patch would happily overwrite a term that moved under us.
            match api.replace(LEASE_NAME, &PostParams::default(), &next).await {
                Ok(l) => Ok(Some(l)),
                // Somebody else won this round. Not an error: re-read on the next tick.
                Err(kube::Error::Api(e)) if e.code == 409 => Ok(None),
                Err(e) => Err(e),
            }
        }
        None => {
            let obj = Lease {
                metadata: kube::api::ObjectMeta { name: Some(LEASE_NAME.into()), namespace: Some(LEASE_NAMESPACE.into()), ..Default::default() },
                spec: Some(spec),
            };
            match api.create(&PostParams::default(), &obj).await {
                Ok(l) => Ok(Some(l)),
                Err(kube::Error::Api(e)) if e.code == 409 => Ok(None),
                Err(e) => Err(e),
            }
        }
    }
}

/// Release on shutdown so a rolling replacement is elected immediately instead of waiting out the
/// TTL: blank the holder, keep the epoch. Best effort — a pod that dies hard is the TTL's case.
pub async fn release(api: &Api<Lease>, me: &str) {
    let Ok(Some(cur)) = read(api).await else { return };
    if view(&cur).is_some_and(|v| v.holder != me) {
        return;
    }
    let patch = serde_json::json!({ "spec": { "holderIdentity": "", "renewTime": null } });
    if let Err(e) = api.patch(LEASE_NAME, &PatchParams::default(), &Patch::Merge(&patch)).await {
        tracing::warn!(lease = %cur.name_any(), error = %e, "leader.release.failed");
    }
}
```

Stub `bins/controller/src/lib.rs` for this task:

```rust
//! `kloudlite-controller`: the cluster's single elected writer of every object that is shared
//! across nodes or derived purely from spec. See
//! `docs/superpowers/specs/2026-09-14-cluster-controller-design.md`.

pub mod lease;
```

Stub `bins/controller/src/main.rs`:

```rust
fn main() {
    // Task 2 fills this in.
    println!("kloudlite-controller");
}
```

- [ ] **Step 5: Run the tests — all green**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo test -p kloudlite-controller-bin --lib lease
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo clippy -p kloudlite-controller-bin --all-targets -- -D warnings
```

- [ ] **Step 6: Commit**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && git log -3 --oneline && git status --short
cd /Volumes/kdisk/rustic-git-wt/desktop-login && git add Cargo.toml Cargo.lock bins/controller && git commit -m "Add the cluster controller crate and its leader lease" && git push platform HEAD
```

---

### Task 2: Boot — client, settings, health, logs, the election loop

**Files:**
- Modify: `bins/controller/src/lib.rs`, `bins/controller/src/main.rs`
- Create: `bins/controller/src/ctx.rs`, `bins/controller/src/health.rs`

**Interfaces:**
- Produces: `pub struct Config { pub region: String, pub holder: String }` with `from_env()`; `pub struct Ctx { client, holder, region, epoch: AtomicU32, applied: Mutex<HashMap<String,(u64, Instant)>>, settings: LiveSettings<AgentSettings>, <reflector stores, added in Task 5> }`; `pub async fn run(cfg: Config) -> Result<(), String>`; `pub async fn elect(ctx: Arc<Ctx>)`.
- Consumes: `kloudlite_workspaces::k8s::client::bounded_client`, `kloudlite_core::settings::LiveSettings`, `kloudlite_workspaces::settings::AgentSettings`.

- [ ] **Step 1: Write the failing test**

Append to `bins/controller/src/ctx.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The holder identity is the POD NAME, which is what makes "who holds the lease" answerable
    /// from `kubectl get lease` alone. An empty `POD_NAME` is a boot failure, not a blank holder:
    /// two pods with the same (empty) identity would each read the other's lease as their own and
    /// both write.
    #[test]
    fn a_blank_holder_is_refused() {
        assert!(Config::check("centralindia-k3s", "kloudlite-controller-abc").is_ok());
        assert!(Config::check("centralindia-k3s", "").is_err());
        assert!(Config::check("", "kloudlite-controller-abc").is_err());
    }

    /// The epoch guard is a process-wide fact, not a parameter threaded through every call site:
    /// a write path asks the Ctx.
    #[test]
    fn the_ctx_remembers_the_term_it_was_elected_under() {
        let ctx = Ctx::for_test();
        assert_eq!(ctx.epoch(), 0);
        assert!(!ctx.leading());
        ctx.promote(4);
        assert_eq!(ctx.epoch(), 4);
        assert!(ctx.leading());
        ctx.demote("fenced");
        assert!(!ctx.leading());
    }
}
```

- [ ] **Step 2: Run it — fails**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo test -p kloudlite-controller-bin --lib ctx 2>&1 | tail -20
```

- [ ] **Step 3: Write `bins/controller/src/ctx.rs`**

Above the tests:

```rust
//! Everything one pass of this process needs, and the term it is allowed to write under.

use kloudlite_core::settings::LiveSettings;
use kloudlite_workspaces::settings::AgentSettings;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

/// Env-derived config. No secret, by design: this process holds none.
pub struct Config {
    /// `WS_REGION` — logged and stamped, never used to filter: one controller per cluster means
    /// every object it can see is its own.
    pub region: String,
    /// `POD_NAME`, from the downward API. The lease's `holderIdentity`.
    pub holder: String,
}

impl Config {
    pub fn from_env() -> Result<Config, String> {
        let region = std::env::var("WS_REGION").unwrap_or_default();
        let holder = std::env::var("POD_NAME").unwrap_or_default();
        Config::check(&region, &holder)?;
        Ok(Config { region, holder })
    }

    pub fn check(region: &str, holder: &str) -> Result<(), String> {
        if region.is_empty() {
            return Err("WS_REGION is required".into());
        }
        if holder.is_empty() {
            return Err("POD_NAME is required: it is the lease holder identity".into());
        }
        Ok(())
    }
}

pub struct Ctx {
    pub client: kube::Client,
    pub holder: String,
    pub region: String,
    /// The term this process was elected under, 0 when it is not the leader. Read immediately
    /// before every write (`lease::may_write`); a stale term demotes instead of finishing.
    epoch: AtomicU32,
    /// `ensure`'s memory, exactly as the agent's (`bins/agent/src/controller/status.rs`), except
    /// that with ONE writer per cluster it is now the truth for the whole cluster rather than one
    /// of N per-process guesses.
    pub applied: Mutex<HashMap<String, (u64, std::time::Instant)>>,
    pub settings: LiveSettings<AgentSettings>,
}

impl Ctx {
    pub fn epoch(&self) -> u32 {
        self.epoch.load(Ordering::SeqCst)
    }
    pub fn leading(&self) -> bool {
        self.epoch() != 0
    }
    pub fn promote(&self, epoch: u32) {
        if self.epoch.swap(epoch, Ordering::SeqCst) != epoch {
            tracing::info!(holder = %self.holder, epoch, "leader.acquired");
        }
    }
    pub fn demote(&self, reason: &str) {
        let was = self.epoch.swap(0, Ordering::SeqCst);
        if was != 0 {
            tracing::info!(holder = %self.holder, epoch = was, reason, "leader.lost");
        }
    }
}
```

`Ctx::for_test` goes behind `#[cfg(test)]` in the same file, building a `Ctx` from
`kube::Client::try_default()`-free parts — use `kube::Client::new(tower::service_fn(...), "default")`
if the workspace already has that pattern; otherwise gate the two constructor fields the test does
not read behind `Option` rather than inventing a fake client. Simplest shape that compiles:

```rust
#[cfg(test)]
impl Ctx {
    /// A Ctx with a client that is never called: every test in this module exercises the epoch
    /// guard, which touches no API server.
    fn for_test() -> Ctx {
        let svc = tower::service_fn(|_req: http::Request<kube::client::Body>| async {
            Err::<http::Response<kube::client::Body>, std::convert::Infallible>(unreachable!())
        });
        Ctx {
            client: kube::Client::new(svc, "default"),
            holder: "ctl-test".into(),
            region: "test".into(),
            epoch: AtomicU32::new(0),
            applied: Mutex::new(HashMap::new()),
            settings: LiveSettings::new(AgentSettings::from_env()),
        }
    }
}
```

Add `tower` and `http` to `[dev-dependencies]` in `bins/controller/Cargo.toml` (`{ workspace = true }`) if `cargo test` reports them missing.

- [ ] **Step 4: Write `bins/controller/src/health.rs`**

```rust
//! One route. The probe answers as soon as the process is up — NOT only while it is the leader:
//! a follower is healthy, it is simply not writing, and failing its probe would restart the one
//! pod that is about to take over.

use std::sync::Arc;

pub fn app(ctx: Arc<crate::ctx::Ctx>) -> axum::Router {
    axum::Router::new().route(
        "/healthz",
        axum::routing::get(move || {
            let ctx = ctx.clone();
            async move {
                // The role is in the BODY, not the status: a 503 for a follower would flap the
                // Deployment's readiness on every ordinary handover.
                if ctx.leading() {
                    format!("leader epoch={}\n", ctx.epoch())
                } else {
                    "follower\n".to_string()
                }
            }
        }),
    )
}
```

- [ ] **Step 5: Write `bins/controller/src/lib.rs`**

```rust
//! `kloudlite-controller`: the cluster's single elected writer of every object shared across nodes
//! or derived purely from spec. Stage 1 owns exactly one thing — a space's two NetworkPolicies —
//! and the node agents stop writing them in the same release.
//!
//! No disk, no object store, no cloud credential, no peer secret: the k3s API server is this
//! process's only dependency, which is also why the lease lives there
//! (`coordination.k8s.io/v1`) rather than in the object store the ownership map uses.
//!
//! One per k3s cluster. AKS has no `Region` CRD, no agents and no `SpaceEnvironment` objects, so
//! nothing of this is deployed there.
//! ponytail: one-per-k3s-cluster is the owner's ruling (2026-09-14; a Region may hold several
//! clusters); a second deployment shape would be a `WS_REGION`-scoped selector, nothing more.

pub mod ctx;
pub mod health;
pub mod lease;
pub mod space;

use std::sync::Arc;

pub use ctx::{Config, Ctx};

pub async fn run(cfg: Config) -> Result<(), String> {
    // Same bounded client the agent and the api tier use: kube's own pool sets no TCP keepalive,
    // and a connection that died without a RST is otherwise handed out and written into nothing
    // (`k8s::client::bounded_client`). A controller whose only dependency is this connection
    // cannot afford that.
    let mut config = kube::Config::infer().await.map_err(|e| e.to_string())?;
    config.read_timeout = Some(std::time::Duration::from_secs(120));
    let client = kloudlite_workspaces::k8s::client::bounded_client(config).map_err(|e| e.to_string())?;

    // `stored ?? env ?? default` through the one handle, never `std::env::var` for a knob that has
    // a `Settings` field. Stage 1 reads nothing from it yet; the wiring is here so the first knob
    // that needs it has somewhere to land and the process does not grow a second settings path.
    let settings = kloudlite_core::settings::LiveSettings::new(initial_settings(&client).await);
    kloudlite_trace::bind_ratio({
        let s = settings.clone();
        move || s.load().trace_sample_ratio
    });

    let ctx = Arc::new(Ctx {
        client: client.clone(),
        holder: cfg.holder,
        region: cfg.region,
        epoch: Default::default(),
        applied: Default::default(),
        settings: settings.clone(),
    });
    spawn_settings_reflector(client, settings);

    let l = tokio::net::TcpListener::bind("0.0.0.0:8080").await.map_err(|e| format!("binding 8080: {e}"))?;
    tracing::info!(listener = "http", addr = "0.0.0.0:8080", holder = %ctx.holder, region = %ctx.region, "listener.started");
    let serving = axum::serve(l, health::app(ctx.clone()));

    tokio::select! {
        r = serving => r.map_err(|e| format!("serving: {e}")),
        _ = elect(ctx.clone()) => Err("election loop ended".into()),
        _ = space::run(ctx.clone()) => Err("reconcilers ended".into()),
    }
}

/// The election beat: read, decide, write, every `RENEW`. It never returns.
///
/// Demotion is driven from HERE and nowhere else: a write path only ASKS (`Ctx::leading`, then
/// `lease::may_write` against a fresh read), because a write that discovers a newer term must
/// abandon itself, not go around re-electing.
pub async fn elect(ctx: Arc<Ctx>) {
    let api: kube::Api<k8s_openapi::api::coordination::v1::Lease> =
        kube::Api::namespaced(ctx.client.clone(), lease::LEASE_NAMESPACE);
    let mut tick = tokio::time::interval(lease::RENEW);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let cur = match lease::read(&api).await {
            Ok(c) => c,
            Err(e) => {
                // Unreachable API server: we stop claiming to lead, because our term may have
                // been taken while we could not look. Keep-biased in the only direction that is
                // safe here — a follower writes nothing, and nothing it owns degrades.
                ctx.demote("lease.unreadable");
                tracing::warn!(error = %e, "leader.read.failed");
                continue;
            }
        };
        let view = cur.as_ref().and_then(lease::view);
        let now = k8s_openapi::chrono::Utc::now();
        match lease::decide(now.timestamp_millis() as u64, &ctx.holder, view.as_ref()) {
            lease::Step::Wait => ctx.demote("held elsewhere"),
            step => match lease::write(&api, &ctx.holder, &step, cur.as_ref(), now).await {
                Ok(Some(l)) => match lease::view(&l) {
                    Some(v) if v.holder == ctx.holder => ctx.promote(v.transitions),
                    _ => ctx.demote("lost the write"),
                },
                // A 409: another pod won this round. Next tick re-reads.
                Ok(None) => ctx.demote("lost the CAS"),
                Err(e) => {
                    ctx.demote("lease.unwritable");
                    tracing::warn!(error = %e, "leader.write.failed");
                }
            },
        }
    }
}
```

`initial_settings` and `spawn_settings_reflector` are the agent's, copied verbatim from
`bins/agent/src/lib.rs:374-440` with the `stall_dump`/`Ctx::new` lines dropped — one GET of
`ClusterSettings/default` at boot, then a watch plus a `SETTINGS_REFRESH_SECS` re-GET. Do not
factor the agent's copy out in this task; that is a refactor of a file this plan otherwise only
deletes from, and it would collide with the concurrent branches.

- [ ] **Step 6: Write `bins/controller/src/main.rs`**

```rust
//! `kloudlite-controller`: one leader-elected process per k3s cluster.

use kloudlite_controller::{run, Config};

#[tokio::main]
async fn main() {
    kloudlite_core::log::init();
    kloudlite_core::metrics::init();
    kloudlite_core::metrics::serve_if_configured().await;
    // Exactly one rustls CryptoProvider before the first handshake — which for this binary is the
    // kube client. Its absence is a panic inside rustls naming nothing about startup order; it
    // crash-looped the api binary once.
    let _ = rustls::crypto::ring::default_provider().install_default();
    use kloudlite_core::metrics::Kind::*;
    kloudlite_core::metrics::register(&[
        ("reconciles_total", Counter, &[("kind", "space"), ("result", "error")]),
        ("reconciles_total", Counter, &[("kind", "environment"), ("result", "error")]),
        ("reconcile_duration_seconds", Histogram, &[]),
    ]);
    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "process.exiting");
            std::process::exit(1);
        }
    };
    if let Err(e) = run(cfg).await {
        tracing::error!(error = %e, "process.exiting");
        std::process::exit(1);
    }
}
```

Add `rustls = { workspace = true }` to `[dependencies]`.

Leave `pub mod space;` as an empty `pub async fn run(_ctx: std::sync::Arc<crate::Ctx>) { std::future::pending::<()>().await }` placeholder in this task — Task 5 fills it.

- [ ] **Step 7: Tests and lint**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo test -p kloudlite-controller-bin
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo clippy -p kloudlite-controller-bin --all-targets -- -D warnings
```

- [ ] **Step 8: Commit**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && git log -3 --oneline
cd /Volumes/kdisk/rustic-git-wt/desktop-login && git add bins/controller Cargo.lock && git commit -m "Boot the cluster controller with a health route and the election beat" && git push platform HEAD
```

---

### Task 3: Image and CI

**Files:**
- Modify: `Dockerfile`, `.github/workflows/image.yml`, `deploy/pin.sh`

**Interfaces:** produces `ghcr.io/kloudlite/kloudlite-controller:<sha>`, built from the same commit as every other Rust binary.

- [ ] **Step 1: Add the Dockerfile stage**

Copy the `gateway` stage (`Dockerfile:84-103`) verbatim, changing only the stage name, the COPY and
the ENTRYPOINT, and DROPPING the `setcap NET_BIND_SERVICE` line if the gateway stage carries one —
this process binds 8080 and never a privileged port:

```dockerfile
# The cluster controller. No capability, no hostPath, no secret: the API server is its only
# dependency, and its one listener is the health route on 8080.
FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241 AS controller
ARG PROFILE=release
RUN adduser --system --uid 1001 --group kloudlite
COPY target/${PROFILE}/kloudlite-controller /usr/local/bin/kloudlite-controller
USER kloudlite
ENTRYPOINT ["kloudlite-controller"]
```

Match the gateway stage's `apt-get`/CA-certificate lines exactly if it has them — read it first
(`sed -n 84,103p Dockerfile`) and mirror, don't invent.

- [ ] **Step 2: Build it in CI**

In `.github/workflows/image.yml`:
- `build` job's `cargo build` line: add `--bin kloudlite-controller`.
- the `glibc ceiling` step's `for b in …` list: add `kloudlite-controller`.
- the `bins` upload-artifact `path:` list: add `/ci-target/release/kloudlite-controller`.
- after the `gateway` push step, a new `docker/build-push-action` block with `target: controller`
  and tags `ghcr.io/kloudlite/kloudlite-controller:latest` / `:${{ github.sha }}`, same pinned
  action SHA as its neighbours.

- [ ] **Step 3: Pin it**

In `deploy/pin.sh`: add `kloudlite-controller` to the `for img in …` list, and after the gateway
line:

```sh
pin 'kloudlite-controller' "$SHA" "${DIGEST[kloudlite-controller]}" k3s/controller.yaml
```

Add `deploy/k3s/controller.yaml` to the closing heredoc's `kubectl apply -f` line.

Update the header contract comment: it says "Six images, two SHAs" — it is now seven.

- [ ] **Step 4: Verify the manifest list is consistent**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && grep -n "kloudlite-controller" Dockerfile .github/workflows/image.yml deploy/pin.sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && bash -n deploy/pin.sh
```

- [ ] **Step 5: Commit**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && git log -3 --oneline
cd /Volumes/kdisk/rustic-git-wt/desktop-login && git add Dockerfile .github/workflows/image.yml deploy/pin.sh && git commit -m "Build and pin the cluster controller image" && git push platform HEAD
```

---

### Task 4: Deploy manifest and RBAC

**Files:**
- Create: `deploy/k3s/controller.yaml`, `deploy/k3s/controller-rbac.yaml`
- Modify: `deploy/k3s/README.md`, `crates/workspaces/src/api/workloads.rs`

**Interfaces:** `ServiceAccount kloudlite-controller` in `kube-system`; the ClusterRole below and nothing more.

- [ ] **Step 1: Write the failing test**

In `crates/workspaces/src/api/workloads.rs`'s test module (or create one at the file's end):

```rust
    /// A roll target's namespace is per NAME, not per cluster — the agent's DaemonSet and the
    /// controller are cluster infra in `kube-system`, the gateway has its own namespace. A
    /// controller resolved into `kloudlite-system` would patch nothing and report "rolled".
    #[test]
    fn the_controller_is_a_per_region_deployment_in_kube_system() {
        let scope = Scope::Region("centralindia-k3s".into());
        assert_eq!(resolve(&scope, "kloudlite-controller"), Some(Kind::Deployment));
        assert_eq!(namespace(&scope, "kloudlite-controller"), "kube-system");
        assert_eq!(namespace(&scope, "kloudlite-gateway"), "kloudlite-system");
        // Never central: AKS runs no cluster controller.
        assert_eq!(resolve(&Scope::Central, "kloudlite-controller"), None);
    }
```

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo test -p kloudlite-workspaces the_controller_is_a_per_region 2>&1 | tail -20
```

- [ ] **Step 2: Make it pass**

In `crates/workspaces/src/api/workloads.rs`:
- `KNOWN_PER_REGION` gains `("kloudlite-controller", Kind::Deployment),`.
- `namespace()`'s region arm: `Scope::Region(_) if name == "kloudlite-agent" || name == "kloudlite-controller" => "kube-system",`.

This is what lets a `Mark::Boot` settings save roll the controller (`admin::workloads::KNOWN`, the
spec's "admin/workloads gains the Deployment").

- [ ] **Step 3: Write `deploy/k3s/controller-rbac.yaml`**

```yaml
# RBAC for the cluster controller (`kloudlite-controller`).
#
# THE TABLE BELOW IS THE ROLE, same contract as agent-rbac.yaml: every Kubernetes call this binary
# makes, with its call site. A verb not in the table is not in the rules, and a call added without
# a row fails with a 403 naming this file.
#
# Stage 1 only. Every other row in the spec's single-writer table arrives with its own stage, and
# an unused verb granted early is a verb nobody notices being wrong.
#
#   resource (group)                       verb(s)                 call site
#   ---------------------------------------------------------------------------------------------
#   spaceenvironments (kloudlite.io)       get,list,watch          the space reflector; the wish
#                                                                  itself. READ ONLY — /v1 is the
#                                                                  one writer, here as in the agent
#   environments (kloudlite.io)            get,list,watch          the environment reflector: the
#                                                                  ingress half's namespace, its
#                                                                  region, its ownerReference
#   workspaces, benches (kloudlite.io)     get,list,watch          space::render's owner lookup and
#                                                                  the legacy `attach-*` collection
#   networkpolicies (networking.k8s.io)    create,patch            space_egress / space_ingress via
#                                                                  server-side apply
#                                          list                    the env-side derived-set render:
#                                                                  which space-* halves exist here
#                                          delete                  a switch removes the old half; a
#                                                                  cleared choice removes both
#   leases (coordination.k8s.io)           get,create,update       lease.rs: elect, renew, release.
#                                                                  Namespaced to kube-system, by
#                                                                  NAME — see the Role below
#
# What the controller deliberately does NOT get in stage 1: namespaces, limitranges,
# resourcequotas, rolebindings, services, statefulsets, pods, and any `/status` subresource. Those
# are stages 2 and 3.
apiVersion: v1
kind: ServiceAccount
metadata:
  name: kloudlite-controller
  namespace: kube-system
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: kloudlite-controller
rules:
  - apiGroups: ["kloudlite.io"]
    resources: ["spaceenvironments", "environments", "workspaces", "benches"]
    verbs: ["get", "list", "watch"]
  # NetworkPolicies are namespaced, and the namespaces are `ws-*`, `wt-*` and `env-*` — RBAC has
  # no globbing, so "only those namespaces" is not expressible as a rule. A Role per namespace
  # would need something to write it, in every namespace, before the first policy: that something
  # would be this controller, holding namespace-create it otherwise does not need. Cluster-wide
  # on exactly one resource is the narrower of the two.
  # ponytail: cluster-scoped NetworkPolicy write; narrows to per-namespace Roles if stage 2 (which
  # owns namespace creation) makes writing them free.
  - apiGroups: ["networking.k8s.io"]
    resources: ["networkpolicies"]
    verbs: ["create", "patch", "list", "delete"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: kloudlite-controller
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: kloudlite-controller
subjects:
  - kind: ServiceAccount
    name: kloudlite-controller
    namespace: kube-system
---
# The lease, by name, in one namespace: a namespaced Role, never the ClusterRole above. `create`
# is needed once, on a cluster that has never run a controller; `update` is the renew and the
# takeover (the resourceVersion on it is the CAS).
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: kloudlite-controller-lease
  namespace: kube-system
rules:
  - apiGroups: ["coordination.k8s.io"]
    resources: ["leases"]
    verbs: ["create"]
  - apiGroups: ["coordination.k8s.io"]
    resources: ["leases"]
    resourceNames: ["kloudlite-controller"]
    verbs: ["get", "update", "patch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: kloudlite-controller-lease
  namespace: kube-system
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: kloudlite-controller-lease
subjects:
  - kind: ServiceAccount
    name: kloudlite-controller
    namespace: kube-system
```

- [ ] **Step 4: Write `deploy/k3s/controller.yaml`**

```yaml
# The cluster controller: one leader-elected writer per k3s cluster.
#
# ONE replica, `Recreate`, no PDB, and each of those is a decision:
#   - a second replica is a hot standby that only shortens a 15 s failover, and every object this
#     process owns is level-triggered — while it is down nothing converges and nothing breaks;
#   - `Recreate` plus the lease bounds a roll to one TTL (the departing pod releases the lease on
#     shutdown, so the usual handover is seconds, not the full TTL);
#   - a PDB with `minAvailable: 1` on a one-replica Deployment BLOCKS node drains, which is an
#     operation this fleet performs deliberately (`decommission`), so it would cost more than the
#     failover it buys.
# Add the second replica if a measured failover ever hurts.
#
# `kube-system`, not `kloudlite-system`: this is cluster infra like the agent DaemonSet, and it
# accepts no connection from anywhere (the gateway's namespace exists because that one does).
apiVersion: apps/v1
kind: Deployment
metadata:
  name: kloudlite-controller
  namespace: kube-system
  labels:
    app: kloudlite-controller
spec:
  replicas: 1
  strategy:
    type: Recreate
  selector:
    matchLabels:
      app: kloudlite-controller
  template:
    metadata:
      labels:
        app: kloudlite-controller
      annotations:
        prometheus.io/scrape: "true"
        prometheus.io/port: "9464"
        prometheus.io/path: /metrics
    spec:
      serviceAccountName: kloudlite-controller
      containers:
        - name: controller
          image: ghcr.io/kloudlite/kloudlite-controller:latest
          imagePullPolicy: IfNotPresent
          ports:
            - name: http
              containerPort: 8080
          env:
            - name: KLOUDLITE_LOG_FORMAT
              value: json
            - name: KLOUDLITE_OTLP_URL
              value: http://kloudlite-otel-agent-otlp.kube-system.svc:4318
            - name: OTEL_SERVICE_NAME
              value: kloudlite-controller
            - name: KLOUDLITE_METRICS_ADDR
              value: 0.0.0.0:9464
            # The lease's holderIdentity. Downward API, never a generated string: `kubectl get
            # lease kloudlite-controller -n kube-system` must name a pod somebody can look at.
            - name: POD_NAME
              valueFrom:
                fieldRef:
                  fieldPath: metadata.name
            - name: WS_REGION
              value: centralindia-k3s
            - name: RUST_BACKTRACE
              value: "1"
          resources:
            requests:
              cpu: 50m
              memory: 128Mi
            limits:
              memory: 512Mi
          # Liveness only, and it passes for a FOLLOWER too: a follower is healthy, it is simply
          # not writing, and failing its probe would restart the pod that is about to take over.
          # There is no Service and no traffic, so there is nothing a readiness gate would gate.
          livenessProbe:
            httpGet:
              path: /healthz
              port: 8080
            periodSeconds: 20
          securityContext:
            runAsUser: 1001
            runAsGroup: 1001
            runAsNonRoot: true
            readOnlyRootFilesystem: true
            allowPrivilegeEscalation: false
            capabilities:
              drop: ["ALL"]
```

Replace `:latest` with a real pin before the first roll (`deploy/pin.sh <sha>` does it).

- [ ] **Step 5: Document the apply order in `deploy/k3s/README.md`**

Add, beside the existing apply instructions:

```md
### Rolling the space policies onto the controller (stage 1, one release)

Order matters and is not the usual one:

1. `kubectl apply -f deploy/k3s/agent-daemonset.yaml` and wait for the DaemonSet to report fully
   rolled. The new agent no longer writes `space-env`/`space-{ns}`; the policies it already wrote
   stay exactly where they are, so nothing loses its grant during the roll.
2. `kubectl apply -f deploy/k3s/controller-rbac.yaml -f deploy/k3s/controller.yaml`. The
   controller adopts the existing objects on its first pass (a forced server-side apply moves the
   field manager from `kloudlite-agent` to `kloudlite-controller`; identical bytes, so no pod sees
   a change).
3. `kubectl apply -f deploy/k3s/agent-rbac.yaml` LAST. It removes the agent's NetworkPolicy write
   verbs. Applied before step 1, a still-writing agent gets a 403 and aborts its whole reconcile —
   the same failure as the missing `networkpolicies: delete` verb on 2026-09-11, which stopped
   every environment on the fleet converging for eight minutes.

Rollback is the reverse: re-apply the old `agent-rbac.yaml`, scale the controller to 0, roll the
previous agent image.
```

- [ ] **Step 6: Verify**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo test -p kloudlite-workspaces the_controller_is_a_per_region
cd /Volumes/kdisk/rustic-git-wt/desktop-login && python3 -c "import sys,yaml;[list(yaml.safe_load_all(open(f))) for f in ['deploy/k3s/controller.yaml','deploy/k3s/controller-rbac.yaml']];print('ok')"
```

- [ ] **Step 7: Commit**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && git log -3 --oneline
cd /Volumes/kdisk/rustic-git-wt/desktop-login && git add deploy/k3s/controller.yaml deploy/k3s/controller-rbac.yaml deploy/k3s/README.md crates/workspaces/src/api/workloads.rs && git commit -m "Deploy the cluster controller with a stage-one role" && git push platform HEAD
```

---

### Task 5: The controller renders the space policies

**Files:**
- Modify: `bins/controller/src/space.rs`, `bins/controller/src/ctx.rs`
- Test: in-crate `#[cfg(test)] mod tests` in `space.rs`

**Interfaces:**
- Produces: `pub enum Choice { Unknown, None, Env(Arc<crd::Environment>, Arc<crd::SpaceEnvironment>) }`; `pub fn desired(space: &crd::SpaceEnvironment, env: Option<&crd::Environment>, region: &str) -> Desired`; `pub struct Desired { pub egress_ns: String, pub env_ns: Option<String> }`; `pub async fn reconcile_space(...)`, `pub async fn reconcile_environment(...)`, `pub async fn run(ctx: Arc<Ctx>)`.
- Consumes: `k8s::{space_egress, space_ingress, SPACE_EGRESS_POLICY, space_ingress_name}`, `crd::{ws_namespace, env_namespace, space_name}`.

- [ ] **Step 1: Write the failing tests**

In `bins/controller/src/space.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use kloudlite_workspaces::crd;

    fn space(owner: &str, team: &str, env: &str) -> crd::SpaceEnvironment {
        let mut s = crd::SpaceEnvironment::new(
            &crd::space_name(owner, team),
            crd::SpaceEnvironmentSpec { owner: owner.into(), team: team.into(), environment: env.into() },
        );
        s.metadata.uid = Some("uid-space".into());
        s
    }

    fn env(name: &str, owner: &str, region: &str) -> crd::Environment {
        let mut e = crd::Environment::new(name, crd::EnvironmentSpec { owner: owner.into(), region: region.into(), ..Default::default() });
        e.metadata.uid = Some("uid-env".into());
        e
    }

    /// The whole derived set, from the wish alone: one egress half in the space's namespace and
    /// one ingress half in the environment's, both named the way `policies.rs` names them.
    #[test]
    fn a_choice_renders_exactly_two_halves() {
        let d = desired(&space("alice", "acme", "env-1"), Some(&env("env-1", "acme", "r1")), "r1");
        assert_eq!(d.egress_ns, "wt-alice-acme");
        assert_eq!(d.env_ns.as_deref(), Some("env-env-1"));
    }

    /// An environment in ANOTHER region is not this controller's to grant: the pods are in a
    /// different cluster and the namespace here would not exist.
    #[test]
    fn an_environment_in_another_region_grants_nothing() {
        let d = desired(&space("alice", "acme", "env-1"), Some(&env("env-1", "acme", "r2")), "r1");
        assert_eq!(d.env_ns, None);
    }

    /// A gone environment likewise — and the egress namespace is still known, which is what lets
    /// the caller delete the stale egress half rather than leaving the space pointed at nothing.
    #[test]
    fn a_missing_environment_grants_nothing_but_still_names_the_space() {
        let d = desired(&space("alice", "acme", "env-1"), None, "r1");
        assert_eq!(d.egress_ns, "wt-alice-acme");
        assert_eq!(d.env_ns, None);
    }

    /// The bytes themselves come from the one definition in `policies.rs` and are unchanged by
    /// this move — if they were not, an adopting apply would rewrite every policy in the cluster.
    #[test]
    fn the_rendered_bytes_are_the_shared_definition() {
        let s = space("alice", "acme", "env-1");
        let r = owner_ref(&s);
        let eg = k8s::space_egress("wt-alice-acme", "env-env-1", "alice", &r);
        assert_eq!(eg.metadata.name.as_deref(), Some(k8s::SPACE_EGRESS_POLICY));
        assert_eq!(eg.metadata.namespace.as_deref(), Some("wt-alice-acme"));
        let ing = k8s::space_ingress("env-env-1", "wt-alice-acme", "alice", &r);
        assert_eq!(ing.metadata.name.as_deref(), Some(&k8s::space_ingress_name("wt-alice-acme")[..]));
        assert_eq!(ing.metadata.namespace.as_deref(), Some("env-env-1"));
    }

    /// The env-side derived set: which `space-*` halves belong in THIS namespace, given the cache.
    /// A space pointing elsewhere is collected; a space pointing here is kept; and a cache that
    /// has not listed keeps EVERYTHING — read as empty it would strip every grant in the cluster
    /// on each controller restart.
    #[test]
    fn the_env_side_keeps_only_the_spaces_that_point_here() {
        let here = vec![space("alice", "acme", "env-1"), space("bob", "acme", "env-2")];
        let keep = kept_here("env-1", Some(&here));
        assert!(keep.contains(&k8s::space_ingress_name("wt-alice-acme")));
        assert!(!keep.contains(&k8s::space_ingress_name("wt-bob-acme")));
        // Unknown: the caller must not prune at all, which is `None`, not an empty set.
        assert!(kept_all_unknown(None));
    }

    /// The fan-out bound, which is the whole reason this moved: one wish event touches the space's
    /// own namespace and at most two environment namespaces (the new choice and the old one).
    #[test]
    fn one_wish_event_touches_at_most_three_namespaces() {
        let touched = touched_namespaces(&space("alice", "acme", "env-2"), Some("env-1"));
        assert_eq!(touched, vec!["wt-alice-acme", "env-env-2", "env-env-1"]);
    }
}
```

Adapt the constructor calls to the real `crd::SpaceEnvironmentSpec`/`EnvironmentSpec` shapes —
read `crates/workspaces/src/crd/space.rs` and `crd/mod.rs` first; the assertions, not the
constructors, are the contract.

- [ ] **Step 2: Run them — fail**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo test -p kloudlite-controller-bin --lib space 2>&1 | tail -30
```

- [ ] **Step 3: Add the reflectors to `Ctx`**

Three stores plus their writers, the agent's shape exactly
(`bins/agent/src/controller/mod.rs:274-283`, `:398-408`, `store_ready` at `:447`), and `remember_*`
seeders behind `#[cfg(test)]`:

```rust
    /// EVERY `SpaceEnvironment`, `Environment` and `Workspace` in the cluster. Unfiltered: one
    /// process decides for the whole cluster, which is the point.
    ///
    /// Read through `spaces()`/`environments()`/`workspaces()` and NEVER without `store_ready`. An
    /// unlisted cache is UNKNOWN, never empty — read as empty, the env-side prune below would
    /// delete every grant in the cluster on each controller restart.
    pub space_store: kube::runtime::reflector::Store<crd::SpaceEnvironment>,
```

`ensure` and `forget_applied` come across too, byte-for-byte from
`bins/agent/src/controller/status.rs:284-320`, with `crd::AGENT_FIELD_MANAGER` replaced by a new
`crd::CONTROLLER_FIELD_MANAGER` (`"kloudlite-controller"`, added beside the others at
`crates/workspaces/src/crd/mod.rs:72`). Keep `.force()`: it is what ADOPTS the objects the agents
wrote, in one apply, with identical bytes and therefore no observable change.

- [ ] **Step 4: Write `bins/controller/src/space.rs`**

Module doc first — this is the file a 3am reader lands on:

```rust
//! The cluster's space grants: `space-env` in the space's namespace, `space-{ns}` in the
//! environment's, and nothing else in stage 1.
//!
//! ONE writer, which is what deletes a whole class of bug rather than patching instances of it.
//! Before this, every node hosting a pod of a space wrote both halves from its own cache: F1 (a
//! deleted policy was unrecreatable for 600 s because the delete path never called
//! `forget_applied`), F2 (the env-side prune read one node's cache against another's trigger), F3
//! (a per-pod condition gate deleted the namespace-wide `space-env`) and F6 (multi-writer flap
//! under cache lag) were all the same shape. Here there is no per-pod gate at all: the two halves
//! are a pure function of the `SpaceEnvironment`.
//!
//! Fan-out is bounded ON PURPOSE. One wish event re-renders that space's egress half and the
//! ingress half in at most two environments — the one it names now and the one it named before —
//! never "every environment", which is what `run.rs`'s `all_in_store` mapper did on every node.
//!
//! An unlisted cache is UNKNOWN: decide nothing, delete nothing. That rule matters more here than
//! in an agent, because this process decides for the whole cluster at once.
```

Then, in order:

1. `desired(space, env, region) -> Desired` — pure, no I/O. `env_ns` is `Some` only when the
   environment exists AND `spec.region == region`.
2. `owner_ref(space)` / `owner_ref(env)` — `owner_ref_of_kind`'s shape from
   `bins/agent/src/controller/volume.rs`; the egress half is owned by the `SpaceEnvironment`, the
   ingress half by the `Environment` (an ownerReference cannot cross namespaces, and the ingress
   half lives in the env namespace).
3. `reconcile_space(space, ctx)`:
   - `ctx.leading()` and `lease::may_write(ctx.epoch(), …)` against a fresh lease read; not the
     leader → return, write nothing.
   - resolve the environment from `ctx.environments()`; `None` (unlisted) → `reconcile.pass` with
     `decision = "unknown"`, return.
   - `ensure` the egress half when `env_ns` is `Some`; otherwise delete `SPACE_EGRESS_POLICY` in
     the space namespace + `forget_applied`.
   - `ensure` the ingress half in `env_ns`.
   - delete the ingress half in the PREVIOUS environment's namespace + `forget_applied`. The
     previous choice comes from `ctx.last_choice: Mutex<HashMap<String, String>>` (space name →
     env id), updated at the end of each pass; a controller that just started has no memory, which
     is exactly what the env-side reconciler below is for.
   - one log line per pass: `tracing::info!(space, environment, env_ns, peers, "grant.rendered")`.
     The vet's 3am note was that `space.grant.pruned` never said which value it read; a
     derived-set render says what it rendered and from what.
4. `reconcile_environment(env, ctx)` — the restart-safe half: list `NetworkPolicy` in the env
   namespace, and for each name starting `space-` that is not in `kept_here(env, ctx.spaces())`,
   delete + `forget_applied`. `ctx.spaces()` is `None` → prune nothing. This is
   `prune_attach_grants`'s `space-` arm (`bins/agent/src/controller/environment/mod.rs:64-77`)
   moved and made a derived set rather than a disagreement between two caches.
5. `run(ctx)` — two `kube::runtime::Controller`s joined, `watch_config()` copied from the agent
   (`timeout(60)`), plus the reflector `watcher`s driving the three stores. The `SpaceEnvironment`
   controller maps a `SpaceEnvironment` event to itself, and an `Environment` event to the spaces
   naming it (a store read, bounded by the spaces of that environment — NOT `all_in_store`).

- [ ] **Step 5: Tests and lint**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo test -p kloudlite-controller-bin
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo clippy -p kloudlite-controller-bin --all-targets -- -D warnings
```

- [ ] **Step 6: Commit**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && git log -3 --oneline
cd /Volumes/kdisk/rustic-git-wt/desktop-login && git add bins/controller crates/workspaces/src/crd/mod.rs && git commit -m "Render every space grant from the cluster controller" && git push platform HEAD
```

---

### Task 6: The agent stops writing space policies

**Files:**
- Modify: `bins/agent/src/controller/space.rs`, `bins/agent/src/controller/environment/mod.rs`, `bins/agent/src/controller/run.rs`, `deploy/k3s/agent-rbac.yaml`
- Test: `bins/agent/tests/reconcile/attachment.rs`

**Interfaces:** `converge_space` keeps its signature and its `Attached` return; it loses every `NetworkPolicy` call.

- [ ] **Step 1: Turn the existing assertions around**

In `bins/agent/tests/reconcile/attachment.rs`, every case that asserted a `space-env` or
`space-{ns}` apply becomes the opposite. Read the file first and rewrite each in place; the shape:

```rust
    /// The agent renders the pod's `/etc/resolv.conf` and writes the `Attached` condition, and
    /// NOTHING else: both halves of the grant belong to `kloudlite-controller` since stage 1 of
    /// the cluster-controller split. Two writers across a roll is the one thing that release must
    /// never have.
    #[tokio::test]
    async fn the_agent_writes_no_network_policy_for_a_space() {
        let (ctx, calls) = /* the suite's existing recording harness */;
        ctx.remember_spaces(vec![space("alice", "acme", "env-1")]);
        let out = converge_space(&ctx, pod("ws-1", "alice", "acme")).await.unwrap();
        assert!(matches!(out, Attached::Set(Some(_))));
        assert!(
            !calls.iter().any(|c| c.contains("networkpolicies")),
            "the agent must make no NetworkPolicy call: {calls:?}"
        );
    }
```

- [ ] **Step 2: Run — fails**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo test -p kloudlite-agent --test reconcile attachment 2>&1 | tail -30
```

- [ ] **Step 3: Delete the writes**

In `bins/agent/src/controller/space.rs`:
- delete the whole `policies`/`legacy_name`/`drop_legacy_ingress`/`reason` match arm block
  (`:130-176`) and everything it needs: the `NetworkPolicy` import, `ensure`, `delete_ignoring_404`,
  `owner_ref_of_kind`, `Pod::owner_ref`, `Pod::prev` if nothing else reads it.
- `converge_space` keeps: resolve → `write_resolv` → return the `Attached` condition. `SPACE_REASON`
  stays (the condition's reason is what the web and `/v1` read).
- the legacy per-pod `attach-{id}` pair: that is the spec's "attach-{id} legacy pair → controller
  (collection only), stage 1". It has no writer in the controller yet, so DELETE the agent's
  handling of it too and record it in the module doc as collected by the Environment's own deletion
  (the pair is ownerReferenced) — do not leave a half-owner.
- update the module doc: the agent owns the `resolv.conf` and the condition; the grants are the
  controller's, and the file names the spec.

In `bins/agent/src/controller/environment/mod.rs`: `prune_attach_grants` loses the
`name.strip_prefix("space-")` arm entirely (`:62-77`) — including the `ctx.spaces()` read, if
nothing else in that function uses it.

In `bins/agent/src/controller/run.rs`: delete the `SpaceEnvironment` watch on the environments
controller (`:432-436`) and the now-unused `env_store_for_spaces` clone. The Workspace and Bench
controllers KEEP their `SpaceEnvironment` watches — they still render `resolv.conf` from the wish.

- [ ] **Step 4: Narrow the RBAC**

In `deploy/k3s/agent-rbac.yaml`:
- the header table's `networkpolicies` row: strike the space halves from the call-site column,
  leaving only what the agent still writes. If the agent writes NO NetworkPolicy at all after step 3
  — check with `git grep -n "NetworkPolicy" bins/agent/src` — remove the rule and the row outright
  and say in a comment where those verbs went (`deploy/k3s/controller-rbac.yaml`). If it still
  writes the intercept pair (it does, until Task 8 lands), keep `create,patch,delete,list` and
  narrow the row's prose to the intercept pair only.
- add a line under the table: "Rows removed on 2026-09-14 moved to `deploy/k3s/controller-rbac.yaml`
  with their call sites; apply that file and roll the agents BEFORE applying this one — see
  `deploy/k3s/README.md`."

- [ ] **Step 5: Tests and lint**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo test -p kloudlite-agent
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo clippy -p kloudlite-agent --all-targets -- -D warnings
```

- [ ] **Step 6: Commit**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && git log -3 --oneline
cd /Volumes/kdisk/rustic-git-wt/desktop-login && git add bins/agent deploy/k3s/agent-rbac.yaml && git commit -m "Stop writing space grants from the node agents" && git push platform HEAD
```

---

### Task 7: The mixed-build guard

**Files:**
- Create: `bins/agent/tests/reconcile/agent_writes_no_space_policies.rs`
- Modify: `bins/agent/tests/reconcile/main.rs`

**Interfaces:** none — one test.

- [ ] **Step 1: Write it**

`bins/agent/tests/reconcile/agent_writes_no_space_policies.rs`:

```rust
//! The one assertion that survives a refactor: this binary contains no space-policy writer.
//!
//! Two writers of one object across a roll is the failure stage 1 exists to avoid, and a unit test
//! of one converge path only proves the path it calls. This reads the source of the two modules
//! that used to write the halves, so a re-introduction anywhere in them fails here — including in
//! a branch that never runs in the reconcile suite.
//!
//! The grants are `kloudlite-controller`'s (`bins/controller/src/space.rs`); the agent keeps the
//! `resolv.conf` render and the `Attached` condition, which are host-bound and stay host-bound.

const SPACE: &str = include_str!("../../src/controller/space.rs");
const ENVIRONMENT: &str = include_str!("../../src/controller/environment/mod.rs");

#[test]
fn the_agent_source_names_no_space_policy_builder() {
    for (file, src) in [("controller/space.rs", SPACE), ("controller/environment/mod.rs", ENVIRONMENT)] {
        for needle in ["space_egress", "space_ingress", "SPACE_EGRESS_POLICY", "space_ingress_name"] {
            assert!(
                !src.contains(needle),
                "{file} names `{needle}`: the space grants belong to kloudlite-controller — \
                 see bins/controller/src/space.rs and the spec at \
                 docs/superpowers/specs/2026-09-14-cluster-controller-design.md"
            );
        }
    }
}

/// And the module that used to hold them still does its own job, so this is not passing because
/// somebody deleted the file.
#[test]
fn the_agent_still_renders_the_resolv_conf() {
    assert!(SPACE.contains("write_resolv"), "the agent still owns the per-pod resolv.conf");
}
```

Declare it in `bins/agent/tests/reconcile/main.rs` beside its siblings (`mod agent_writes_no_space_policies;`).

- [ ] **Step 2: Run**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo test -p kloudlite-agent --test reconcile no_space_policy
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo clippy -p kloudlite-agent --all-targets -- -D warnings
```

- [ ] **Step 3: Commit**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && git log -3 --oneline
cd /Volumes/kdisk/rustic-git-wt/desktop-login && git add bins/agent/tests/reconcile && git commit -m "Assert the agent binary carries no space policy writer" && git push platform HEAD
```

---

### Task 8: Intercept objects — BLOCKED, do not start

**Dependency:** `docs/superpowers/plans/2026-09-14-intercept-proxy.md` (spec
`docs/superpowers/specs/2026-09-14-intercept-proxy-design.md`, commit `0c5ca8ae`), which must ship
first.

The cluster-controller spec puts the intercept objects — the `intercept-{service}` proxy pod, the
Service's selector, the StatefulSet scale, and the fixed grant pair — in the controller in stage 1,
"per `2026-09-14-intercept-proxy-design.md`". Those objects change SHAPE in that plan (a proxy pod
in the environment namespace replaces the hand-written `EndpointSlice`), so moving today's writer
(`bins/agent/src/controller/environment/intercept.rs:267-349`) into the controller now would port
code that plan deletes. That plan says the same thing from its own side: it puts every decision in
a pure render module in `crates/workspaces`, leaves the agent's environment reconciler as the only
caller, and states that moving to the cluster controller is then "a change to the CALLER only".

**Do not implement this task from this plan.** After the intercept-proxy plan has shipped, the move
is one new reconciler in `bins/controller/src/space.rs`'s sibling module calling that render module,
and the two facts it needs from here are:

- the controller's `Ctx`, `ensure`/`forget_applied`, epoch guard and reflectors already exist
  (Tasks 2 and 5) — the move is a reconciler, not a process;
- `deploy/k3s/controller-rbac.yaml` gains the rows that move with it (`pods`, `services`,
  `statefulsets`, and `networkpolicies` for the intercept pair), and `agent-rbac.yaml` loses them
  in the same release, under the same apply order as `deploy/k3s/README.md` now records.

Until then the agent keeps writing the intercept objects, unchanged, and keeps the NetworkPolicy
verbs for exactly that (Task 6, step 4).

---

### Task 9: SLO — seven ids, each judged on output

**Files:**
- Create: `bins/slo/src/stages/controller.rs`
- Modify: `bins/slo/src/stages/mod.rs`, `crates/workspaces/src/slo/catalogue.rs`, `deploy/slo.md`

**Interfaces:** consumes the stage harness (`c.step(id, ceiling, closure)`, `c.skip(id, why)`,
`crate::kube::exec`, `c.kube`, `c.probe_jwt`, `c.state.workspace`) exactly as
`bins/slo/src/stages/environment.rs` does.

The rule for every id below: **judge on OUTPUT.** A policy object that exists proves nothing; a
connect that succeeds or is refused does. Two ids are exceptions and say why in their own doc
comment (`ctl.leader` reads the Lease because the lease IS the output; `ctl.agent.nowrite` reads
`managedFields` because "who wrote it" has no other observable).

- [ ] **Step 1: Add the catalogue rows**

In `crates/workspaces/src/slo/catalogue.rs`, a new block after the workspaces rows:

```rust
    // 15 · Cluster controller. The single elected writer of the space grants (stage 1 of
    // `docs/superpowers/specs/2026-09-14-cluster-controller-design.md`). Every id but `ctl.leader`
    // and `ctl.agent.nowrite` judges by CONNECTING, never by reading a policy object: a rendered
    // NetworkPolicy that the CNI never programmed is exactly the outage these exist to catch.
    Slo { id: "ctl.leader", feature: "Cluster controller", sli: "Exactly one controller pod holds the `kloudlite-controller` Lease and has renewed it within its TTL", target: avail(99.9), suite: Suite::Fast, stage: "15 · Cluster controller" },
    Slo { id: "ctl.grant.set", feature: "Cluster controller", sli: "Choosing an environment for a space lets a probe workspace resolve and connect to one of its services", target: bound(30_000), suite: Suite::Fast, stage: "15 · Cluster controller" },
    Slo { id: "ctl.grant.switch", feature: "Cluster controller", sli: "Switching the choice makes the new environment's service reachable and the old one unreachable", target: bound(30_000), suite: Suite::Fast, stage: "15 · Cluster controller" },
    Slo { id: "ctl.grant.cleared", feature: "Cluster controller", sli: "Clearing the choice refuses the connect and leaves no `space-*` policy for that space", target: bound(30_000), suite: Suite::Fast, stage: "15 · Cluster controller" },
    Slo { id: "ctl.failover", feature: "Cluster controller", sli: "Deleting the leader pod elects another within 20 s and a choice made during the gap converges once it is up", target: bound(120_000), suite: Suite::Hourly, stage: "15 · Cluster controller" },
    Slo { id: "ctl.agent.nowrite", feature: "Cluster controller", sli: "Every `space-*` NetworkPolicy in the cluster is managed by `kloudlite-controller` and by no agent", target: avail(99.9), suite: Suite::Hourly, stage: "15 · Cluster controller" },
    Slo { id: "ctl.fanout", feature: "Cluster controller", sli: "One change of a space's choice reconciles at most two environments", target: avail(99.9), suite: Suite::Hourly, stage: "15 · Cluster controller" },
```

Mirror all seven into `deploy/slo.md`'s table in the same order, with the same wording and
`| fast |`/`| hourly |` and `| 15 · Cluster controller |` columns. The equality test
(`the_catalogue_matches_deploy_slo_md`) is what holds them.

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo test -p kloudlite-workspaces the_catalogue_matches_deploy_slo_md
```

- [ ] **Step 2: Write `bins/slo/src/stages/controller.rs`**

```rust
//! 15 · Cluster controller. The controller is invisible from outside — a person never calls it —
//! so every id here is judged the way a person would notice it failing: a workspace that cannot
//! reach its environment, or one that still can after the grant was taken away.
//!
//! Two exceptions, both deliberate: `ctl.leader` reads the Lease, because the lease IS the output
//! being asserted, and `ctl.agent.nowrite` reads `managedFields`, because "which process wrote
//! this" has no other observable — and it is the one assertion that catches a mixed build
//! silently re-acquiring two writers.
```

Then, in journey order:

1. **`ctl.leader`** — `coordination.k8s.io` `Lease` `kloudlite-controller` in `kube-system`:
   `holderIdentity` is non-empty, `renewTime` is within `leaseDurationSeconds` of now, and that
   holder names a pod that is `Running` (a lease held by a pod that is gone is a stale lease, not
   a leader). Also asserts the cluster has exactly ONE controller pod `Running` — two would mean a
   replica count somebody raised without moving the counts.
2. **`ctl.grant.set`** — `PUT /v1/me/environments/{team}` with the run's environment, then from the
   probe WORKSPACE pod (not the env pod): `(getent hosts redis || nslookup redis) && redis-cli -h
   redis -p 6379 ping` must answer `PONG` within the ceiling. Poll every 2 s. `environment.rs`'s
   `resolves` is the shape; the exec target is `c.state.workspace`'s pod in `ws_namespace`.
3. **`ctl.grant.switch`** — a SECOND environment is created for this (one service, same image), the
   choice is switched to it, and BOTH halves are judged: the new environment's `redis` answers
   `PONG`, and the OLD environment's ClusterIP stops answering. The old side is judged by dialling
   its ClusterIP directly (`redis-cli -h <old ip> -p 6379 ping` with a short `-t`) rather than by
   name, because the name stops resolving when `resolv.conf` is re-rendered and a DNS failure would
   pass this id without the policy ever having been removed.
4. **`ctl.grant.cleared`** — `DELETE /v1/me/environments/{team}`, then the same dial must be
   REFUSED within the ceiling, and `kubectl`-equivalent list of `NetworkPolicy` across the two
   environment namespaces must show no `space-{ws_namespace}` — the object check here is the
   SECOND assertion, never the only one.
5. **`ctl.failover`** (hourly) — record the current holder, delete that pod, assert a DIFFERENT
   holder within 20 s, and — this is the part that makes it a journey and not a liveness check —
   issue a `PUT /v1/me/environments/{team}` DURING the gap (immediately after the delete) and
   assert the connect from the workspace succeeds once the new leader is up. Converging exactly
   once is what the epoch guard buys; a wish lost in a handover is the failure this catches.
6. **`ctl.agent.nowrite`** (hourly) — list every `NetworkPolicy` named `space-*` in the cluster and
   assert each one's `metadata.managedFields` carries `kloudlite-controller` and carries no entry
   for `kloudlite-agent`. Skip with "no space grants in the cluster" if the list is empty — an empty
   list is not evidence.
7. **`ctl.fanout`** (hourly) — change one space's choice and assert the controller reconciled at
   most two environments for it. Read it from the controller's own log lines
   (`grant.rendered`) over the window, via the admin history if `KLOUDLITE_CLICKHOUSE_URL` is
   configured for the probe. **If neither source is reachable from the probe, skip with the named
   reason `"fan-out is not measurable from the probe: no history reader configured"`** — a skip
   with a reason is honest; a pass without a measurement is the thing the 2026-09-09 review
   caught (skipped ids read as passed).

Each step uses `c.step(id, CEILING, …)`; each unreachable precondition uses `c.skip(id, why)` with
a specific why. Never let a later id pass on a failed earlier one — `environment.rs:321-325` is the
pattern: skip the dependants naming the step that did not happen.

- [ ] **Step 3: Wire the stage in**

`bins/slo/src/stages/mod.rs`: `pub mod controller;` and a call in the fast journey after the
environment stage (it needs the environment and the workspace that stage stood up), with the
hourly ids behind the suite check the neighbouring stages already use.

Teardown: the second environment created by `ctl.grant.switch` must carry the `run-{id}` name
prefix, so the existing prefix teardown collects it; do not add a teardown path.

- [ ] **Step 4: Tests and lint**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo test -p kloudlite-workspaces slo
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo clippy -p kloudlite-slo --all-targets -- -D warnings
```

- [ ] **Step 5: Commit**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && git log -3 --oneline
cd /Volumes/kdisk/rustic-git-wt/desktop-login && git add bins/slo crates/workspaces/src/slo/catalogue.rs deploy/slo.md && git commit -m "Probe the cluster controller's lease and its space grants" && git push platform HEAD
```

---

### Task 10: History — leader changes

**Files:**
- Modify: `crates/workspaces/src/history/watch.rs`

**Interfaces:** produces `controller.leader` event rows, keyed `{uid}:{resourceVersion}:leader`.

- [ ] **Step 1: Write the failing test**

Beside the other mapper tests in `history/watch.rs`:

```rust
    /// Only a change of HOLDER is an event. A Lease is rewritten every 5 s by the renew beat, and
    /// a row per renew would be 17 k rows a day per cluster saying nothing happened.
    #[test]
    fn only_a_change_of_holder_is_a_leader_event() {
        let a = lease("ctl-a", 3, "100");
        let renewed = lease("ctl-a", 3, "101");
        let b = lease("ctl-b", 4, "102");
        assert!(leader_events(Some(&a), &renewed).is_empty());
        let rows = leader_events(Some(&a), &b);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "controller.leader");
        assert_eq!(rows[0].name, "ctl-b");
        // The first sighting after a restart is an event: the holder went from unknown to known.
        assert_eq!(leader_events(None, &a).len(), 1);
    }
```

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo test -p kloudlite-workspaces only_a_change_of_holder 2>&1 | tail -20
```

- [ ] **Step 2: Implement**

A mapper beside `snapshot_events`/`volume_events`, and one reflector per cluster over
`coordination.k8s.io/v1` `Lease` in `kube-system` with `metadata.name=kloudlite-controller` (a
field selector, so it streams one object per cluster, not every Lease in it). `ts` comes
from `spec.renewTime` — derived from the object, so a replayed watch is byte-identical, the rule
every other mapper here follows. Reuse the existing row builder; add no table and no migration.

If the cluster's history client cannot list `Lease` (RBAC on an older cluster), the reflector logs
once and the rest of `history::watch` is unaffected — the same fallback shape the other kinds have.

- [ ] **Step 3: Tests**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo test -p kloudlite-workspaces history
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo clippy -p kloudlite-workspaces --all-targets -- -D warnings
```

Add `leases: get,list,watch` in `kube-system` to the admin ServiceAccount's per-region role
(`deploy/k3s/api-rbac.yaml`) — the reflector is a call, and a call needs a row.

- [ ] **Step 4: Commit**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && git log -3 --oneline
cd /Volumes/kdisk/rustic-git-wt/desktop-login && git add crates/workspaces/src/history deploy/k3s/api-rbac.yaml && git commit -m "Record every controller leader change in history" && git push platform HEAD
```

---

### Task 11: Docs

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Add the "Cluster controller" paragraph**

After "Workspaces and environments" and before "Live settings", matching the file's density — one
paragraph, the load-bearing facts only:

```md
## Cluster controller

`bins/controller` (`kloudlite-controller`) is a second control-plane process in every **k3s**
cluster — one Deployment, one replica, `Recreate`, in `kube-system`, no PDB — and it is the ONE
writer of every object that is shared across nodes or derived purely from spec. AKS runs none: it
has no `Region` CRD and no agents. Leadership is a `coordination.k8s.io/v1` `Lease` named
`kloudlite-controller` in `kube-system` (`bins/controller/src/lease.rs`): `holderIdentity` is the
pod name, `leaseTransitions` is the EPOCH, `renewTime + 15 s` the expiry, renewed every 5 s, and
the resourceVersion CAS on the update is what admits exactly one winner. Not
`ownership/lease.rs` — that one needs an `ObjectStore`, and this process holds no disk, no object
store and no cloud credential by design. **Every write checks the epoch it was elected under**; a
write that finds a newer `leaseTransitions` demotes rather than finishing, and since there is no
writer fence underneath, each object's own forced SSA under field manager `kloudlite-controller`
is the backstop. Failover is ~15 s and costs nothing: every object it owns is level-triggered and
already applied, so while it is down pods keep running and grants keep granting. Stage 1 owns
exactly the space grants — `space-env` in the space's namespace and `space-{ns}` in the
environment's — and the agents' writes of them were deleted in the same release
(`agent_writes_no_space_policies` is the guard). Fan-out is bounded: one `SpaceEnvironment` event
re-renders that space's egress half and the ingress half in at most two environments, never every
environment on every node. An unlisted reflector is UNKNOWN and the pass decides nothing — the
same rule as the agents', and it matters more here, because this process decides for the whole
cluster at once. Rolling it is not the usual order: agents first, then the controller, then the
narrowed `agent-rbac.yaml` (`deploy/k3s/README.md`) — RBAC applied early 403s a still-writing
agent and aborts its whole reconcile. Stages 2 (namespaces, quotas, `OwnerKeys`/`OwnerBinding`
status) and 3 (the sweeps, behind a `Mark::Boot` flag) are designed and not built; placement
stays in the agents deliberately, because the claiming node is the authority on the bytes it
holds. Spec: `docs/superpowers/specs/2026-09-14-cluster-controller-design.md`.
```

- [ ] **Step 2: Edit the agent's paragraph**

In "Workspaces and environments", where the attach mechanism is described ("a `/etc/resolv.conf`
the agent renders per workspace … Two NetworkPolicies named `attach-{ws}` open the path"), say that
the agent renders the file and writes the `Attached` condition and that **the controller writes
both policy halves**; and where the agent is described as "it talks to the k3s API and to OTHER
AGENTS' peer listeners … and to nothing else", leave it — that is still true.

- [ ] **Step 3: Commit**

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && git log -3 --oneline
cd /Volumes/kdisk/rustic-git-wt/desktop-login && git add CLAUDE.md && git commit -m "Document the cluster controller beside the node agents" && git push platform HEAD
```

---

## Done means

```sh
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo clippy --workspace --all-targets --locked -- -D warnings
cd /Volumes/kdisk/rustic-git-wt/desktop-login && cargo test --locked
```

both green, and on the cluster after the three-step apply: `kubectl -n kube-system get lease
kloudlite-controller` names a Running controller pod, every `space-*` NetworkPolicy's
`managedFields` names `kloudlite-controller` and no `kloudlite-agent`, and one fast SLO run reports
`ctl.leader`, `ctl.grant.set`, `ctl.grant.switch` and `ctl.grant.cleared` good. Until that run
passes on the carrying build, this is "changed, unverified".
