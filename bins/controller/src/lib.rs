//! `kloudlite-controller`: the cluster's single elected writer of every object shared across nodes
//! or derived purely from spec. Stage 1 owns exactly one thing — a space's two NetworkPolicies —
//! and the node agents stop writing them in the same release. See
//! `docs/superpowers/specs/2026-09-14-cluster-controller-design.md`.
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

use kloudlite_core::settings::LiveSettings;
use kloudlite_workspaces::crd;
use kloudlite_workspaces::settings::AgentSettings;
use std::sync::Arc;

pub use ctx::{Config, Ctx};

pub async fn run(cfg: Config) -> Result<(), String> {
    // Same bounded client the agent and the api tier use: kube's own pool sets no TCP keepalive,
    // and a connection that died without a RST is otherwise handed out and written into nothing
    // (`k8s::client::bounded_client`). A controller whose only dependency is this connection
    // cannot afford that.
    let mut config = kube::Config::infer().await.map_err(|e| e.to_string())?;
    config.read_timeout = Some(std::time::Duration::from_secs(120));
    let client =
        kloudlite_workspaces::k8s::client::bounded_client(config).map_err(|e| e.to_string())?;

    // `stored ?? env ?? default` through the one handle, never `std::env::var` for a knob that has
    // a `Settings` field. Stage 1 reads nothing from it yet; the wiring is here so the first knob
    // that needs it has somewhere to land and the process does not grow a second settings path.
    let settings = LiveSettings::new(initial_settings(&client).await);
    kloudlite_trace::bind_ratio({
        let s = settings.clone();
        move || s.load().trace_sample_ratio
    });

    let ctx = Arc::new(Ctx::new(client.clone(), cfg.holder, cfg.region, settings.clone()));
    spawn_settings_reflector(client, settings);

    let l = tokio::net::TcpListener::bind("0.0.0.0:8080")
        .await
        .map_err(|e| format!("binding 8080: {e}"))?;
    tracing::info!(listener = "http", addr = "0.0.0.0:8080", holder = %ctx.holder, region = %ctx.region, "listener.started");
    let serving = axum::serve(l, health::app(ctx.clone()));

    tokio::select! {
        r = serving => r.map_err(|e| format!("serving: {e}")),
        _ = elect(ctx.clone()) => Err("election loop ended".into()),
        _ = space::run(ctx.clone()) => Err("reconcilers ended".into()),
        // Hand the lease back rather than making the replacement wait out the TTL. Drop the epoch
        // FIRST, so nothing still in flight writes under a term we are about to blank.
        sig = shutdown_signal() => {
            ctx.demote(sig);
            lease::release(&lease_api(&ctx), &ctx.holder).await;
            tracing::info!(signal = sig, holder = %ctx.holder, "process.stopping");
            Ok(())
        }
    }
}

fn lease_api(ctx: &Ctx) -> kube::Api<k8s_openapi::api::coordination::v1::Lease> {
    kube::Api::namespaced(ctx.client.clone(), lease::LEASE_NAMESPACE)
}

/// SIGTERM (the kubelet) or SIGINT (a terminal).
async fn shutdown_signal() -> &'static str {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(t) => t,
        // No handler is not a reason to exit: the TTL still covers a hard stop.
        Err(_) => return std::future::pending().await,
    };
    tokio::select! {
        _ = term.recv() => "sigterm",
        _ = tokio::signal::ctrl_c() => "sigint",
    }
}

/// The election beat: read, decide, write, every `RENEW`. It never returns.
///
/// Demotion is driven from HERE and nowhere else: a write path only ASKS (`Ctx::leading`, then
/// `lease::may_write` against a fresh read), because a write that discovers a newer term must
/// abandon itself, not go around re-electing.
pub async fn elect(ctx: Arc<Ctx>) {
    let api = lease_api(&ctx);
    let mut tick = tokio::time::interval(lease::RENEW);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        tick_once(&ctx, &api).await;
    }
}

/// One beat, lifted out of the loop so it can be scripted against a canned API server. At most ONE
/// write per call: every branch that does not land the write leaves the next tick to re-read.
async fn tick_once(ctx: &Ctx, api: &kube::Api<k8s_openapi::api::coordination::v1::Lease>) {
    // Re-read EVERY tick and pass that object to `write`: the CAS is the resourceVersion the
    // read carried, so a `cur` held across ticks would be a write with no fence at all.
    let cur = match lease::read(api).await {
        Ok(c) => c,
        Err(e) => {
            // Unreachable API server: we stop claiming to lead, because our term may have been
            // taken while we could not look. Keep-biased in the only direction that is safe here —
            // a follower writes nothing, and nothing it owns degrades.
            ctx.demote("lease.unreadable");
            tracing::warn!(error = %e, "leader.read.failed");
            return;
        }
    };
    let view = cur.as_ref().and_then(lease::view);
    let now = k8s_openapi::jiff::Timestamp::now();
    let now_ms = now.as_millisecond().max(0) as u64;
    match lease::decide(now_ms, &ctx.holder, view.as_ref()) {
        lease::Step::Wait => ctx.demote("held elsewhere"),
        step => match lease::write(api, &ctx.holder, &step, cur.as_ref(), now).await {
            Ok(Some(l)) => match lease::view(&l) {
                // Epoch 0 is the "never elected" sentinel `Ctx::leading` reads, so a lease that
                // echoes our own identity at transitions 0 (an object written by hand, or by a
                // client that omits the field) would leave us holding it and never writing.
                // Refuse the term rather than lead invisibly; `decide` advances it next tick.
                Some(v) if v.holder == ctx.holder && v.transitions == 0 => {
                    ctx.demote("epoch zero");
                    tracing::warn!(holder = %ctx.holder, "leader.epoch.zero");
                }
                Some(v) if v.holder == ctx.holder => ctx.promote(v.transitions),
                _ => ctx.demote("lost the write"),
            },
            // A 409: another pod won this round. Next tick re-reads — never an immediate retry,
            // which would race the winner with the stale object we already lost on.
            Ok(None) => ctx.demote("lost the CAS"),
            Err(e) => {
                ctx.demote("lease.unwritable");
                tracing::warn!(error = %e, "leader.write.failed");
            }
        },
    }
}

/// `SETTINGS_REFRESH_SECS`, default 30 — bootstrap-only, so this one stays a plain env read even
/// though everything it governs is now live: there is no live source to refresh THIS with.
fn settings_refresh_interval() -> std::time::Duration {
    std::time::Duration::from_secs(
        std::env::var("SETTINGS_REFRESH_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(30),
    )
}

/// The one-shot GET at boot. Missing object or a failed GET falls back to
/// `AgentSettings::from_env()` alone — first boot, or a region that has never had a
/// `ClusterSettings/default` written; the reflector spawned right after this picks up the real one
/// the moment it exists.
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

/// Keeps `settings` live for the rest of the process: a watch on the single `default` object plus
/// a periodic re-GET (`settings_refresh_interval`) as the backstop for an event this process
/// missed (a reconnect gap, an apiserver restart). "Last good wins": a spec that fails to
/// deserialize surfaces as a stream error, is logged once, and changes nothing.
///
/// Unlike the agent's copy this writes no `status.observedGeneration` back: that is a WRITE, every
/// node's agent already makes it, and a follower controller must write nothing at all.
fn spawn_settings_reflector(client: kube::Client, settings: LiveSettings<AgentSettings>) {
    tokio::spawn(async move {
        use futures::StreamExt;
        use kube::runtime::{watcher, WatchStreamExt};
        let api: kube::Api<crd::ClusterSettings> = kube::Api::all(client.clone());
        let cfg = watcher::Config::default().timeout(60).fields("metadata.name=default");
        let watched = api.clone();
        let new_stream = move || {
            watcher(watched.clone(), cfg.clone()).default_backoff().applied_objects().boxed()
        };
        let mut events = new_stream();
        let mut tick = tokio::time::interval(settings_refresh_interval());
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                event = events.next() => {
                    match event {
                        Some(Ok(obj)) => settings.store(AgentSettings::from_env().merged_with(&obj.spec)),
                        Some(Err(e)) => tracing::warn!(scope = "cluster", error = %e, "settings.invalid"),
                        // Rebuilt, never fatal — the same rule (and the same 2026-09-08 cause) as
                        // the agent's own watch loops.
                        None => {
                            tracing::warn!(scope = "cluster", "settings.watch.ended");
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            events = new_stream();
                        }
                    }
                }
                _ = tick.tick() => {
                    if let Ok(Some(obj)) = api.get_opt("default").await {
                        settings.store(AgentSettings::from_env().merged_with(&obj.spec));
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use kloudlite_workspaces::kube_test::{self, Route};

    const PATH: &str =
        "/apis/coordination.k8s.io/v1/namespaces/kube-system/leases/kloudlite-controller";
    const ME: &str = "ctl-test";

    /// `renewTime` now, so the lease reads as live; the tests that want an expired one pass a date
    /// far enough back that no TTL covers it.
    fn lease_json(holder: &str, transitions: i32, renewed: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": { "name": lease::LEASE_NAME, "namespace": lease::LEASE_NAMESPACE, "resourceVersion": "7" },
            "spec": {
                "holderIdentity": holder,
                "leaseTransitions": transitions,
                "leaseDurationSeconds": 15,
                "renewTime": renewed,
            },
        })
    }

    fn now_rfc3339() -> String {
        k8s_openapi::jiff::Timestamp::now().to_string()
    }

    fn put(body: serde_json::Value) -> Route {
        Route { method: "PUT", path: PATH.into(), status: 200, body }
    }

    async fn run_tick(routes: Vec<Route>, start_epoch: u32) -> (Ctx, Vec<String>) {
        let (client, rec) = kube_test::mock_client(routes);
        let ctx = Ctx::for_test_with(client.clone());
        if start_epoch != 0 {
            ctx.promote(start_epoch);
        }
        let api = lease_api(&ctx);
        tick_once(&ctx, &api).await;
        let calls = rec.calls();
        (ctx, calls)
    }

    /// (a) We hold it, we renew, and somebody else got there first: a 409 demotes and the tick
    /// ENDS. A retry inside the same tick would write our stale object over the winner's term.
    #[tokio::test]
    async fn a_renew_that_loses_the_cas_demotes_and_writes_once() {
        let (ctx, calls) = run_tick(
            vec![
                kube_test::get(PATH, lease_json(ME, 4, &now_rfc3339())),
                kube_test::conflict("PUT", PATH),
            ],
            4,
        )
        .await;
        assert!(!ctx.leading());
        assert_eq!(calls, vec![format!("GET {PATH}"), format!("PUT {PATH}")]);
    }

    /// (b) Held by a live peer: `Wait`. Not one byte is written — this is what the TTL means.
    #[tokio::test]
    async fn a_live_peers_lease_is_waited_on_without_a_write() {
        let (ctx, calls) =
            run_tick(vec![kube_test::get(PATH, lease_json("ctl-b", 4, &now_rfc3339()))], 4).await;
        assert!(!ctx.leading());
        assert_eq!(calls, vec![format!("GET {PATH}")]);
    }

    /// (c) A follower finds an expired lease, takes it, and adopts the epoch the API SERVER echoed
    /// — never the one it asked for: the echo is the only term any other pod will see.
    #[tokio::test]
    async fn an_expired_lease_is_taken_and_the_echoed_epoch_is_adopted() {
        let (ctx, calls) = run_tick(
            vec![
                kube_test::get(PATH, lease_json("ctl-b", 4, "2020-01-01T00:00:00.000000Z")),
                put(lease_json(ME, 5, &now_rfc3339())),
            ],
            0,
        )
        .await;
        assert!(ctx.leading());
        assert_eq!(ctx.epoch(), 5);
        assert_eq!(calls, vec![format!("GET {PATH}"), format!("PUT {PATH}")]);
    }

    /// (d) The API server is unreachable: demote and write nothing. Our term may have ended while
    /// we could not look, and a follower that writes nothing degrades nothing.
    #[tokio::test]
    async fn an_unreadable_lease_demotes_without_writing() {
        let route =
            Route { method: "GET", path: PATH.into(), status: 500, body: serde_json::json!({}) };
        let (ctx, calls) = run_tick(vec![route], 4).await;
        assert!(!ctx.leading());
        assert_eq!(calls, vec![format!("GET {PATH}")]);
    }

    /// An echo carrying transitions 0 is refused rather than promoted: epoch 0 IS "not the leader"
    /// to every write path, so promoting it would leave this pod holding the lease and writing
    /// nothing, forever.
    #[tokio::test]
    async fn an_echoed_epoch_of_zero_is_refused() {
        let (ctx, _) = run_tick(
            vec![
                kube_test::get(PATH, lease_json(ME, 0, "2020-01-01T00:00:00.000000Z")),
                put(lease_json(ME, 0, &now_rfc3339())),
            ],
            0,
        )
        .await;
        assert!(!ctx.leading());
    }
}
