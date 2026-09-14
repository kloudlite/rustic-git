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

    let ctx = Arc::new(Ctx {
        client: client.clone(),
        holder: cfg.holder,
        region: cfg.region,
        epoch: Default::default(),
        applied: Default::default(),
        settings: settings.clone(),
    });
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
        // Re-read EVERY tick and pass that object to `write`: the CAS is the resourceVersion the
        // read carried, so a `cur` held across ticks would be a write with no fence at all.
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
        let now = k8s_openapi::jiff::Timestamp::now();
        let now_ms = now.as_millisecond().max(0) as u64;
        match lease::decide(now_ms, &ctx.holder, view.as_ref()) {
            lease::Step::Wait => ctx.demote("held elsewhere"),
            step => match lease::write(&api, &ctx.holder, &step, cur.as_ref(), now).await {
                Ok(Some(l)) => match lease::view(&l) {
                    Some(v) if v.holder == ctx.holder => ctx.promote(v.transitions),
                    _ => ctx.demote("lost the write"),
                },
                // A 409: another pod won this round. Next tick re-reads — never an immediate
                // retry, which would race the winner with the stale object we already lost on.
                Ok(None) => ctx.demote("lost the CAS"),
                Err(e) => {
                    ctx.demote("lease.unwritable");
                    tracing::warn!(error = %e, "leader.write.failed");
                }
            },
        }
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
