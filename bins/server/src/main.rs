use rustic_git_server::boot::{host_key, run};
use rustic_git_server::config::{env, open_store};
use rustic_git_server::lanes::spawn_lease_tasks;
use rustic_git_server::listeners;
use rustic_git_server::store::Store;
use rustic_git_server::{err, hex, require_jwt_secret_from_env, App, Result};
use std::sync::Arc;

/// Start the server. This node opens whatever repo the balancer sends it and holds it warm
/// afterwards. Nothing is elected here: which node serves a repo is the balancer's decision, and
/// it must route a repo to exactly one node, or the second opener fences the first.
/// How long the release of every warm database may take before the drain starts without it.
const RELEASE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(8);
/// Hard ceiling on the whole shutdown, enforced by a watchdog that exits the process.
const HARD_EXIT: std::time::Duration = std::time::Duration::from_secs(15);

async fn serve() -> Result<()> {
    let store = open_store(true).await?;
    store.spawn_health_probe();
    // The registry's 401 challenge carries this URL in a header; a value that cannot be one
    // must fail here, not on the first anonymous pull.
    rustic_git_server::registry::auth::check_external_url()?;

    let peer_addr = env("RUSTIC_GIT_PEER_ADDR", "0.0.0.0:8081");
    let peer_port: u16 = peer_addr
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| err("RUSTIC_GIT_PEER_ADDR must be host:port"))?;
    // Multi-node when a peer Service is configured, single node otherwise. Single node needs no
    // ownership map at all: with one node there is nothing to coordinate, so it claims everything
    // from an empty in-process map and never touches the ownership database.
    let svc = std::env::var("RUSTIC_GIT_PEER_SVC").unwrap_or_default();
    let (me, peer_secret, ownership, leader_for_app) = if svc.is_empty() {
        // Random secret so nothing on the network can drive the peer port.
        use rand::RngCore;
        let mut b = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut b);
        let secret = hex(&b);
        (
            "rustic-git-0".to_string(),
            secret,
            rustic_git_server::ownership::OwnershipStore::solo(),
            "rustic-git-0".to_string(),
        )
    } else {
        let need = |k: &str| {
            std::env::var(k)
                .ok()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| err(format!("{k} is required with RUSTIC_GIT_PEER_SVC")))
        };
        // Checked here, where fleet mode is decided: App::new falls back to a random
        // per-process secret, which in a fleet means each node rejects the others' tokens.
        require_jwt_secret_from_env()?;
        let me = need("RUSTIC_GIT_SELF")?;
        let secret = need("RUSTIC_GIT_PEER_SECRET")?;
        // Fails loudly on a malformed name: the leader is derived from it, and a name without an
        // ordinal would silently make this pod its own leader — two leaders, two maps.
        //
        // RUSTIC_GIT_LEADER overrides that derivation for a leader in its own StatefulSet, and it
        // decides the one thing that must never be decided twice: who opens the map as WRITER.
        // Read here rather than passed down, so the writer decision and App's routing decision
        // cannot drift apart.
        let leader = match std::env::var("RUSTIC_GIT_LEADER").ok().filter(|v| !v.is_empty()) {
            Some(l) => l,
            None => rustic_git_server::ownership::leader_of(&me)?,
        };
        let store = rustic_git_server::ownership::OwnershipStore::open(store.os.clone());
        if me == leader {
            store.promote().await?;
        }
        (me, secret, store, leader)
    };
    // A node name resolves to its peer listener through the StatefulSet's own identity: no
    // lookup, nothing that can be stale.
    let svc_for_addr = svc.clone();
    let addr_of: rustic_git_server::AddrOf = if svc.is_empty() {
        std::sync::Arc::new(move |_: &str| format!("127.0.0.1:{peer_port}"))
    } else {
        std::sync::Arc::new(move |n: &str| format!("{n}.{svc_for_addr}:{peer_port}"))
    };
    // Pod zero holds the map, not repositories, so the leader must know how many servers exist to
    // hand a repo to. Defaults to 1 (solo), where the leader serves because there is no one else.
    // Defaults to the leader's own prefix, which is the single-StatefulSet layout.
    let server_prefix = std::env::var("RUSTIC_GIT_SERVER_PREFIX")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            leader_for_app
                .rsplit_once('-')
                .map(|(p, _)| p.to_string())
                .unwrap_or_else(|| leader_for_app.clone())
        });
    // Required with a fleet: defaulting to 1 made the leader hand every repo to `srv-0`, silently,
    // on any pod whose env lost the variable. Solo mode has nobody else to hand a repo to, so 1.
    let replicas: u32 = match std::env::var("RUSTIC_GIT_REPLICAS").ok().filter(|v| !v.is_empty()) {
        Some(v) => v
            .parse()
            .ok()
            .filter(|n| *n >= 1)
            .ok_or_else(|| err("RUSTIC_GIT_REPLICAS must be a positive integer"))?,
        None if svc.is_empty() => 1,
        None => {
            return Err(err(
                "RUSTIC_GIT_REPLICAS is required with RUSTIC_GIT_PEER_SVC (the leader hands repos \
                 to rustic-git-srv-{0..N-1})",
            ))
        }
    };
    // The one thing a serving node still asks Mongo for: a repo's pre-existing pull requests, copied
    // into its own database on first touch. A failure here must NOT degrade to "nothing to migrate":
    // this node may own repos whose changes live only in Mongo, and recording them as migrated would
    // hide them for good. So it still serves git, but pull routes fail loudly until it is restarted
    // against a reachable directory.
    let dir = match std::env::var("RUSTIC_GIT_MONGO_URI").ok().filter(|s| !s.is_empty()) {
        Some(uri) => {
            match rustic_git_server::directory::Directory::connect(&uri, &env("RUSTIC_GIT_MONGO_DB", "kloudlite")).await {
                Ok(d) => rustic_git_server::pulls::Source::Directory(Arc::new(d)),
                Err(e) => {
                    tracing::warn!(error = %e, "directory unreachable, pull requests will not migrate");
                    rustic_git_server::pulls::Source::Unavailable
                }
            }
        }
        None => rustic_git_server::pulls::Source::Absent,
    };
    // Splitting the leader into its own StatefulSet breaks name derivation: a server called
    // rustic-git-1 cannot compute "rustic-git-leader-0" from its own name. So both halves of the
    // topology become configuration when set, and every pod MUST agree — two nodes disagreeing on
    // who the writer is opens the map twice and fences a live database.
    let app = Arc::new(
        App::new(store.clone(), Arc::new(ownership), me, addr_of, peer_secret, replicas)
            .with_directory(dir)
            .with_topology(leader_for_app, server_prefix),
    );
    let jobs = rustic_git_server::boot::build_jobs_state().await?;
    // Withdraw any draining mark left by a previous life of this pod: the name is stable across
    // restarts, so without this a node comes back permanently ineligible for new repos.
    if !svc.is_empty() {
        if let Err(e) = app.announce_draining(false).await {
            tracing::warn!(error = %e, "clearing the shutdown mark");
        }
    }
    store.pool.spawn_sweeper();
    // The lifecycle invariant, both directions: eviction releases the lease before it closes the
    // database, and the renewal task closes any database whose lease we have lost. Single node has
    // neither — nothing to release to, nothing that can take a lease away.
    if !svc.is_empty() {
        store.pool.set_release_hook(
            Arc::downgrade(&app) as std::sync::Weak<dyn rustic_git_server::pool::ReleaseHook>
        );
        spawn_lease_tasks(app.clone());
    }

    let l = listeners::bind(&peer_addr).await?;
    let key = host_key(&env("RUSTIC_GIT_HOST_KEY", "./.local/host_key"))?;
    tracing::info!(
        "http on {} ssh on {} — peers on {} and {}, up to {} warm databases",
        l.http.local_addr()?,
        l.ssh.local_addr()?,
        l.peer_http.local_addr()?,
        l.peer_stream.local_addr()?,
        store.pool.max_warm()
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
        tracing::info!("sigterm: releasing the pool");
        // Say so BEFORE releasing. Releasing empties this node, which is exactly what makes the
        // leader pick it for the next repo — so the announcement has to land first or the pod can
        // be handed work on its way out.
        if let Err(e) = app_for_term.announce_draining(true).await {
            tracing::warn!(error = %e, "announcing shutdown");
        }

        // A watchdog, because every step below has been observed to hang. Measured: the leader sat
        // through the whole 90s terminationGracePeriodSeconds and was SIGKILLed while every other
        // pod exited in 17s — and a SIGKILLed leader is a fleet-wide claim outage for the length of
        // the grace period, which is the single most expensive thing a roll can do here. Whatever
        // is stuck, the process leaves on time: the pool release below is what peers actually need,
        // and it is attempted first with its own bound.
        tokio::spawn(async {
            tokio::time::sleep(HARD_EXIT).await;
            tracing::error!("shutdown watchdog: exiting");
            // Exit 1, not 0: this path means shutdown hung and got cut short by the watchdog,
            // not that it finished cleanly. A 0 here made every hung-shutdown restart look like a
            // normal exit in the pod's exit-code history, hiding exactly the failure mode this
            // watchdog exists to catch.
            std::process::exit(1);
        });

        // Bounded: a release that cannot finish must not hold up the signal to drain. The lease
        // lapses on its own TTL if this does not land, which is the slower path but not a wrong one.
        match tokio::time::timeout(RELEASE_DEADLINE, pool_for_term.close()).await {
            Ok(()) => tracing::info!("sigterm: pool released"),
            Err(_) => tracing::warn!("sigterm: pool release timed out; draining anyway"),
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
    let http_srv = axum::serve(l.http, rustic_git_server::router::router(a2, jobs.clone()))
        .with_graceful_shutdown(wait(term_rx.clone()));
    let peer_srv = axum::serve(l.peer_http, rustic_git_server::router::peer_router(a3, jobs))
        .with_graceful_shutdown(wait(term_rx.clone()));
    // Both HTTP servers as ONE select arm: select! returns when its first arm resolves, and if
    // each server were its own arm the first to finish draining would end the select and
    // pool.close() would run under the other's in-flight requests. try_join waits for both.
    tokio::select! {
        r = async { tokio::try_join!(http_srv, peer_srv) } => { r?; }
        _ = deadline => { tracing::warn!("drain deadline reached; exiting with sockets still open"); }
        r = rustic_git_server::proxy::serve_peer_streams(a4, l.peer_stream) => { r?; }
        r = rustic_git_server::ssh::serve(app.clone(), l.ssh, key) => { r?; }
    }
    // ponytail: the SSH and peer-stream listeners stop on select! exit without draining; the
    // preStop delay is what makes that rare (the pod has left DNS before it stops). Add per-session
    // tracking if SSH sessions being cut on roll ever matters.
    // A second close() is a no-op after the SIGTERM path already ran it; it covers the non-signal
    // exits (a listener error) so those still flush. The ownership map closes with it: on the
    // leader its last writes are still inside the 10ms flush window.
    // Bounded like the SIGTERM path: with the leader down every release waits out its retries,
    // and an unbounded close here left only the watchdog's exit 1 to end the process.
    if tokio::time::timeout(RELEASE_DEADLINE, store.pool.close()).await.is_err() {
        tracing::warn!("final pool release timed out; exiting anyway");
    }
    if let Err(e) = app.ownership.close().await {
        tracing::error!(error = %e, "closing the ownership map");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    rustic_git_core::log::init();
    rustic_git_core::metrics::init();
    // See config::install_crypto_provider — it must happen before any TLS, and
    // `admin` subcommands reach object storage without going through open_store.
    rustic_git_server::config::install_crypto_provider();

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
