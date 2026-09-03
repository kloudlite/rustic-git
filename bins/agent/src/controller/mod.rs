//! The node-scoped controller: three reconcilers over the `rustic-git.io/v1alpha1` CRDs bound to
//! THIS node, converging the local btrfs pool and the node's pods toward what the specs ask for.
//!
//! `spec.nodeName` is the whole sharding story: two nodes cannot contend for one object because the
//! object names its node. There is no acquisition, no expiry, no requeue sweep — the queue, the
//! lease and the heartbeat this replaces were all re-implementations of what a watch already is.
//!
//! Long btrfs work runs on `spawn_blocking` (its own OS thread, its own tiny current-thread
//! runtime), not `tokio::spawn` on the shared reactor: `Engine::push`/`squash` block on `ws_lock`'s
//! synchronous `libc::flock`, and a `LocalSet`/single-reactor-thread design would let one
//! workspace's lock wait freeze every other in-flight operation. `spawn_blocking` also sidesteps
//! `WsClone`'s `&dyn Fn` stop/start hooks (no `+Send` bound in `engine::ops.rs`, out of scope to
//! change here) — they never have to cross an actual cross-thread `.await` boundary.

use kube::runtime::watcher;
use rustic_git_workspaces::crd::{self, Phase, VolumeSource};
use rustic_git_workspaces::engine::Engine;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub(crate) mod environment;
pub use environment::apply_environment;
pub(crate) use environment::{stopped_condition, write_env_status};
pub(crate) mod workspace;
pub use workspace::{apply_workspace, cleanup_workspace_worktree, reconcile_environment, reconcile_workspace};
// pub so the inode invariant is assertable from the integration suite — see reconcile.rs.
pub use workspace::write_resolv_conf;
pub(crate) use workspace::{kept_conditions, migrate_and_seed_baseline, replaced, write_ws_status};
pub(crate) mod volume;
pub use volume::{apply_volume, cleanup_volume};
pub(crate) use volume::{heal_labels, owner_ref_of_kind, reconcile_volume, resolve_volume, Resolved};
pub mod run;
pub use run::{run, running_contains, wake_on_finish};
pub(crate) mod stop;
pub use stop::{replicated_condition, start_placement};
pub(crate) mod status;
pub(crate) use status::{conditions_eq, create_if_absent, delete_ignoring_404, ensure, forget_applied, patch_status, replace_status, settle, write_status, Outcome};

/// While an operation runs, and while a stop is waiting for its push to land. Short on purpose:
/// these are progress checks, not retries.
/// The commit a `cloneOf` is pinned to, if any — the graft the API recorded at clone/restore time.
/// Shared because a Workspace and an Environment carry the identical `WorkspaceStorage`, and both
/// reconcilers have to resolve it into `status.head` before they check anything out (an
/// environment that never did parked forever in `HeadUnknown`).
pub(crate) fn clone_commit(storage: &Option<rustic_git_workspaces::crd::WorkspaceStorage>) -> Option<&str> {
    match storage.as_ref()?.source.as_ref()? {
        rustic_git_workspaces::crd::VolumeSource::CloneOf { commit: Some(c), .. } => Some(c.as_str()),
        _ => None,
    }
}

pub(crate) const TICK: Duration = Duration::from_secs(15);
/// After a failure. The reconcile that observes it does not stamp `observedGeneration`, so the next
/// pass starts the work again — backoff, never give up.
const RETRY: Duration = Duration::from_secs(60);

/// What the CLUSTER says about THIS node — one GET on our own Node, per reconcile.
///
/// The pull beat already refuses to sweep in that state; this is the other half. A partitioned
/// agent (kubelet down, API server still reachable) goes on reconciling its own objects every
/// `TICK`, and every one of those passes rewrites status: the stopped arms clear the sweep's
/// `Degraded=True/NodeDead` and `apply_volume`'s `Unavailable` re-run rewrites the volume's
/// conditions. The mark another node just wrote was therefore absent almost all of the time, and
/// `/v1` accepted `start` on a node the cluster reads as dead. So the guard goes ABOVE
/// `cleared_node_dead`, not beside it: while we are dead we write nothing at all.
///
/// `node_is_dead`, never `unplaceable`: a DECOMMISSIONING node is alive and must keep converging
/// or its drain never finishes. Keep-biased on a failed read — a Node we cannot fetch is not
/// evidence of death, and pausing on a hiccup would stall every workspace on the node.
///
/// The `WS_NODE_DEAD_SECS` floor (180 s) is the only guard here: a node wrongly NotReady past it
/// stops reconciling until its Node object recovers. Deliberate — a wrong write corrupts placement,
/// a paused reconcile only waits.
/// Both facts a reconcile needs about its own node, off the SAME single GET: `dead` gates every
/// write below, `decommissioning` is the drain notice the running arms stamp (F7).
#[derive(Clone, Copy, Default)]
pub(crate) struct MyNode {
    pub dead: bool,
    pub decommissioning: bool,
}

pub(crate) async fn my_node(ctx: &Ctx) -> MyNode {
    match kube::Api::<k8s_openapi::api::core::v1::Node>::all(ctx.client.clone()).get_opt(&ctx.node).await {
        // Absent is not dead either: a wrong `$NODE_NAME` would otherwise freeze every reconcile
        // on a live node with nothing but a silent requeue. The sweep sees the whole listing and is
        // the authority on a node that is not there.
        Ok(None) => {
            tracing::warn!(node = %ctx.node, "reconcile: my own Node object is missing; assuming alive");
            MyNode::default()
        }
        Ok(Some(n)) => MyNode {
            dead: crate::peer::node_is_dead(Some(&n), crate::peer::node_dead_secs(), k8s_openapi::jiff::Timestamp::now()),
            decommissioning: crate::peer::decommissioning(Some(&n)),
        },
        Err(e) => {
            tracing::warn!(node = %ctx.node, error = %e, "reconcile: reading my own Node; assuming alive");
            MyNode::default()
        }
    }
}

/// The drain notice a RUNNING parent carries while its node is being retired — the one place a
/// person is told that stopping is theirs to time and that the next start lands elsewhere.
///
/// Written by the parent's OWN reconcile, not by the decommission beat: the running arms rewrite
/// the condition list wholesale every `TICK`, so the beat's mark was erased seconds after it landed
/// and the workspace never carried it at all. A reconcile cannot race itself.
pub(crate) const DRAIN_MESSAGE: &str = "this node is being retired; stop when convenient and the next start lands elsewhere";

pub(crate) fn with_drain_notice(prev: &[crd::Condition], mut conditions: Vec<crd::Condition>, decommissioning: bool, gen: i64) -> Vec<crd::Condition> {
    if decommissioning {
        let was = prev.iter().find(|c| c.type_ == "Decommissioning");
        conditions.push(crd::condition_since(was, "Decommissioning", true, "NodeLeaving", DRAIN_MESSAGE, gen));
    }
    conditions
}

/// Keyed by uid, carrying the generation it was started for — see `Ctx::running`.
///
/// The generation is in the VALUE, not the key. Keyed by `{uid, generation}` a spec edit during a
/// long push produced a new key, so the running handle was never looked up again: it was never
/// drained, never removed, and its btrfs work ran on unobserved. One entry per volume cannot leak,
/// and "is anything running for this volume" becomes answerable — which is what the delete path
/// needs.
pub type InFlight = HashMap<String, (i64, tokio::task::JoinHandle<Result<Done, String>>)>;

/// The API tier's identity, which the per-namespace Secret grant names. Hard-coded: the API is
/// deployed by the manifests in `deploy/`, which name exactly these.
pub(crate) const API_SERVICE_ACCOUNT: &str = "rustic-git-api";
pub(crate) const API_NAMESPACE: &str = "kube-system";

pub struct Ctx {
    pub client: kube::Client,
    pub engine: Arc<Engine>,
    pub node: String,
    pub pool: String,
    /// `WS_RUNTIME_CLASS`, e.g. `gvisor`. Empty means tenant pods run on the host kernel.
    ///
    /// Per-cluster, because a `runtimeClassName` naming a runtime the nodes have not got makes
    /// every tenant pod fail to start. Enabling it belongs where the runtime is installed.
    pub runtime_class: Option<String>,
    /// `WS_DEFAULT_IMAGE`: the tagged platform image behind `model::DEFAULT_WS_IMAGE`.
    pub default_image: String,
    /// In-flight long btrfs operations, keyed by the uid of the object that asked for them (a
    /// `Volume` being materialized, or a `Snapshot` being cut). THE idempotency guard,
    /// and a local in-memory check rather than a distributed lease because exactly one agent ever
    /// reconciles a given object: for a `Volume` that is the `spec.nodeName` field selector on the
    /// watch, and for a `Snapshot` — which names no node — it is `snapshot::my_volume`,
    /// which acts only when the named Volume's `spec.nodeName` is this one.
    pub running: Mutex<InFlight>,
    /// A finished operation wakes its own reconciler instead of waiting out the `TICK` requeue: a
    /// local clone's btrfs work takes under a second, and without this the object sat `progressing`
    /// for the rest of the 15s tick because nothing but the clock ever looked at the handle again.
    /// The requeue stays as the backstop — a dropped send costs a tick, never the object.
    pub wake_volume: tokio::sync::mpsc::UnboundedSender<kube::runtime::reflector::ObjectRef<crd::Volume>>,
    /// The same, for a finished Nix profile build — without it a workspace waits out the tick with
    /// its pod ungated on a profile that is already on disk.
    pub wake_workspace: tokio::sync::mpsc::UnboundedSender<kube::runtime::reflector::ObjectRef<crd::Workspace>>,
    /// The receiving halves, until `run` takes them and feeds each `Controller::reconcile_on`.
    #[allow(clippy::type_complexity)]
    pub wakes: Mutex<
        Option<(
            tokio::sync::mpsc::UnboundedReceiver<kube::runtime::reflector::ObjectRef<crd::Volume>>,
            tokio::sync::mpsc::UnboundedReceiver<kube::runtime::reflector::ObjectRef<crd::Workspace>>,
        )>,
    >,
    /// Where `gitRepo` seeding clones from and with what. `WS_GIT_BASE` and the agent-side clone
    /// are gone: the clone happens inside the pod, over SSH, as the owner.
    pub git_ssh_host: String,
    pub git_ssh_port: String,
    pub git_init_image: String,
    /// This node's region, from `WS_REGION` — the other half of an `OwnerBinding`'s identity.
    pub region: String,
    /// The roles this node carries, read ONCE from its own `Node` labels at startup
    /// (`rustic-git.io/session`, `rustic-git.io/env`). A second, hand-maintained copy of a label
    /// the scheduler already reads is a second thing that can be wrong — see `k8s::placement`.
    pub roles: Vec<String>,
    /// `WS_HOMES_EXPORT`: `None` means this node has no shared-home NFS mount, and every
    /// workspace reconcile parks on `HomeNotReady` rather than starting a pod that would hostPath
    /// an empty local dir in the home's place.
    pub homes_export: Option<String>,
    /// The one Nix client, behind a trait so the reconciler is tested with a fake instead of a
    /// real daemon and store.
    pub nix: Arc<dyn crate::nix::Nix>,
    /// The spec hash each in-flight profile build was STARTED from, keyed like `running`. Without
    /// it a spec edit during a build is lost: the finished build is published as if it were the
    /// new spec, stamped with the new hash, and never rebuilt.
    pub profile_builds: Mutex<HashMap<String, String>>,
    /// Where this node's per-workspace profile links live (`nix::PROFILES_DIR` in production). A
    /// field and not a global so a test can point it at a tempdir without racing every other test.
    pub profiles_dir: std::path::PathBuf,
    /// Makes a workspace's SSH host key. Behind a trait so tests never shell out to `ssh-keygen`.
    /// This node's Volumes, from the ONE shared watch `run` opens. Four controllers used to open
    /// their own `spec.nodeName` watch on the same objects, and the snapshot reconciler GETted the
    /// Volume for every request in the cluster — this store is both answers.
    pub volumes: kube::runtime::reflector::Store<crd::Volume>,
    /// The writing half, until `run` takes it and drives the watch into it (tests feed it directly).
    pub volume_writer: Mutex<Option<kube::runtime::reflector::store::Writer<crd::Volume>>>,
    /// What `ensure` last applied, by kind/namespace/name: the hash of the desired object and when.
    /// A converged parent reconciles on every child event and re-applied ~10 objects each time;
    /// an apply whose body has not changed is skipped. See `ensure` for the ceiling.
    pub applied: Mutex<HashMap<String, (u64, std::time::Instant)>>,
    /// Fired by `/peer/v1/wake` and awaited by `spawn_pull`. A `Notify` and not a channel because
    /// the payload is nothing at all: "something changed, pull now". `notify_one` stores exactly
    /// one permit, so a burst of stops coalesces into one extra pass instead of N.
    pub pull_wake: Arc<tokio::sync::Notify>,
    /// `WS_PEER_SECRET`, read ONCE at boot. Every peer dial takes it as a parameter from here
    /// rather than reading process env itself: a function that reads env is a function whose
    /// tests must write env, and this binary's tests all share one process.
    pub peer_secret: String,
    /// When this node last woke its peers for a SYNC point, in unix millis — the coalescing window
    /// behind `snapshot::wake_worthy`. Per node and not per worktree because the wake itself is per
    /// node: `wake_peers` tells every peer "pull now", so a second one inside the same window
    /// carries no information the first did not, and would cost another Node list.
    pub last_sync_wake: std::sync::atomic::AtomicI64,
}

impl Ctx {
    #[allow(clippy::too_many_arguments)]
    pub fn new(client: kube::Client, engine: Arc<Engine>, node: String, pool: String, region: String, roles: Vec<String>, homes_export: Option<String>, nix: Arc<dyn crate::nix::Nix>, profiles_dir: std::path::PathBuf) -> Ctx {
        // Required, not defaulted: a workspace that names no image runs THIS, and an agent that
        // silently fell back to `:latest` would move every workspace on its next restart.
        let default_image = std::env::var("WS_DEFAULT_IMAGE").ok().filter(|v| !v.is_empty())
            .unwrap_or_else(|| panic!("WS_DEFAULT_IMAGE is required: the pinned image a workspace without one runs"));
        let runtime_class = std::env::var("WS_RUNTIME_CLASS").ok().filter(|v| !v.is_empty());
        if let Some(rc) = &runtime_class {
            tracing::info!(runtime_class = %rc, "tenant pods will run sandboxed");
        }
        // Unbounded on purpose: the only senders are this agent's own finished operations, one
        // wake each, so the queue can never hold more than the operations in flight.
        let (wake_volume, vol_rx) = tokio::sync::mpsc::unbounded_channel();
        let (wake_workspace, ws_rx) = tokio::sync::mpsc::unbounded_channel();
        // 256 is the dispatcher's per-subscriber buffer, not a cap on volumes: a subscriber that
        // stops polling stalls the reflector once it fills, and every subscriber here is a
        // controller polled by the same `join!`.
        let (volumes, volume_writer) = kube::runtime::reflector::store_shared(256);
        Ctx {
            volumes,
            volume_writer: Mutex::new(Some(volume_writer)),
            applied: Mutex::new(HashMap::new()),
            pull_wake: Arc::new(tokio::sync::Notify::new()),
            // 0, not "now": a restarted agent's first sync cut should wake immediately.
            last_sync_wake: std::sync::atomic::AtomicI64::new(0),
            peer_secret: std::env::var("WS_PEER_SECRET").unwrap_or_default(),
            wake_volume,
            wake_workspace,
            wakes: Mutex::new(Some((vol_rx, ws_rx))),
            client,
            engine,
            node,
            pool,
            git_ssh_host: std::env::var("WS_GIT_SSH_HOST").unwrap_or_else(|_| "git.khost.dev".into()),
            git_ssh_port: std::env::var("WS_GIT_SSH_PORT").unwrap_or_else(|_| "22".into()),
            git_init_image: std::env::var("WS_GIT_INIT_IMAGE").unwrap_or_else(|_| "alpine/git:2.45.2".into()),
            runtime_class,
            default_image,
            running: Mutex::new(HashMap::new()),
            region,
            roles,
            homes_export,
            nix,
            profiles_dir,
            profile_builds: Mutex::new(HashMap::new()),
        }
    }
}

impl Ctx {
    /// Put a Volume in the shared store by hand. For tests, which have no watch to feed it; a
    /// no-op once `run` has taken the writer, because then the watch is the only writer.
    pub fn remember_volume(&self, v: crd::Volume) {
        if let Some(w) = self.volume_writer.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            w.apply_watcher_event(&watcher::Event::Apply(v));
        }
    }
}

/// What a finished volume operation has to say about the pool, drained into status on a later pass.
#[derive(Debug, Default)]
pub struct Done {
    pub phase: Phase,
    /// The snapshot an in-place restore materialized, echoed into `status.restoredTo` — the field
    /// both this controller and the parent read to tell "already done" from "not yet".
    pub restored_to: Option<String>,
    /// Why `spec.quotaGb` is NOT enforced on disk, when it is not — surfaced as `QuotaEnforced`
    /// rather than failing the volume: a pool without qgroups is the operator's to fix, and a
    /// usable-but-uncapped volume beats an unusable one.
    pub quota_unenforced: Option<String>,
}

#[derive(Debug)]
pub struct ReconcileErr(pub String);

impl std::fmt::Display for ReconcileErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ReconcileErr {}
impl From<kube::Error> for ReconcileErr {
    fn from(e: kube::Error) -> Self {
        ReconcileErr(e.to_string())
    }
}

// ── volumes ──────────────────────────────────────────────────────────────

/// Everything one volume operation needs, as a struct rather than positional arguments that were
/// trivially swappable at the call site.
pub struct Work {
    pub id: String,
    pub owner: String,
    pub source: Option<VolumeSource>,
    pub materialize: bool,
    /// An in-place restore of THIS volume's own `live`, already gated by the parent (services
    /// down) and by `apply_volume` (not already restored).
    pub restore: Option<crd::RestoreWish>,
    pub quota_gb: u64,
}

// ── workspaces ───────────────────────────────────────────────────────────

// ── environments ─────────────────────────────────────────────────────────

// ── shared plumbing ──────────────────────────────────────────────────────
