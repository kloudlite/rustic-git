# Volume Replication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every Workspace/Environment volume keeps btrfs replicas on N-1 standby nodes, refreshed on
a background beat over a new agent peer listener, recorded in the parent's
`status.compatibleNodes` so `claim::may_claim` can already place against them.

**Architecture:** Pure selection/ordering logic in `crates/workspaces/src/replicate.rs`; an
authenticated axum listener on the agent that pipes request bodies into `btrfs receive
{pool}/repl/{id}/`; a sender beat beside the home-push beat that snapshots, picks `-p`/`-c`
parents, and streams `btrfs send` to each target; a keep-biased janitor sweep for `repl/`.

**Tech Stack:** Rust (axum, reqwest, tokio), btrfs send/receive, kube-rs.

**Spec:** `docs/superpowers/specs/2026-08-31-volume-replication-design.md` — read it first. Its
"What already supports this" section is why no claim-path change appears anywhere below, and its
amended transport section is why NOTHING here writes into `{pool}/recv/`.

## Global Constraints

- **`WS_REPLICA_COUNT` defaults to 1 — replication OFF.** With the default, no listener request is
  ever made, no snapshot taken, no status written. Every diff must be inert at N=1.
- **Replica snapshots live under `{pool}/repl/{id}/`, never `{pool}/recv/`.** `janitor_sweep_recv`
  deletes recv/ entries not named in a lineage; a replica is in no lineage.
- **`compatibleNodes` is written by the RECEIVER, only after a clean receive, on the PARENT
  (Workspace/Environment), via the widening idiom already in `bins/agent/src/claim.rs`
  (`with_me` + re-read-on-conflict).** A partial receive must leave no subvolume and no status write.
- **Only the owning node sends** (`status.nodeName == ctx.node`). Home volumes are excluded from v1.
- **The peer secret is its own key (`WS_PEER_SECRET`), never the registry token**, compared in
  constant time. An unset secret disables the listener AND the sender — fail closed.
- **Every `btrfs` invocation is keep-biased on failure**: a failed `-c` send falls back to a full
  send; a failed receive deletes its partial; an unreadable directory sweeps nothing.
- The sender beat never blocks a reconcile: it runs where the home-push beat runs, on its own tick.
- Comments explain WHY at the density of `bins/server/src/router/route.rs`. Commit subjects
  imperative sentence case, no tool attribution.
- `CARGO_INCREMENTAL=0` on every cargo run, FOREGROUND, long timeout.
- `CARGO_INCREMENTAL=0 cargo test --workspace --locked` and
  `CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets --locked -- -D warnings` green.
  `tests/routing.rs` has a known flake under parallel load — re-run it alone.

---

### Task 1: Selection and ordering (pure logic)

**Files:**
- Create: `crates/workspaces/src/replicate.rs`; register `pub mod replicate;` in `lib.rs`
- Modify: `crates/workspaces/src/engine/pool.rs` — add `repl` beside `recv`
- Test: `mod tests` inside `replicate.rs`

**Interfaces:**
- Produces: `replicate::targets(volume_id: &str, me: &str, candidates: &[String], total: usize) -> Vec<String>`
  — the `min(total-1, candidates minus me)` standby nodes, rendezvous-ordered, never containing `me`.
- Produces: `replicate::order_groups(vols: &[(String, Option<String>)]) -> Vec<String>` — input is
  `(volume_id, clone_of)`; output is every id ordered so a `cloneOf` source precedes its clones
  (sources whose id is absent from the input sort as roots of their own).
- Produces: `Pool::repl(&self, name: &str) -> PathBuf` = `{pool}/repl/{name}`.

- [ ] **Step 1: Write the failing tests**

```rust
    /// Rendezvous, not modulo: every agent computes the same set with no coordinator, and adding
    /// a node moves ~1/N of volumes instead of nearly all — each move is a full btrfs send.
    #[test]
    fn selection_is_deterministic_excludes_me_and_caps_at_total() {
        let nodes: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        let t = targets("ws-1", "a", &nodes, 3);
        assert_eq!(t, targets("ws-1", "a", &nodes, 3), "same inputs, same answer, any process");
        assert_eq!(t.len(), 2);
        assert!(!t.contains(&"a".to_string()));
        assert!(targets("ws-1", "a", &nodes, 1).is_empty(), "N=1 is replication off");
        assert_eq!(targets("ws-1", "a", &nodes[..2], 5).len(), 1, "capped by the cluster");
    }

    #[test]
    fn adding_a_node_moves_few_volumes() {
        let four: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        let five: Vec<String> = ["a", "b", "c", "d", "e"].iter().map(|s| s.to_string()).collect();
        let moved = (0..1000)
            .filter(|i| {
                let id = format!("ws-{i}");
                targets(&id, "a", &four, 2) != targets(&id, "a", &five, 2)
            })
            .count();
        assert!(moved < 400, "rendezvous keeps most assignments: {moved}/1000 moved");
    }

    /// Ancestor-first is the entire sharing mechanism: a clone sent before its source arrives as
    /// a full copy, and nothing ever repairs that.
    #[test]
    fn groups_order_sources_before_clones() {
        let vols = vec![
            ("ws-clone2".into(), Some("ws-clone1".into())),
            ("ws-root".into(), None),
            ("ws-clone1".into(), Some("ws-root".into())),
            ("ws-lonely".into(), Some("ws-gone".into())), // source not on this node
        ];
        let out = order_groups(&vols);
        let pos = |id: &str| out.iter().position(|v| v == id).unwrap();
        assert!(pos("ws-root") < pos("ws-clone1") && pos("ws-clone1") < pos("ws-clone2"));
        assert_eq!(out.len(), 4, "a clone with an absent source is still sent");
    }
```

- [ ] **Step 2: Run and watch them fail** — `CARGO_INCREMENTAL=0 cargo test -p rustic-git-workspaces replicate` (module doesn't exist).

- [ ] **Step 3: Implement**

```rust
use sha2::{Digest, Sha256};

/// Highest-random-weight: score every candidate by `sha256(volume_id, node)` and take the top
/// `total-1` that are not this node. Deterministic across processes on purpose — sha2, never
/// `DefaultHasher`, whose output the std docs do not promise stable across releases.
pub fn targets(volume_id: &str, me: &str, candidates: &[String], total: usize) -> Vec<String> {
    let n = total.saturating_sub(1);
    let mut scored: Vec<(Vec<u8>, &String)> = candidates
        .iter()
        .filter(|c| c.as_str() != me)
        .map(|c| {
            let mut h = Sha256::new();
            h.update(volume_id.as_bytes());
            h.update([0]); // separator: ("ab","c") must not collide with ("a","bc")
            h.update(c.as_bytes());
            (h.finalize().to_vec(), c)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().take(n).map(|(_, c)| c.clone()).collect()
}

/// Topological order over `cloneOf`, sources first. Kahn-style with a stable tie-break so the
/// beat visits volumes in the same order every time. A missing source is a root: its clone still
/// replicates, just without a `-c` to share against.
pub fn order_groups(vols: &[(String, Option<String>)]) -> Vec<String> {
    let ids: std::collections::HashSet<&str> = vols.iter().map(|(id, _)| id.as_str()).collect();
    let mut out = Vec::with_capacity(vols.len());
    let mut placed = std::collections::HashSet::new();
    let mut rest: Vec<&(String, Option<String>)> = vols.iter().collect();
    rest.sort_by(|a, b| a.0.cmp(&b.0));
    while !rest.is_empty() {
        let before = out.len();
        rest.retain(|(id, src)| {
            let ready = match src.as_deref() {
                Some(s) if ids.contains(s) => placed.contains(s),
                _ => true,
            };
            if ready {
                out.push(id.clone());
                placed.insert(id.clone());
            }
            !ready
        });
        if out.len() == before {
            // A cloneOf cycle cannot be built through /v1, but a hand-edited spec must not hang
            // the beat: emit the remainder in name order and let the sends be full.
            out.extend(rest.drain(..).map(|(id, _)| id.clone()));
        }
    }
    out
}
```

`Pool::repl` in `pool.rs`, beside `recv`:

```rust
    /// Replica snapshots, sender side and receiver side both. NOT `recv/`: the janitor keeps
    /// recv/ entries only when a lineage names them, and a replica is in no lineage.
    pub fn repl(&self, name: &str) -> PathBuf {
        self.root.join("repl").join(name)
    }
```

- [ ] **Step 4: Tests pass** — `CARGO_INCREMENTAL=0 cargo test -p rustic-git-workspaces replicate`
- [ ] **Step 5: Commit** — `git add crates/workspaces && git commit -m "Add replica target selection and clone-group ordering"`

---

### Task 2: The peer listener (receive side)

**Files:**
- Create: `bins/agent/src/peer.rs`; wire from `bins/agent/src/lib.rs`
- Modify: `bins/agent/Cargo.toml` — add `axum = { workspace = true }` (already a workspace dep)
- Test: `bins/agent/tests/peer.rs` (new; the axum router is testable with `tower::ServiceExt::oneshot` without btrfs by injecting the receive command)

**Interfaces:**
- Consumes: `Pool::repl`, the kube client from `Ctx`, `claim.rs`'s widening idiom.
- Produces: `peer::router(state) -> axum::Router` and `peer::serve(ctx)` bound to
  `WS_PEER_ADDR` (default `0.0.0.0:8444`), spawned from `lib.rs` ONLY when `WS_PEER_SECRET` is set.
- Routes:
  - `GET /peer/v1/snapshots/{owner}/{id}` → JSON array of subvolume names in `{pool}/repl/{id}/`
    (empty array when the directory is absent).
  - `POST /peer/v1/replicate/{owner}/{id}` → body is one btrfs send stream; piped into
    `btrfs receive {pool}/repl/{id}/`; 200 with the received snapshot name on success.
- Every route requires header `x-peer-secret` equal to `WS_PEER_SECRET` (constant-time compare —
  `subtle::ConstantTimeEq` if already in the tree, else compare `Sha256` digests of both, which is
  constant-time in the value); anything else is 401 before any filesystem or kube call.

- [ ] **Step 1: Failing tests** — router-level, fake receive command:

```rust
/// A wrong or missing secret must be refused before the request body is read: the body is a
/// root-run `btrfs receive`, and auth is the only thing between the network and it.
#[tokio::test]
async fn every_peer_route_requires_the_secret() { /* oneshot GET+POST with no header, wrong header → 401; right header → not 401 */ }

/// A receive that dies mid-stream must leave nothing: a partial subvolume advertised in
/// compatibleNodes is a node that claims a workspace it cannot start.
#[tokio::test]
async fn a_failed_receive_deletes_its_partial_and_writes_no_status() { /* fake receive exits 1 after creating the dir; assert dir gone, no PATCH recorded */ }

#[tokio::test]
async fn a_clean_receive_widens_the_parents_compatible_nodes() { /* fake receive exits 0; assert one status PATCH on the Workspace carrying both nodes */ }
```

Follow `bins/agent/tests/reconcile.rs`'s existing fake-API idiom for recording PATCHes; do not
invent a second fixture style.

- [ ] **Step 2: Run and watch them fail.**

- [ ] **Step 3: Implement the router.** The receive handler:

1. Auth (above).
2. `Digest`-style validation of `{owner}` and `{id}` path segments via
   `rustic_git_storage::store::valid_segment` — a path segment becomes a filesystem path here,
   same rule as everywhere else in this codebase.
3. `create_dir_all(pool.repl(id))`, snapshot names present BEFORE, spawn
   `btrfs receive <repl/{id}>` with stdin piped, stream the body in with
   `tokio::io::copy` (spawn via `tokio::process::Command`).
4. On non-zero exit: diff the names AFTER against BEFORE, `btrfs subvolume delete` anything new,
   return 500. On success: the one new name is the response body.
5. Widen the parent: try `Api::<crd::Workspace>` get by `{id}`, else `Api::<crd::Environment>`;
   patch status `compatibleNodes` with `claim::with_me(&current, &ctx.node)`, re-reading and
   retrying once on conflict — the same shape `claim::decide`'s 409 comment documents. A parent
   that exists on neither API is a replica for a deleted volume: still 200 (the janitor sweeps
   it), no status write.

Bound the body: no explicit limit — a send stream is legitimately tens of GiB; the quota that
matters is pool space, and `btrfs receive` failing on ENOSPC is the enforcement (spec's failure
table). Say this in a comment rather than leaving the absence ambiguous.

- [ ] **Step 4: Tests pass**, then full suite + clippy.
- [ ] **Step 5: Commit** — `git commit -m "Add the agent peer listener for replica receive"`

---

### Task 3: The sender beat, retention, and the repl sweep

**Files:**
- Modify: `bins/agent/src/peer.rs` — sender half lives beside the receiver so the two ends of the
  protocol sit in one file
- Modify: `bins/agent/src/controller.rs` — spawn the beat where the home-push beat is spawned
  (`WS_HOME_PUSH_SECS` is at `controller.rs:574`; mirror its env/default/spawn shape with
  `WS_REPLICA_SECS`, default 300, and `WS_REPLICA_COUNT`, default 1)
- Modify: `bins/agent/src/janitor.rs` — `janitor_sweep_repl`
- Test: `bins/agent/tests/reconcile.rs` janitor tests' file for the sweep; unit tests in `peer.rs`
  for parent/`-c` argument construction

**Interfaces:**
- Consumes: `replicate::targets`, `replicate::order_groups`, `Pool::repl`,
  `Engine::generation_of` (make it `pub(crate)`-visible or add a public wrapper on `Engine` —
  whichever the tree already leans toward), `blob::spawn_send(path, parent)` — EXTEND it with a
  `clones: &[PathBuf]` parameter emitting `-c` per entry (its two existing callers pass `&[]`).
- Produces: `peer::replicate_beat(ctx)` — one pass; the spawned loop calls it on the tick.

- [ ] **Step 1: Failing tests**

```rust
/// The send argument set IS the sharing model: -p resumes this volume's own chain, -c lets a
/// clone reference its ancestor's extents on the receiver. Wrong arguments silently ship full
/// copies forever, so the construction is pinned here.
#[test]
fn send_args_use_p_for_own_parent_and_c_for_ancestor() { /* assert on the argv spawn_send builds */ }

/// An unchanged volume must cost nothing: the beat is every 300s forever, on every volume.
#[test]
fn an_unmoved_generation_sends_nothing() { /* .replicated-gen-{node} == current → no snapshot, no dial */ }
```

Janitor test, in the existing sweeps' shape: an orphaned `repl/{id}` (no `vol/{id}` entry) older
than the floor is swept; a live one and a young one are kept; unreadable `vol/` sweeps nothing.

- [ ] **Step 2: Run and watch them fail.**

- [ ] **Step 3: Implement one beat pass**

1. `WS_REPLICA_COUNT <= 1` or `WS_PEER_SECRET` unset → return immediately.
2. Candidates: `Api::<Node>::all` list by label `rustic-git.io/pool=true`, names sorted. (RBAC
   gains `list` on nodes — Task 4.)
3. This node's volumes: the Volumes already cached by the controller's reflector if one exists,
   else one list filtered to `spec.nodeName == ctx.node`; drop homes
   (`crd::home_volume_name(owner)` names — match by the existing helper, never by prefix
   formatting) and volumes whose parent is Stopped mid-teardown.
4. `order_groups` over `(id, clone_of)`; for each volume, `targets(id, me, candidates, N)`.
5. Per (volume, target) with `generation(live) > .replicated-gen-{target}`:
   - RO snapshot `{pool}/repl/{id}/g{generation}` if not already present (one snapshot serves
     every target of this beat).
   - `GET /peer/v1/snapshots/{owner}/{id}` on the target (address: list agent pods by label
     `app=rustic-git-agent` + field `spec.nodeName={target}` in kube-system, dial pod IP:8444 —
     the ClusterRole already holds pods get/list/watch).
   - Parent = newest `g*` name present on BOTH sides; clones add `-c {pool}/repl/{src}/g*` for the
     newest ancestor snapshot the receiver also lists.
   - `spawn_send(snap, parent, clones)`, stream stdout as the POST body
     (`reqwest::Body::wrap_stream` over `tokio_util::io::ReaderStream`). Non-2xx or a `-c` refusal
     from btrfs → retry once as a FULL send (no `-p`, no `-c`) before giving up until next beat.
   - On 200: write `.replicated-gen-{target}`, then delete local `g*` snapshots older than the
     oldest generation any target still needs (retention: the spec's "sender-side snapshot
     retention" section is the rule).
6. Every failure: `tracing::warn!` and continue to the next volume — the beat never aborts.

- [ ] **Step 4: The repl sweep** — `janitor_sweep_repl(pool, min_age)` mirroring
  `janitor_sweep_attach`: keep-set is one read of `vol/`, entries in `repl/` whose id has no
  voldir and whose subvolumes are older than the floor are `btrfs subvolume delete`d then the dir
  removed. Wire it into the beat where the other sweeps run.

- [ ] **Step 5: Full suite + clippy.**
- [ ] **Step 6: Commit** — `git commit -m "Replicate owned volumes to standby nodes on a beat"`

---

### Task 4: Wiring, RBAC, deploy

**Files:**
- Modify: `deploy/k3s/agent-daemonset.yaml` — env `WS_REPLICA_COUNT` (value "1" — OFF until the
  operator raises it), `WS_REPLICA_SECS`, `WS_PEER_ADDR`, and `WS_PEER_SECRET` from the existing
  optional `rustic-git-agent` secretRef; containerPort 8444 named `peer`
- Modify: `deploy/k3s/agent-rbac.yaml` — nodes `get` → `get,list` (header table row AND rule
  together; the table IS the role)
- Create: peer NetworkPolicy + headless discovery note in `deploy/k3s/agent-peer.yaml` — a
  NetworkPolicy in kube-system admitting 8444 only from pods labelled `app=rustic-git-agent`
  (discovery is by pod IP from the API, so no Service object is needed at all — say so in the
  file header instead of shipping an unused Service)
- Modify: `deploy/k3s/README.md` — table row, apply line, and a "Replication" section: how to set
  the secret key, raise `WS_REPLICA_COUNT`, and that the rollout order is unconstrained (the
  listener is fail-closed without its secret, and N defaults to 1)

- [ ] **Step 1: Make the changes.** RBAC comment for the new verb:

```yaml
  # `list` joined `get` for replication: the sender computes standby targets by rendezvous over
  # the pooled nodes, and every agent must see the same candidate list or two nodes will disagree
  # about who holds what.
```

- [ ] **Step 2: Verify** — `python3 -c "import yaml,sys; [list(yaml.safe_load_all(open(f))) for f in ['deploy/k3s/agent-daemonset.yaml','deploy/k3s/agent-rbac.yaml','deploy/k3s/agent-peer.yaml']]"`,
  and grep that the header table and rules changed together.
- [ ] **Step 3: Commit** — `git commit -m "Wire replication env, peer policy, and node list RBAC"`

---

## Self-review

**Spec coverage.** Rendezvous + growth (`targets`, Task 1), group ordering with `-c` (Tasks 1, 3),
peer transport + auth + partial cleanup (Task 2), receiver-writes-parent-status (Task 2), beat +
generation gate + retention (Task 3), repl/ not recv/ with its own sweep (Tasks 2–3), N default 1
(Tasks 3–4), homes excluded (Task 3 step 3.3).

**Deviations from the spec, deliberate.** (a) No headless Service: discovery is by pod IP through
the API the agent already watches pods on, so the Service would be an unused object — the spec's
"headless Service gives per-pod DNS" is replaced by the pod list, and the spec should be amended if
this survives review. (b) `subtle` vs digest-compare for the secret: whichever the tree already
has; the requirement is constant-time, not a particular crate.

**Not covered on purpose.** The spec's measured check (N=2 on the real cluster, both nodes in
`compatibleNodes`, replica contents matching) is a post-deploy verification. Cross-region, space
shedding, per-owner N, and home volumes are the spec's own blanks.

**Type consistency.** `targets(&str, &str, &[String], usize) -> Vec<String>` (Tasks 1→3);
`order_groups(&[(String, Option<String>)]) -> Vec<String>` (1→3); `spawn_send(path, parent, clones)`
(3, with both existing callers updated in the same commit); `Pool::repl(&str) -> PathBuf` (1→2→3).

**Known soft spots.** (a) `-c` relatedness is btrfs-version-sensitive and cannot be unit-tested on
this Mac — the full-send fallback in Task 3 step 5 is the safety, and the e2e measured check is
where `-c` is actually proven. (b) The receiver's parent lookup (Workspace then Environment by id)
assumes parent name == volume id, which is the codebase's convention ("Both kinds own a Volume of
the parent's own name" — claim.rs:37); if that ever breaks, the widen silently no-ops, which is
availability lost, not correctness.
