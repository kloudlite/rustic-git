use kloudlite_git_server::boot::{host_key, run};
use kloudlite_git_server::config::{env, open_store};
use kloudlite_git_server::lanes::spawn_lease_tasks;
use kloudlite_git_server::listeners;
use kloudlite_git_server::store::Store;
use kloudlite_git_server::{err, hex, require_jwt_secret_from_env, App, Result};
use std::sync::Arc;

/// Start the server. This node opens whatever repo the balancer sends it and holds it warm
/// afterwards. Which node serves a repo is the ownership map's decision; which node WRITES the
/// map is elected by lease (`App::election_tick`). Either way a repo must land on exactly one
/// node, or the second opener fences the first.
/// How long the release of every warm database may take before the drain starts without it.
const RELEASE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(8);
/// Hard ceiling on the whole shutdown, enforced by a watchdog that exits the process.
const HARD_EXIT: std::time::Duration = std::time::Duration::from_secs(15);

async fn serve() -> Result<()> {
    let store = open_store(true).await?;
    store.spawn_health_probe();
    // The registry's 401 challenge carries this URL in a header; a value that cannot be one
    // must fail here, not on the first anonymous pull.
    kloudlite_git_server::registry::auth::check_external_url()?;

    let peer_addr = env("KLOUDLITE_GIT_PEER_ADDR", "0.0.0.0:8081");
    let peer_port: u16 = peer_addr
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| err("KLOUDLITE_GIT_PEER_ADDR must be host:port"))?;
    // Multi-node when a peer Service is configured, single node otherwise. Single node needs no
    // ownership map at all: with one node there is nothing to coordinate, so it claims everything
    // from an empty in-process map and never touches the ownership database.
    let svc = std::env::var("KLOUDLITE_GIT_PEER_SVC").unwrap_or_default();
    let (me, peer_secret, ownership) = if svc.is_empty() {
        // Random secret so nothing on the network can drive the peer port.
        use rand::RngCore;
        let mut b = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut b);
        let secret = hex(&b);
        (
            "kloudlite-git-0".to_string(),
            secret,
            kloudlite_git_server::ownership::OwnershipStore::solo(),
        )
    } else {
        let need = |k: &str| {
            std::env::var(k)
                .ok()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| err(format!("{k} is required with KLOUDLITE_GIT_PEER_SVC")))
        };
        // Checked here, where fleet mode is decided: App::new falls back to a random
        // per-process secret, which in a fleet means each node rejects the others' tokens.
        require_jwt_secret_from_env()?;
        // The lease that elects the map's writer is a conditional put; a backend without them
        // cannot fence a stale leader, so it is refused here rather than found out in a failover.
        kloudlite_git_server::config::fleet_store_ok(&env("KLOUDLITE_GIT_S3_URL", ""))?;
        let me = need("KLOUDLITE_GIT_SELF")?;
        let secret = need("KLOUDLITE_GIT_PEER_SECRET")?;
        let store = kloudlite_git_server::ownership::OwnershipStore::open(store.os.clone());
        (me, secret, store)
    };
    // A node name resolves to its peer listener through the StatefulSet's own identity: no
    // lookup, nothing that can be stale.
    let svc_for_addr = svc.clone();
    let addr_of: kloudlite_git_server::AddrOf = if svc.is_empty() {
        std::sync::Arc::new(move |_: &str| format!("127.0.0.1:{peer_port}"))
    } else {
        std::sync::Arc::new(move |n: &str| format!("{n}.{svc_for_addr}:{peer_port}"))
    };
    // The one thing a serving node still asks Mongo for: a repo's pre-existing pull requests, copied
    // into its own database on first touch. A failure here must NOT degrade to "nothing to migrate":
    // this node may own repos whose changes live only in Mongo, and recording them as migrated would
    // hide them for good. So it still serves git, but pull routes fail loudly until it is restarted
    // against a reachable directory.
    let dir = match std::env::var("KLOUDLITE_GIT_MONGO_URI").ok().filter(|s| !s.is_empty()) {
        Some(uri) => {
            match kloudlite_git_server::directory::Directory::connect(&uri, &env("KLOUDLITE_GIT_MONGO_DB", "kloudlite")).await {
                Ok(d) => kloudlite_git_server::pulls::Source::Directory(Arc::new(d)),
                Err(e) => {
                    tracing::warn!(error = %e, "directory.unavailable");
                    kloudlite_git_server::pulls::Source::Unavailable
                }
            }
        }
        None => kloudlite_git_server::pulls::Source::Absent,
    };
    let app = Arc::new(App::new(store.clone(), Arc::new(ownership), me, addr_of, peer_secret, dir));
    // One synchronous GET before serving anything, so the first request already sees whatever an
    // admin has already set rather than waiting out the first `SETTINGS_REFRESH_SECS` beat.
    // Missing key or a corrupt document: keep the env-only default and let the beat try again.
    if let Some(bytes) = kloudlite_git_server::config::get_central(&store.os).await {
        match serde_json::from_slice(&bytes) {
            Ok(doc) => app.central.store(
                kloudlite_git_core::settings::CentralSettings::from_env().merged_with(&doc),
            ),
            Err(e) => tracing::warn!(scope = "central", error = %e, "settings.invalid"),
        }
    }
    tokio::spawn(kloudlite_git_core::settings::refresh_central_beat(
        kloudlite_git_server::config::central_fetch(store.os.clone()),
        app.central.clone(),
    ));
    if !svc.is_empty() {
        // One beat before anything asks: a fresh fleet has no leader until somebody takes the
        // lease, and the first claim should not wait a tick for it. Not fatal — the loop retries
        // and /healthz stays un-ready until a lease is read.
        if let Err(e) = app.election_tick().await {
            tracing::warn!(attempt = 1, error = %e, "election.tick.failed");
        }
    }
    store.pool.spawn_sweeper();
    // The lifecycle invariant, both directions: eviction releases the lease before it closes the
    // database, and the renewal task closes any database whose lease we have lost. Single node has
    // neither — nothing to release to, nothing that can take a lease away.
    if !svc.is_empty() {
        store.pool.set_release_hook(
            Arc::downgrade(&app) as std::sync::Weak<dyn kloudlite_git_server::pool::ReleaseHook>
        );
        spawn_lease_tasks(app.clone());
    }

    let l = listeners::bind(&peer_addr).await?;
    let key = host_key(&env("KLOUDLITE_GIT_HOST_KEY", "./.local/host_key"))?;
    tracing::info!(listener = "http", addr = %l.http.local_addr()?, "listener.started");
    tracing::info!(listener = "ssh", addr = %l.ssh.local_addr()?, "listener.started");
    tracing::info!(listener = "peer", addr = %l.peer_http.local_addr()?, "listener.started");
    tracing::info!(listener = "peer", addr = %l.peer_stream.local_addr()?, "listener.started");
    tracing::info!(
        service = "server",
        warm_databases = store.pool.max_warm(),
        "process.started"
    );

    // SIGTERM: stop accepting, let in-flight requests finish, close every warm database. Without
    // this the kubelet's SIGTERM kills the process outright — in-flight clones and pushes die, the
    // pool is never closed, and the next opener replays the WAL. terminationGracePeriodSeconds is
    // meaningless without a handler that uses it.
    // Both HTTP listeners drain: for repos this node owns, most traffic arrives on the PEER
    // listener (forwarded from the other N-1 nodes), so draining only the public one would cut the
    // majority of in-flight requests. One SIGTERM, fanned out to both via a watch channel.
    // ORDER MATTERS, and the first deploy proved it: the pool must be released the instant SIGTERM
    // arrives, BEFORE the listeners drain — not after. Kubernetes drops a terminating pod from the
    // headless Service at once, so within a few seconds every peer stops seeing it, and the next
    // node to be sent one of its repos claims it once the lease lapses or is released. If this pod is still holding those databases while
    // it drains, that open fences it: in-flight requests here fail, the peer's next write flaps
    // ownership back, and the roll shows a burst of 503s in the middle of every preStop window
    // (measured: 1–2 failures per pod, 7–14 s after Killing, on three consecutive rolls). Releasing
    // first hands the repos over cleanly; a request in flight here that still needs its database
    // gets a prompt 503 — which is what the fence would have given it, minus the flap.
    let (term_tx, term_rx) = tokio::sync::watch::channel(false);
    let pool_for_term = store.pool.clone();
    let app_for_term = app.clone();
    tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("sigterm handler");
        term.recv().await;
        let began = std::time::Instant::now();
        tracing::info!(signal = "sigterm", "process.shutdown.begun");
        // A watchdog, because every step below has been observed to hang. Measured: the leader sat
        // through the whole 90s terminationGracePeriodSeconds and was SIGKILLed while every other
        // pod exited in 17s — and a SIGKILLed leader is a fleet-wide claim outage for the length of
        // the grace period, which is the single most expensive thing a roll can do here. Whatever
        // is stuck, the process leaves on time: the pool release below is what peers actually need,
        // and it is attempted first with its own bound.
        tokio::spawn(async {
            tokio::time::sleep(HARD_EXIT).await;
            tracing::error!(reason = "shutdown_watchdog", "process.exiting");
            // Exit 1, not 0: this path means shutdown hung and got cut short by the watchdog,
            // not that it finished cleanly. A 0 here made every hung-shutdown restart look like a
            // normal exit in the pod's exit-code history, hiding exactly the failure mode this
            // watchdog exists to catch.
            std::process::exit(1);
        });

        // Bounded: a release that cannot finish must not hold up the signal to drain. The lease
        // lapses on its own TTL if this does not land, which is the slower path but not a wrong one.
        match tokio::time::timeout(RELEASE_DEADLINE, pool_for_term.close()).await {
            Ok(()) => tracing::info!(
                duration_ms = began.elapsed().as_millis() as u64,
                "process.shutdown.completed"
            ),
            Err(_) => tracing::warn!(
                step = "pool_release",
                timeout_s = RELEASE_DEADLINE.as_secs(),
                "process.shutdown.stalled"
            ),
        }
        // AFTER the pool: releasing repos goes through the leader, which may be this node. Bounded
        // like everything here; an unreleased lease lapses on its TTL.
        if tokio::time::timeout(RELEASE_DEADLINE, app_for_term.resign()).await.is_err() {
            tracing::warn!(step = "resign", timeout_s = RELEASE_DEADLINE.as_secs(), "process.shutdown.stalled");
        }
        let _ = term_tx.send(true); // then let the listeners drain what is in flight
    });
    let wait = |mut rx: tokio::sync::watch::Receiver<bool>| async move {
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    };
    // Stop waiting for the drain after this long and exit anyway.
    //
    // `with_graceful_shutdown` waits for every CONNECTION to close, not merely for in-flight
    // requests to finish — and followers hold pooled keep-alive connections to the leader's peer
    // port, reusing them for a renewal every RENEW_EVERY. Those connections never go idle, so the
    // leader never finished draining: measured, it sat through the whole 90s
    // terminationGracePeriodSeconds and was SIGKILLed, while every other pod exited in 17s. That
    // made a leader restart a ninety-second window with no grants anywhere in the fleet, and made
    // a rolling restart take 112s instead of 37s.
    //
    // The pool is already released before this point, so nothing here is holding a database; what
    // remains is idle sockets and whatever request is genuinely in flight.
    const DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);
    let deadline = {
        let mut rx = term_rx.clone();
        async move {
            while !*rx.borrow() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
            tokio::time::sleep(DRAIN_DEADLINE).await;
        }
    };
    let (a2, a3, a4) = (app.clone(), app.clone(), app.clone());
    let http_srv = axum::serve(l.http, kloudlite_git_server::router::router(a2))
        .with_graceful_shutdown(wait(term_rx.clone()));
    let peer_srv = axum::serve(l.peer_http, kloudlite_git_server::router::peer_router(a3))
        .with_graceful_shutdown(wait(term_rx.clone()));
    // Both HTTP servers as ONE select arm: select! returns when its first arm resolves, and if
    // each server were its own arm the first to finish draining would end the select and
    // pool.close() would run under the other's in-flight requests. try_join waits for both.
    tokio::select! {
        r = async { tokio::try_join!(http_srv, peer_srv) } => { r?; }
        _ = deadline => { tracing::warn!(step = "drain", timeout_s = DRAIN_DEADLINE.as_secs(), "process.shutdown.stalled"); }
        r = kloudlite_git_server::proxy::serve_peer_streams(a4, l.peer_stream) => { r?; }
        r = kloudlite_git_server::ssh::serve(app.clone(), l.ssh, key) => { r?; }
    }
    // ponytail: the SSH and peer-stream listeners stop on select! exit without draining; the
    // preStop delay is what makes that rare (the pod has left DNS before it stops). Add per-session
    // tracking if SSH sessions being cut on roll ever matters.
    // A second close() is a no-op after the SIGTERM path already ran it; it covers the non-signal
    // exits (a listener error) so those still flush. The ownership map RESIGNS with it (a no-op
    // after the SIGTERM path already did): the leader lease is given back and the writer, whose
    // last writes are still inside the 10ms flush window, is closed by demotion — which leaves a
    // reader behind, so a checkpoint beat that fires late finds a follower, never a closed handle.
    // Bounded like the SIGTERM path: with the leader down every release waits out its retries,
    // and an unbounded close here left only the watchdog's exit 1 to end the process.
    if tokio::time::timeout(RELEASE_DEADLINE, store.pool.close()).await.is_err() {
        tracing::warn!(step = "final_pool_release", timeout_s = RELEASE_DEADLINE.as_secs(), "process.shutdown.stalled");
    }
    app.resign().await;
    Ok(())
}

/// Every series this tier can emit, so a quiet node still exports them. The labelled entries are
/// exactly the combinations `deploy/alerts.md`'s rules filter on — `status="5xx"` and `"421"` per
/// listener for `Http5xxRate`/`MisdirectedWrites` — plus the two `op` values the git counters have,
/// since a counter with a label has no unlabelled series to fall back on.
fn register_metrics() {
    use kloudlite_git_core::metrics::Kind::*;
    let mut series: Vec<kloudlite_git_core::metrics::Series> = vec![
        ("http_request_duration_seconds", Histogram, &[]),
        ("ownership_is_leader", Gauge, &[]),
        ("ownership_map_size", Gauge, &[]),
        ("ownership_election_failures_total", Counter, &[]),
        ("ownership_renew_failures_total", Counter, &[]),
        ("ownership_demotions_total", Counter, &[]),
        ("ownership_claims_total", Counter, &[("result", "moved")]),
        ("db_fence_detected_total", Counter, &[]),
        ("git_pack_requests_total", Counter, &[("op", "upload")]),
        ("git_pack_requests_total", Counter, &[("op", "receive")]),
        ("git_pack_bytes_in_total", Counter, &[("op", "upload")]),
        ("git_pack_bytes_in_total", Counter, &[("op", "receive")]),
        ("git_pack_duration_seconds", Histogram, &[]),
        ("merge_stranded_total", Counter, &[]),
        ("registry_blob_bytes_in_total", Counter, &[]),
        ("registry_blob_bytes_out_total", Counter, &[]),
    ];
    // The label ORDER is the middleware's, because a key with the same labels in another order is
    // a different series and would export the set twice. `probe` is the class every listener is
    // guaranteed to serve, so the registered series is one the middleware itself will also use.
    for l in [
        &[("listener", "public"), ("class", "probe"), ("status", "5xx")] as &'static [(&str, &str)],
        &[("listener", "public"), ("class", "probe"), ("status", "421")],
        &[("listener", "peer"), ("class", "probe"), ("status", "5xx")],
        &[("listener", "peer"), ("class", "probe"), ("status", "421")],
    ] {
        series.push(("http_requests_total", Counter, l));
    }
    kloudlite_git_core::metrics::register(&series);
}

#[tokio::main]
async fn main() -> Result<()> {
    kloudlite_git_core::log::init();
    kloudlite_git_core::metrics::init();
    // Its own listener, like every other binary's: the peer port is secret-gated, and metrics
    // text names every repository key this node has touched.
    kloudlite_git_core::metrics::serve_if_configured().await;
    register_metrics();
    // See config::install_crypto_provider — it must happen before any TLS, and
    // `admin` subcommands reach object storage without going through open_store.
    kloudlite_git_server::config::install_crypto_provider();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    if a.first() == Some(&"serve") {
        let r = serve().await;
        if let Err(e) = r {
            eprintln!("{e}");
            std::process::exit(2);
        }
        return Ok(());
    }
    let store: Arc<Store> = open_store(false).await?;
    let r = run(&a, &store).await;
    store.pool.close().await;
    if let Err(e) = r {
        eprintln!("{e}");
        std::process::exit(2);
    }
    Ok(())
}
