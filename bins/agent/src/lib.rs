//! Process setup for `kloudlite-agent`: the local `Engine`, the storage janitor (`janitor.rs`),
//! and the
//! Kubernetes client the node controller (`controller/`) reconciles with. The work itself is
//! there, not here — the CRD IS the work item, so there is no queue, no lease and no poll loop.
//!
//! # Tests that do not run in CI
//!
//! CI has no loopback btrfs and no root. `janitor::cleanup_local_deletes_nested_worktree_subvolumes`
//! is the only test gated explicitly on `have_btrfs()` and the only one exercising `cleanup_local`
//! against real subvolumes; every other test of that path proves `btrfs_delete`'s test-only
//! `remove_dir_all` fallback instead. Several more pass on a Mac only because the code
//! short-circuits before touching btrfs (each carries an `IMPLICITLY GATED` doc line saying which
//! short-circuit). If you change the engine so a path that used to return early now shells out,
//! those tests keep passing here and fail on a node — run `tests/ws_e2e.sh` on the Linux VM.

use kloudlite_core::settings::LiveSettings;
use kloudlite_workspaces::crd;
use kloudlite_workspaces::engine::{Engine, Pool};
use kloudlite_workspaces::settings::AgentSettings;
use std::sync::Arc;

pub mod binding;
pub mod claim;
pub mod controller;
pub mod decommission;
pub mod janitor;
pub mod listing;
pub mod nix;
pub mod peer;
pub mod snapshot;
pub mod sshkeys;
pub mod stats;
pub mod sync;
pub mod usage;
#[cfg(test)]
mod testsupport;

/// Env-derived config for `run`. `WS_REGISTRY_URL`/`WS_AGENT_TOKEN` are gone with the
/// object-store registry surface they pointed at (Task 8).
pub struct Config {
    pub region: String,
    pub pool: String,
    /// This node's name, from the downward-API `NODE_NAME`. It is the shard key: the controller
    /// watches only objects whose `spec.nodeName` equals it.
    pub node: String,
    /// `WS_REGISTRY_HOST`: the platform registry's external host (no scheme), the same value
    /// `registry::auth::realm()`'s host half resolves to on the api tier — the agent has no route
    /// to that env, so it is configured here instead. Fed into every workspace pod as
    /// `KL_REGISTRY_HOST`, the docker credential helper's `credHelpers` key.
    pub registry_host: String,
    /// `WS_API_URL`: the public api base, fed into every bench pod as `KL_API_URL`. Empty leaves
    /// it unset, so bench tools fail closed.
    pub api_url: String,
}

impl Config {
    pub fn from_env() -> Config {
        Config {
            region: std::env::var("WS_REGION").unwrap_or_else(|_| "default".into()),
            pool: std::env::var("WS_POOL").unwrap_or_else(|_| "/mnt/wspool".into()),
            // Declared capacity is gone: the kubelet reports node allocatable, and a second
            // hand-maintained copy of it is a second thing that can be wrong.
            node: std::env::var("NODE_NAME").unwrap_or_default(),
            registry_host: std::env::var("WS_REGISTRY_HOST").unwrap_or_default(),
            api_url: std::env::var("WS_API_URL").unwrap_or_default(),
        }
    }
}

/// Boots the node controller: Engine, janitor, Kubernetes client, then reconcile forever.
pub async fn run(cfg: Config) -> Result<(), String> {
    let engine = Arc::new(Engine::new(Pool::new(&cfg.pool)));
    let nix_client: Arc<dyn nix::Nix> = Arc::new(nix::RealNix { bin: "/nix/var/nix/profiles/default/bin".into() });
    if cfg.node.is_empty() {
        return Err("NODE_NAME is unset: the controller would watch every node's objects".into());
    }
    // Env-only, ahead of `LiveSettings` — this validates the pin the process is ABOUT to build
    // with, before anything (including the settings merge below) can build with it.
    let pin = nix::nixpkgs_pin_env();
    if pin.is_empty() {
        return Err("WS_NIXPKGS is required: the nixpkgs pin every profile on this node is built against".into());
    }
    // A branch or a tag would make the same package hash mean different bits on different days,
    // which is the one promise the profile hash makes.
    if !nix::valid_pin(&pin) {
        return Err(format!("WS_NIXPKGS must be github:NixOS/nixpkgs/<40-hex-rev>, not {pin:?}"));
    }
    if let Err(e) = std::fs::create_dir_all(nix::PROFILES_DIR) {
        tracing::warn!(error = %e, "nix.profiles.dir.failed");
    }
    // One indirect root over the whole profiles tree: `nix build --no-link` registers none, and
    // the publish rename would orphan an out-link's auto-root anyway.
    nix::ensure_gcroot();
    // The CRDs must be Established before the watch starts, or it fails at startup and the
    // controller sits idle looking healthy. Fail loudly here rather than in production.
    // `infer`, then a READ timeout: kube's default is none, so a request the API server queues —
    // priority-and-fairness held one node's `OwnerKeys` status patch for nine minutes behind a
    // patch storm on 2026-09-11 — blocks the loop that made it for as long as the server likes.
    // 120 s never cuts a healthy watch: every watch here asks for `timeoutSeconds` 60
    // (`controller::watch_config`), so the server ends it first.
    let mut config = kube::Config::infer().await.map_err(|e| e.to_string())?;
    config.read_timeout = Some(std::time::Duration::from_secs(120));
    // Through the fleet's one bounded constructor: `read_timeout` is the connection's bound, and a
    // request the server QUEUES needs one of its own — 30 s per non-watch call, `kube.slow` past
    // 2 s (`k8s::client`). The keys step of one reconcile blocked 60 s on 2026-09-12 and every
    // other object on the node aged behind it.
    let client = kloudlite_workspaces::k8s::client::bounded_client(config).map_err(|e| e.to_string())?;
    let has_pool = node_has_pool(&client, &cfg.node).await;
    tracing::info!(node = %cfg.node, has_pool, "node.pool");
    // Resolved BEFORE `Ctx`: `Ctx::new` reads the boot-marked fields (`default_image`,
    // `git_init_image`, `runtime_class`) straight off this handle instead of `std::env` itself,
    // so the CRD's admin-written value is what a fresh pod boots with, not just what a running
    // one picks up later.
    let settings = LiveSettings::new(initial_settings(&client).await);
    // The sampler reads this region's `ClusterSettings` through the live handle, never env.
    kloudlite_trace::bind_ratio({
        let s = settings.clone();
        move || s.load().trace_sample_ratio
    });
    kloudlite_trace::bind_probe_budget({
        let s = settings.clone();
        move || {
            let v = s.load();
            (v.trace_probe_rate, v.trace_probe_burst)
        }
    });
    kloudlite_trace::bind_promote_budget({
        let s = settings.clone();
        move || {
            let v = s.load();
            (v.trace_promote_rate, v.trace_promote_burst)
        }
    });
    kloudlite_workspaces::k8s::stall_dump::ENABLED.store(settings.load().stall_dumps, std::sync::atomic::Ordering::Relaxed);
    // The gauges the collector cannot get from the kubelet: the btrfs pool is this process's
    // filesystem to read, and "working copies running here" is this node's own view. Must run
    // before `Ctx::new` below, which moves `cfg.pool`/`cfg.node`.
    janitor::spawn_janitor(cfg.pool.clone(), nix_client.clone(), client.clone());
    stats::spawn_stats(cfg.pool.clone(), client.clone(), cfg.node.clone());
    let ctx = Arc::new(controller::Ctx::new(client.clone(), engine, cfg.node, cfg.pool, cfg.region, has_pool, cfg.registry_host, cfg.api_url, nix_client, nix::PROFILES_DIR.into(), settings.clone()));
    spawn_settings_reflector(client, settings);
    // Not a Controller: `OwnerKeys` is cluster-wide, every node converges every object, and there
    // is no per-node sharding to reconcile against.
    tokio::spawn(controller::keys::run(ctx.clone()));
    // Fail closed: no `WS_PEER_SECRET` means no listener at all, never one guarded by an empty
    // secret that would compare-equal to a missing header.
    if let Some(secret) = kloudlite_core::secret::read("WS_PEER_SECRET") {
        if !secret.is_empty() {
            let peer_ctx = ctx.clone();
            tokio::spawn(async move {
                if let Err(e) = peer::serve(&peer_ctx, secret).await {
                    tracing::error!(listener = "peer", error = %e, "listener.failed");
                }
            });
        }
    }
    controller::run(ctx).await
}

/// `SETTINGS_REFRESH_SECS`, default 30 — bootstrap-only, so this one stays a plain env read even
/// though everything it governs is now live: there is no live source to refresh THIS with.
fn settings_refresh_interval() -> std::time::Duration {
    std::time::Duration::from_secs(std::env::var("SETTINGS_REFRESH_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(30))
}

/// The one-shot GET `Ctx::new` needs before it can build a pod template. Missing object or a
/// failed GET falls back to `AgentSettings::from_env()` alone — first boot, or a region that has
/// never had a `ClusterSettings/default` written; the reflector spawned right after this will
/// pick up the real one the moment it exists.
async fn initial_settings(client: &kube::Client) -> AgentSettings {
    let base = AgentSettings::from_env();
    let api: kube::Api<crd::ClusterSettings> = kube::Api::all(client.clone());
    match api.get_opt("default").await {
        Ok(Some(obj)) => base.merged_with(&obj.spec),
        Ok(None) => base,
        Err(e) => {
            tracing::warn!(scope = "cluster", mode = "env-only", error = %e, "settings.unavailable");
            base
        }
    }
}

/// Keeps `settings` live for the rest of the process: a watch on the single `default` object —
/// the first CLUSTER-WIDE singleton watch this agent does, everything else shards by node — plus
/// a periodic re-GET (`settings_refresh_interval`) as the backstop for a watch event this node
/// missed (a reconnect gap, an apiserver restart). "Last good wins": a spec that fails to
/// deserialize (a future field, a hand-edit with the wrong type) surfaces as a stream error, is
/// logged once, and changes nothing — the process keeps whatever it last applied.
fn spawn_settings_reflector(client: kube::Client, settings: LiveSettings<AgentSettings>) {
    tokio::spawn(async move {
        use futures::StreamExt;
        use kube::runtime::{watcher, WatchStreamExt};
        let api: kube::Api<crd::ClusterSettings> = kube::Api::all(client.clone());
        let cfg = crate::controller::watch_config().fields("metadata.name=default");
        let watched = api.clone();
        let new_stream =
            move || watcher(watched.clone(), cfg.clone()).default_backoff().applied_objects().boxed();
        let mut events = new_stream();
        let mut tick = tokio::time::interval(settings_refresh_interval());
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                event = events.next() => {
                    match event {
                        Some(Ok(obj)) => apply_settings(&api, &settings, obj).await,
                        // A malformed spec never reaches here as `Ok` — kube-runtime's own
                        // decode failed first, which IS the "logged once, changes nothing" case.
                        Some(Err(e)) => tracing::warn!(scope = "cluster", error = %e, "settings.invalid"),
                        // Rebuilt, never fatal — the same rule (and the same 2026-09-08 cause) as
                        // `controller::keys::run_with`, whose test pins the shape.
                        None => {
                            tracing::warn!(scope = "cluster", "settings.watch.ended");
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            events = new_stream();
                        }
                    }
                }
                _ = tick.tick() => {
                    if let Ok(Some(obj)) = api.get_opt("default").await {
                        apply_settings(&api, &settings, obj).await;
                    }
                }
            }
        }
    });
}

/// The store, plus the one write-back the spec asks for: `status.observedGeneration`, so the
/// admin UI's pending marker has something to compare `metadata.generation` against. Never called
/// from the boot-time `initial_settings` load — only a watch/refresh event that actually reached
/// this node earns the write.
async fn apply_settings(api: &kube::Api<crd::ClusterSettings>, settings: &LiveSettings<AgentSettings>, obj: crd::ClusterSettings) {
    let merged = AgentSettings::from_env().merged_with(&obj.spec);
    kloudlite_workspaces::k8s::stall_dump::ENABLED.store(merged.stall_dumps, std::sync::atomic::Ordering::Relaxed);
    settings.store(merged);
    let body = serde_json::json!({
        "apiVersion": format!("{}/{}", crd::GROUP, crd::VERSION),
        "kind": "ClusterSettings",
        "status": {"observedGeneration": obj.metadata.generation},
    });
    let params = kube::api::PatchParams::apply(crd::AGENT_FIELD_MANAGER).force();
    if let Err(e) = api.patch_status("default", &params, &kube::api::Patch::Apply(&body)).await {
        tracing::warn!(scope = "cluster", error = %e, "settings.status.write.failed");
    }
}

/// Whether this node carries `kloudlite.io/pool`. An unreadable Node object reads as false, so
/// the agent converges what it already owns and claims nothing new — the safe direction, since the
/// alternative is claiming work for a pool this box may not have.
async fn node_has_pool(client: &kube::Client, node: &str) -> bool {
    let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(client.clone());
    let Ok(Some(n)) = api.get_opt(node).await else {
        tracing::warn!(%node, reason = "unreadable", "node.labels.missing");
        return false;
    };
    let labels = n.metadata.labels.unwrap_or_default();
    let has_pool = labels.get("kloudlite.io/pool").map(String::as_str) == Some("true");
    if !has_pool {
        // No pool label means no claim watches, and an agent with no claim watch looks identical
        // to a healthy one from the outside — it just never picks anything up. Say so.
        tracing::warn!(%node, reason = "no-pool-label", "node.labels.missing");
    }
    has_pool
}

#[cfg(test)]
mod tests {
    /// The one gate on the claim watches. A node without the pool label starts neither — the
    /// spec's rule — and that is decided here, before any controller is built.
    #[tokio::test]
    async fn a_node_without_the_pool_label_has_no_pool_and_one_with_it_does() {
        let bare = serde_json::json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": "cp"}});
        let (client, _) = kloudlite_workspaces::kube_test::mock_client(vec![kloudlite_workspaces::kube_test::get("/api/v1/nodes/cp", bare)]);
        assert!(!super::node_has_pool(&client, "cp").await);
        let pool = serde_json::json!({"apiVersion": "v1", "kind": "Node",
            "metadata": {"name": "n1", "labels": {"kloudlite.io/pool": "true", "kloudlite.io/session": "true"}}});
        let (client, _) = kloudlite_workspaces::kube_test::mock_client(vec![kloudlite_workspaces::kube_test::get("/api/v1/nodes/n1", pool)]);
        assert!(super::node_has_pool(&client, "n1").await);
    }
}
