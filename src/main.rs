use rustic_git::config::{env, open_store};
use rustic_git::{store::Store, Result};
use std::sync::Arc;

// ponytail: no CryptoRng impl for OsRng is reachable through the rand_core
// version russh/ssh-key 0.7.0-rc.11 pin (0.10.1, which has no OsRng at all);
// shell out to ssh-keygen (present on any host running sshd) instead of
// pulling in a duplicate rand_core dependency just for key generation.
fn host_key(path: &str) -> Result<russh::keys::PrivateKey> {
    let p = std::path::Path::new(path);
    if !p.exists() {
        if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?; // ssh-keygen will not create it
        }
        let status = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(p)
            .status()?;
        if !status.success() {
            return Err(rustic_git::err("ssh-keygen failed to generate host key"));
        }
    }
    Ok(russh::keys::PrivateKey::read_openssh_file(p)?)
}

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

    let peer_addr = env("RUSTIC_GIT_PEER_ADDR", "0.0.0.0:8081");
    let peer_port: u16 = peer_addr
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| rustic_git::err("RUSTIC_GIT_PEER_ADDR must be host:port"))?;
    // Multi-node when a peer Service is configured, single node otherwise. Single node needs no
    // ownership map at all: with one node there is nothing to coordinate, so it claims everything
    // from an empty in-process map and never touches the ownership database.
    let svc = std::env::var("RUSTIC_GIT_PEER_SVC").unwrap_or_default();
    let (me, peer_secret, ownership, leader_for_app) = if svc.is_empty() {
        // Random secret so nothing on the network can drive the peer port.
        use rand::RngCore;
        let mut b = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut b);
        let secret: String = b.iter().map(|x| format!("{x:02x}")).collect();
        (
            "rustic-git-0".to_string(),
            secret,
            rustic_git::ownership::OwnershipStore::Solo,
            "rustic-git-0".to_string(),
        )
    } else {
        let need = |k: &str| {
            std::env::var(k)
                .ok()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| rustic_git::err(format!("{k} is required with RUSTIC_GIT_PEER_SVC")))
        };
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
            None => rustic_git::ownership::leader_of(&me)?,
        };
        let store =
            rustic_git::ownership::OwnershipStore::open(store.os.clone(), me == leader).await?;
        (me, secret, store, leader)
    };
    // A node name resolves to its peer listener through the StatefulSet's own identity: no
    // lookup, nothing that can be stale.
    let svc_for_addr = svc.clone();
    let addr_of: rustic_git::AddrOf = if svc.is_empty() {
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
            .ok_or_else(|| rustic_git::err("RUSTIC_GIT_REPLICAS must be a positive integer"))?,
        None if svc.is_empty() => 1,
        None => {
            return Err(rustic_git::err(
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
            match rustic_git::directory::Directory::connect(&uri, &env("RUSTIC_GIT_MONGO_DB", "kloudlite")).await {
                Ok(d) => rustic_git::pulls::Source::Directory(Arc::new(d)),
                Err(e) => {
                    eprintln!("directory unreachable, pull requests will not migrate: {e}"); // ponytail: eprintln
                    rustic_git::pulls::Source::Unavailable
                }
            }
        }
        None => rustic_git::pulls::Source::Absent,
    };
    // Splitting the leader into its own StatefulSet breaks name derivation: a server called
    // rustic-git-1 cannot compute "rustic-git-leader-0" from its own name. So both halves of the
    // topology become configuration when set, and every pod MUST agree — two nodes disagreeing on
    // who the writer is opens the map twice and fences a live database.
    let app = Arc::new(
        rustic_git::App::new(
            store.clone(),
            Arc::new(ownership),
            me,
            addr_of,
            peer_secret,
            replicas,
        )
        .with_directory(dir)
        .with_topology(leader_for_app, server_prefix),
    );
    // Withdraw any draining mark left by a previous life of this pod: the name is stable across
    // restarts, so without this a node comes back permanently ineligible for new repos.
    if !svc.is_empty() {
        if let Err(e) = app.announce_draining(false).await {
            eprintln!("clearing the shutdown mark: {e}"); // ponytail: eprintln
        }
    }
    store.pool.spawn_sweeper();
    // The lifecycle invariant, both directions: eviction releases the lease before it closes the
    // database, and the renewal task closes any database whose lease we have lost. Single node has
    // neither — nothing to release to, nothing that can take a lease away.
    if !svc.is_empty() {
        store.pool.set_release_hook(
            Arc::downgrade(&app) as std::sync::Weak<dyn rustic_git::pool::ReleaseHook>
        );
        spawn_lease_tasks(app.clone());
    }

    let http = tokio::net::TcpListener::bind(env("RUSTIC_GIT_HTTP_ADDR", "0.0.0.0:8080")).await?;
    let ssh = tokio::net::TcpListener::bind(env("RUSTIC_GIT_SSH_ADDR", "0.0.0.0:2222")).await?;
    let peer_http = tokio::net::TcpListener::bind(&peer_addr).await?;
    let peer_stream =
        tokio::net::TcpListener::bind(rustic_git::proxy::stream_addr(&peer_addr)).await?;
    let key = host_key(&env("RUSTIC_GIT_HOST_KEY", "./.local/host_key"))?;
    eprintln!(
        "http on {} ssh on {} — peers on {} and {}, up to {} warm databases",
        http.local_addr()?,
        ssh.local_addr()?,
        peer_http.local_addr()?,
        peer_stream.local_addr()?,
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
        eprintln!("sigterm: releasing the pool"); // ponytail: eprintln
        // Say so BEFORE releasing. Releasing empties this node, which is exactly what makes the
        // leader pick it for the next repo — so the announcement has to land first or the pod can
        // be handed work on its way out.
        if let Err(e) = app_for_term.announce_draining(true).await {
            eprintln!("announcing shutdown: {e}"); // ponytail: eprintln
        }

        // A watchdog, because every step below has been observed to hang. Measured: the leader sat
        // through the whole 90s terminationGracePeriodSeconds and was SIGKILLed while every other
        // pod exited in 17s — and a SIGKILLed leader is a fleet-wide claim outage for the length of
        // the grace period, which is the single most expensive thing a roll can do here. Whatever
        // is stuck, the process leaves on time: the pool release below is what peers actually need,
        // and it is attempted first with its own bound.
        tokio::spawn(async {
            tokio::time::sleep(HARD_EXIT).await;
            eprintln!("shutdown watchdog: exiting"); // ponytail: eprintln
            // Exit 1, not 0: this path means shutdown hung and got cut short by the watchdog,
            // not that it finished cleanly. A 0 here made every hung-shutdown restart look like a
            // normal exit in the pod's exit-code history, hiding exactly the failure mode this
            // watchdog exists to catch.
            std::process::exit(1);
        });

        // Bounded: a release that cannot finish must not hold up the signal to drain. The lease
        // lapses on its own TTL if this does not land, which is the slower path but not a wrong one.
        match tokio::time::timeout(RELEASE_DEADLINE, pool_for_term.close()).await {
            Ok(()) => eprintln!("sigterm: pool released"), // ponytail: eprintln
            Err(_) => eprintln!("sigterm: pool release timed out; draining anyway"), // ponytail: eprintln
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
    let http_srv = axum::serve(http, rustic_git::http::router(a2))
        .with_graceful_shutdown(wait(term_rx.clone()));
    let peer_srv = axum::serve(peer_http, rustic_git::http::peer_router(a3))
        .with_graceful_shutdown(wait(term_rx.clone()));
    // Both HTTP servers as ONE select arm: select! returns when its first arm resolves, and if
    // each server were its own arm the first to finish draining would end the select and
    // pool.close() would run under the other's in-flight requests. try_join waits for both.
    tokio::select! {
        r = async { tokio::try_join!(http_srv, peer_srv) } => { r?; }
        _ = deadline => { eprintln!("drain deadline reached; exiting with sockets still open"); } // ponytail: eprintln
        r = rustic_git::proxy::serve_peer_streams(a4, peer_stream) => { r?; }
        r = rustic_git::ssh::serve(app.clone(), ssh, key) => { r?; }
    }
    // ponytail: the SSH and peer-stream listeners stop on select! exit without draining; the
    // preStop delay is what makes that rare (the pod has left DNS before it stops). Add per-session
    // tracking if SSH sessions being cut on roll ever matters.
    // A second close() is a no-op after the SIGTERM path already ran it; it covers the non-signal
    // exits (a listener error) so those still flush. The ownership map closes with it: on the
    // leader its last writes are still inside the 10ms flush window.
    store.pool.close().await;
    if let Err(e) = app.ownership.close().await {
        eprintln!("closing the ownership map: {e}"); // ponytail: eprintln
    }
    Ok(())
}

/// The read API process: no repository state, no ownership, no local packs — object-store
/// credentials for token lookups, the peer secret, and Redis.
/// Renewal, and pruning on the leader — the two background halves of the lifecycle invariant.
/// The work itself lives on `App`; these are only the clocks.
fn spawn_lease_tasks(app: Arc<rustic_git::App>) {
    use rustic_git::ownership::{LEASE_TTL, RENEW_EVERY};
    /// How often the leader moves the ownership map's flush pointer. Matched to the collector's
    /// `min_age` so the WAL settles at about two of these rather than growing without bound.
    const CHECKPOINT_EVERY: std::time::Duration = std::time::Duration::from_secs(300);
    /// Ceiling on one checkpoint. Generous for the work (a healthy one takes ~14ms) and short
    /// against the lease TTL it must never eat into.
    const CHECKPOINT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
    let a = app.clone();
    tokio::spawn(async move {
        let mut beat = 0u64;
        let mut last_checkpoint = std::time::Instant::now();
        loop {
            tokio::time::sleep(RENEW_EVERY).await;
            // A renewal that cannot reach the leader is not fatal: the lease runs to its TTL and
            // the next beat is three seconds away. Missing every beat for a whole TTL is what lets
            // another node claim, which is the intended outcome.
            if let Err(e) = a.renew_once().await {
                eprintln!("renewing leases: {e}"); // ponytail: eprintln
            }
            // Low-frequency reconcile lane: heals any warm repo whose visibility marker drifted
            // from its DB (a crashed flip, or a repo that predates markers entirely) even if
            // nobody touches it again to trigger the lazy repair in `open_repo`. Every tenth beat
            // at `RENEW_EVERY` = 3s puts the drift ceiling at 30 seconds plus 200ms per owned
            // repo: a crashed flip, or a fail-closed marker the structural sweep invented, is
            // corrected within ~30 seconds of the node holding that repo.
            beat += 1;
            if beat.is_multiple_of(10) {
                a.reconcile_owned_markers().await;
            }
            // The merge worker's safety floor, and it lives here now: a repo's changes are in its
            // own database, so this node is the ONLY one allowed to go looking for a mergeability
            // check that is due. Every twentieth beat at `RENEW_EVERY` = 3s puts the drift ceiling
            // at 60 seconds plus 200ms per owned repo — the same 60s the worker's Mongo sweep gave
            // before this moved, so nothing anyone is watching gets slower, and it now holds with
            // Redis and Mongo both down. The stream nudge (the routed `pulls/{n}/check`) is what
            // makes the common case sub-second; this is what makes it never lost.
            if beat.is_multiple_of(20) {
                a.check_owned_pulls().await;
            }
            // Merges the owner was asked for. Every fifth beat = 15 seconds, tighter than the
            // check lane because someone clicked and is watching: a merge nobody nudged lands
            // within ~15 seconds plus 200ms per owned repo, with Redis and Mongo both down. The
            // merge itself already ran on this node — only the claim and the outcome moved here.
            if beat.is_multiple_of(5) {
                a.merge_owned_pulls().await;
            }
            // Move the ownership map's flush pointer so the WAL behind it can be reclaimed, every
            // five minutes to match the collector's `min_age` — which puts steady state at roughly
            // ten minutes of WAL instead of forever. Only the leader has a memtable to flush; on a
            // follower this is a no-op. Log-and-continue: a missed checkpoint costs a few hundred
            // objects that the next one makes collectable, and it must never take down the lease
            // renewal it rides on.
            //
            // Timed off the CLOCK rather than a beat count, unlike the lanes above. A beat is only
            // three seconds when the loop does nothing else, and by here it has reconciled markers,
            // swept for mergeability work and run a merge — so counting beats drifted to well over
            // the five minutes it claimed. The lanes above tolerate drift because they are
            // backstops; this one bounds an unbounded resource, so it gets a real deadline.
            if last_checkpoint.elapsed() >= CHECKPOINT_EVERY {
                last_checkpoint = std::time::Instant::now();
                // BOUNDED, because this shares a task with lease renewal. An unbounded flush hung
                // here once and the leader stopped renewing leases entirely — the map's own
                // housekeeping took out the thing the map exists for. Whatever a maintenance step
                // does, it gets a deadline; missing a checkpoint costs a few hundred reclaimable
                // objects, missing every renewal costs the fleet its routing.
                match tokio::time::timeout(CHECKPOINT_TIMEOUT, a.ownership.checkpoint()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => eprintln!("ownership checkpoint: {e}"), // ponytail: eprintln
                    Err(_) => eprintln!(
                        "ownership checkpoint: timed out after {}s; leases keep renewing", // ponytail: eprintln
                        CHECKPOINT_TIMEOUT.as_secs()
                    ),
                }
            }
        }
    });
    if !app.is_leader() {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(LEASE_TTL).await;
            if let Err(e) = app.prune_once().await {
                eprintln!("pruning ownership: {e}"); // ponytail: eprintln
            }
        }
    });
}

/// Same either-variable "is a fleet configured" test as `set-visibility`/`set-image-visibility`
/// (keying on the secret alone would let an operator whose shell doesn't export it take the
/// direct path against a live fleet). These four commands open a repo's SlateDB from a bare
/// process with zero ownership coordination, and unlike `set-visibility` and
/// `set-image-visibility` there is no routed `/api` endpoint to deliver a fork/repack/delete/create
/// to the owning node, so a configured fleet means refuse rather
/// than open the database here and fence whatever node is currently serving it. Only with
/// nothing configured (single node, or an offline run) does it proceed, saying out loud what
/// it is assuming.
fn fleet_guard(cmd: &str, path: &str, upstream: Option<String>, secret: Option<String>) -> Result<()> {
    if upstream.is_some() || secret.is_some() {
        return Err(rustic_git::err(format!(
            "{cmd}: a fleet is configured (RUSTIC_GIT_UPSTREAM or RUSTIC_GIT_PEER_SECRET set) but \
             there is no routed endpoint to deliver this to the node serving {path} — refusing to \
             run it here. Run this only when no node is currently serving that repo."
        )));
    }
    eprintln!(
        "{cmd}: no RUSTIC_GIT_UPSTREAM or RUSTIC_GIT_PEER_SECRET set — running against {path} \
         directly, assuming NO node is currently serving it. If one is, opening its database here \
         fences the serving node's writer."
    ); // ponytail: eprintln
    Ok(())
}

/// Writes a listing marker for every row, returning how many landed and naming the ones that did
/// not. A marker is a VIEW, so Mongo's `public` mirror is a fine source for it — and any row that
/// fails here self-heals the next time the owning node touches the repo (`reconcile_marker`).
///
/// It never deletes: a marker whose repo has no row is left alone, because unlisting is the GC
/// sweep's job and that sweep is keep-biased on purpose. Reporting partial failure beats aborting
/// halfway and saying nothing, so a failed row prints and the loop continues.
async fn backfill_repo_markers(store: &Arc<Store>, rows: &[rustic_git::directory::Repo]) -> usize {
    use rustic_git::index::{self, Kind, Marker};
    let mut written = 0;
    for r in rows {
        let m = Marker {
            name: r.name.clone(),
            public: r.public,
            created_by: r.created_by.clone(),
            created_ms: r.created_at.timestamp_millis(),
            description: r.description.clone(),
            // Image-only fields; a code repo has neither.
            manifests: 0,
            updated_ms: 0,
        };
        // The flip-safe writer, not a raw put: a pre-existing marker on the opposite side would
        // otherwise survive beside the new one and `list` would read the pair as private.
        match index::write(&store.os, Kind::Repo, &r.owner, &m).await {
            Ok(()) => written += 1,
            Err(e) => eprintln!("backfill-repo-markers: {}/{}: {e}", r.owner, r.name), // ponytail: eprintln
        }
    }
    written
}

/// Deliver a flip to the node that owns `path`'s database: POST it to the peer Service and let
/// the `route` middleware carry it. Carries the owner as the peer identity because
/// `imagevisibility` authorizes on it (the repo route ignores it). A peer that accepts and never
/// answers must not hang the command forever, so the call is bounded like the api's upstream calls.
async fn post_to_owner(
    cmd: &str,
    owner: &str,
    route: &str,
    upstream: Option<String>,
    secret: Option<String>,
) -> Result<()> {
    let upstream = upstream.unwrap_or_else(|| "http://rustic-git:8081".into());
    let res = reqwest::Client::builder()
        .timeout(rustic_git::api::UPSTREAM_TIMEOUT)
        .build()?
        .post(format!("{}{route}", upstream.trim_end_matches('/')))
        .header(rustic_git::proxy::PEER_HEADER, secret.unwrap_or_default())
        .header(rustic_git::proxy::OWNER_HEADER, owner)
        .send()
        .await
        .map_err(|e| rustic_git::err(format!("{cmd}: {e}")))?;
    let status = res.status();
    if status.is_success() {
        return Ok(());
    }
    let body = rustic_git::api::read_bounded(res)
        .await
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    Err(rustic_git::err(format!("{cmd}: {status}: {body}")))
}

async fn run(a: &[&str], store: &Arc<Store>) -> Result<()> {
    match a {
        ["admin", "fork", src, dst] => {
            let (so, sn) = src.split_once('/').ok_or("owner/name")?;
            let (o, n) = dst.split_once('/').ok_or("owner/name")?;
            fleet_guard(
                "admin fork",
                dst,
                std::env::var("RUSTIC_GIT_UPSTREAM").ok(),
                std::env::var("RUSTIC_GIT_PEER_SECRET").ok(),
            )?;
            let src = store.open_repo(so, sn).await?.ok_or("source repository not found")?;
            store.fork(&src, o, n).await
        }
        ["admin", "repack", path] => {
            let (o, n) = path.split_once('/').ok_or("owner/name")?;
            fleet_guard(
                "admin repack",
                path,
                std::env::var("RUSTIC_GIT_UPSTREAM").ok(),
                std::env::var("RUSTIC_GIT_PEER_SECRET").ok(),
            )?;
            let (before, after) = store.repack(o, n).await?;
            println!("repacked {path}: {before} packs -> {after}");
            Ok(())
        }
        ["admin", "delete-repo", path] => {
            let (o, n) = path.split_once('/').ok_or("owner/name")?;
            fleet_guard(
                "admin delete-repo",
                path,
                std::env::var("RUSTIC_GIT_UPSTREAM").ok(),
                std::env::var("RUSTIC_GIT_PEER_SECRET").ok(),
            )?;
            store.delete_repo(o, n).await
        }
        // Clean up after a repo that was deleted BEFORE delete removed the database files: the
        // directory survives, so the GC sweep reads it as an existing repo that merely lost its
        // marker and recreates one, and the repo reappears in every listing. Refuses to touch a
        // repo that still exists, so it can only ever remove what is already gone.
        ["admin", "purge-ghost-repo", path] => {
            let (o, n) = path.split_once('/').ok_or("owner/name")?;
            fleet_guard(
                "admin purge-ghost-repo",
                path,
                std::env::var("RUSTIC_GIT_UPSTREAM").ok(),
                std::env::var("RUSTIC_GIT_PEER_SECRET").ok(),
            )?;
            if store.repo_exists(o, n).await? {
                return Err(rustic_git::err(format!(
                    "{path} still exists — purge only removes the remains of a deleted repo"
                )));
            }
            let _ = rustic_git::index::remove(&store.os, rustic_git::index::Kind::Repo, o, n).await;
            store.delete_repo_db(o, n).await?;
            println!("purged the remains of {path}");
            Ok(())
        }
        // Diagnostic for the ownership map's WAL. Prints the one number that decides whether the
        // WAL can be reclaimed at all -- `replay_after_wal_id`, the point the memtable has been
        // flushed to -- then runs one collection synchronously so a failure is reported instead of
        // disappearing into a background task that logs nothing. An explicit `min_age` in seconds
        // may be given to drain a backlog that predates the fix; it defaults to the leader's own.
        //
        // Reads and deletes object-store keys only. It never opens the ownership database, so it
        // cannot fence the leader that has it open.
        ["admin", "ownership-gc", rest @ ..] => {
            let min_age = rest.first().and_then(|s| s.parse::<u64>().ok()).unwrap_or(300);
            let wal = format!("{}/wal", rustic_git::ownership::PATH);
            let count = |os: std::sync::Arc<dyn slatedb::object_store::ObjectStore>, p: String| async move {
                use futures::StreamExt;
                os.list(Some(&slatedb::object_store::path::Path::from(p)))
                    .filter_map(|r| async move { r.ok() })
                    .count()
                    .await
            };

            let admin = slatedb::admin::AdminBuilder::new(
                rustic_git::ownership::PATH,
                store.os.clone(),
            )
            .build();
            match admin.read_manifest(None).await {
                Ok(Some(m)) => {
                    let v = serde_json::to_value(&m).unwrap_or_default();
                    // Flattened, so the fields sit at the top level. Printed on their own rather
                    // than dumping the manifest: it carries every L0 entry and is unreadable.
                    let f = |k: &str| {
                        v.pointer(&format!("/{k}")).map(|x| x.to_string()).unwrap_or("?".into())
                    };
                    println!("replay_after_wal_id = {}", f("replay_after_wal_id"));
                    println!("next_wal_sst_id     = {}", f("next_wal_sst_id"));
                    println!("last_l0_clock_tick  = {}", f("last_l0_clock_tick"));
                    println!("writer_epoch        = {}", f("writer_epoch"));
                }
                Ok(None) => println!("no manifest — the map has never been written"),
                Err(e) => println!("reading the manifest failed: {e}"),
            }

            let before = count(store.os.clone(), wal.clone()).await;
            println!("wal objects before = {before}");
            let opts = slatedb::config::GarbageCollectorOptions {
                wal_options: Some(slatedb::config::GarbageCollectorDirectoryOptions {
                    interval: None,
                    min_age: std::time::Duration::from_secs(min_age),
                    dry_run: false,
                }),
                ..Default::default()
            };
            match admin.run_gc_once(opts).await {
                Ok(()) => println!("collection ran"),
                Err(e) => println!("collection FAILED: {e}"),
            }
            println!("wal objects after  = {}", count(store.os.clone(), wal).await);
            Ok(())
        }
        ["admin", "create-repo", path] => {
            let (o, n) = path.split_once('/').ok_or("owner/name")?;
            fleet_guard(
                "admin create-repo",
                path,
                std::env::var("RUSTIC_GIT_UPSTREAM").ok(),
                std::env::var("RUSTIC_GIT_PEER_SECRET").ok(),
            )?;
            store.create_repo(o, n).await
        }
        ["admin", "backfill-repo-markers"] => {
            // No fleet guard and no `db_for`: this writes object-store keys only, so it opens no
            // repo database and cannot fence a serving node. Mongo is reached the same way `serve`
            // and the api binary reach it — no other admin subcommand needs a directory handle.
            let uri = std::env::var("RUSTIC_GIT_MONGO_URI").map_err(|_| {
                rustic_git::err("RUSTIC_GIT_MONGO_URI required: the repo rows are the source")
            })?;
            let db = env("RUSTIC_GIT_MONGO_DB", "kloudlite");
            let dir = rustic_git::directory::Directory::connect(&uri, &db).await?;
            let rows = dir.all_repos().await?;
            let written = backfill_repo_markers(store, &rows).await;
            println!("backfilled {written}/{} repo markers", rows.len());
            Ok(())
        }
        ["admin", "add-token", owner] => {
            // Same rule the api tier applies: a credential for an owner no URL can name is a
            // credential nothing can use, and a reserved name (`api`, `v2`) would be worse.
            if !rustic_git::store::valid_owner(owner) {
                return Err(rustic_git::err(format!("{owner}: not a valid owner name")));
            }
            println!("{}", store.create_token(owner).await?);
            Ok(())
        }
        ["admin", "add-key", owner, file] => {
            if !rustic_git::store::valid_owner(owner) {
                return Err(rustic_git::err(format!("{owner}: not a valid owner name")));
            }
            store.add_ssh_key(owner, &std::fs::read_to_string(file)?).await
        }
        ["admin", "purge-cache", path] => {
            let (o, n) = path.split_once('/').ok_or("owner/name")?;
            store.cache.bump_generation(&format!("{o}/{n}")).await
        }
        ["admin", "set-visibility", path, vis] => {
            let (o, n) = path.split_once('/').ok_or("owner/name")?;
            if !matches!(*vis, "public" | "private") {
                return Err(rustic_git::err("visibility must be public or private"));
            }
            // The flip changes LIVE authorization, so it must happen on the handle that serves the
            // repo. Writing it here would open the repo's database as a second process while the
            // owning node keeps answering from its own view — measured at ~4s of a private repo
            // still being served as public. With a fleet configured, post it to the peer Service
            // and let the `route` middleware deliver it to the owner.
            //
            // "Configured" is EITHER variable: keying on the secret alone would make an operator
            // whose shell happens not to export it take the direct path silently, reintroducing
            // exactly that window. Neither set is still a guess — this process cannot see whether a
            // node is serving the repo — so the direct path says out loud what it is assuming.
            let upstream = std::env::var("RUSTIC_GIT_UPSTREAM").ok();
            let secret = std::env::var("RUSTIC_GIT_PEER_SECRET").ok();
            if upstream.is_none() && secret.is_none() {
                eprintln!(
                    "set-visibility: no RUSTIC_GIT_UPSTREAM or RUSTIC_GIT_PEER_SECRET set — \
                     writing {path} directly, assuming NO node is currently serving it. If one is, \
                     it keeps authorizing from its own view for several seconds; set both and \
                     re-run to route the flip through the owner."
                ); // ponytail: eprintln
                return store.set_public(o, n, *vis == "public").await;
            }
            post_to_owner("set-visibility", o, &format!("/api/{o}/{n}/visibility?visibility={vis}"), upstream, secret).await
        }
        ["admin", "set-image-visibility", path, vis] => {
            let (o, n) = path.split_once('/').ok_or("owner/image")?;
            if !matches!(*vis, "public" | "private") {
                return Err(rustic_git::err("visibility must be public or private"));
            }
            // Mirrors `set-visibility` exactly: `imagevisibility` is a routed browse endpoint
            // (by the IMAGE key), so with a fleet configured the flip is delivered to the node
            // that owns the image's database rather than written here under a live writer.
            // Same either-variable test for "configured", for the same reason.
            let upstream = std::env::var("RUSTIC_GIT_UPSTREAM").ok();
            let secret = std::env::var("RUSTIC_GIT_PEER_SECRET").ok();
            if upstream.is_none() && secret.is_none() {
                eprintln!(
                    "set-image-visibility: no RUSTIC_GIT_UPSTREAM or RUSTIC_GIT_PEER_SECRET set — \
                     writing {path} directly, assuming NO node is currently serving it. If one is, it \
                     keeps answering from its own view for several seconds."
                ); // ponytail: eprintln
                return store.set_image_visibility(o, n, *vis == "public").await;
            }
            post_to_owner(
                "set-image-visibility",
                o,
                &format!("/api/{o}/{n}/imagevisibility?visibility={vis}"),
                upstream,
                secret,
            )
            .await
        }
        _ => Err(rustic_git::err(
            "usage: rustic-git serve | admin create-repo <owner>/<name> | admin fork <src>/<name> <owner>/<name> | admin delete-repo <owner>/<name> | admin purge-ghost-repo <owner>/<name> | admin ownership-gc [min-age-secs] | admin repack <owner>/<name> | admin add-token <owner> | admin add-key <owner> <pubkey-file> | admin set-visibility <owner>/<name> public|private | admin set-image-visibility <owner>/<image> public|private | admin purge-cache <owner>/<name> | admin backfill-repo-markers",
        )),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // See config::install_crypto_provider — it must happen before any TLS, and
    // `admin` subcommands reach object storage without going through open_store.
    rustic_git::config::install_crypto_provider();

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
    let store = open_store(false).await?;
    let r = run(&a, &store).await;
    store.pool.close().await;
    if let Err(e) = r {
        eprintln!("{e}");
        std::process::exit(2);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{fleet_guard, run};

    #[test]
    fn fleet_guard_refuses_when_either_var_is_set() {
        assert!(fleet_guard("admin repack", "alice/web", None, None).is_ok());
        assert!(fleet_guard("admin repack", "alice/web", Some("http://x".into()), None).is_err());
        assert!(fleet_guard("admin repack", "alice/web", None, Some("secret".into())).is_err());
        assert!(fleet_guard(
            "admin repack",
            "alice/web",
            Some("http://x".into()),
            Some("secret".into())
        )
        .is_err());
    }

    // `set_visibility_routes_unless_nothing_is_configured` and `set_image_visibility_writes_it`
    // both mutate the process-wide RUSTIC_GIT_UPSTREAM/RUSTIC_GIT_PEER_SECRET env vars; without
    // this they race each other across threads.
    // An async mutex, not a std one: both tests await while holding it, and a std guard held
    // across `.await` can park the whole runtime thread on a lock another task must release.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    pub(crate) async fn store() -> std::sync::Arc<rustic_git::store::Store> {
        // Leaked so the store outlives the temp dir without a struct to hold both.
        let tmp = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        std::sync::Arc::new(
            rustic_git::store::Store::open(
                std::sync::Arc::new(slatedb::object_store::memory::InMemory::new()),
                tmp.path().join("cache"),
                false,
            )
            .await
            .unwrap(),
        )
    }

    /// Both halves of the fleet-vs-direct choice, in ONE test: it mutates process-wide env vars,
    /// and a second test doing the same would race it.
    ///
    /// Catches: (1) the fallback being lost when the flip moved onto the peer endpoint; (2) the
    /// branch keying on the SECRET alone — an operator whose shell does not export it, but who has
    /// an upstream configured, would silently write directly against a live fleet, which is the
    /// stale-authorization window this change exists to close.
    #[tokio::test]
    async fn set_visibility_routes_unless_nothing_is_configured() {
        let _guard = ENV_LOCK.lock().await;
        let store = store().await;
        run(&["admin", "create-repo", "alice/web"], &store).await.unwrap();

        // Nothing configured: a single node or an offline run. Writes directly (with a warning).
        std::env::remove_var("RUSTIC_GIT_PEER_SECRET");
        std::env::remove_var("RUSTIC_GIT_UPSTREAM");
        run(&["admin", "set-visibility", "alice/web", "public"], &store).await.unwrap();
        assert!(store.is_public("alice", "web").await.unwrap());

        // An upstream configured but no secret in this shell: must still go to the fleet, and fail
        // loudly when it cannot reach it — never write here.
        std::env::set_var("RUSTIC_GIT_UPSTREAM", "http://127.0.0.1:1");
        let e = run(&["admin", "set-visibility", "alice/web", "private"], &store)
            .await
            .expect_err("an unreachable fleet must fail, not fall back to a direct write");
        assert!(store.is_public("alice", "web").await.unwrap(), "nothing written here: {e}");
        std::env::remove_var("RUSTIC_GIT_UPSTREAM");
        store.pool.close().await;
    }

    /// `set_image_visibility` had zero non-test callers before this command existed, which made
    /// every image private forever. This is the CLI's only path to it.
    ///
    /// Also covers the fleet-vs-direct guard, in ONE test since it mutates process-wide env vars
    /// and a second test doing the same would race it. It mirrors `set-visibility` exactly: a
    /// configured fleet means the flip is posted to the routed `imagevisibility` endpoint, so this
    /// catches the guard writing here anyway when only one of the two vars is set.
    #[tokio::test]
    async fn set_image_visibility_writes_it() {
        let _guard = ENV_LOCK.lock().await;
        std::env::remove_var("RUSTIC_GIT_PEER_SECRET");
        std::env::remove_var("RUSTIC_GIT_UPSTREAM");
        let store = store().await;
        use rustic_git::index::{self, Kind};
        use slatedb::object_store::ObjectStoreExt;
        let pub_path = index::path(true, Kind::Img, "acme", "nginx");
        let priv_path = index::path(false, Kind::Img, "acme", "nginx");

        assert!(!store.image_is_public("acme", "nginx").await.unwrap());
        run(&["admin", "set-image-visibility", "acme/nginx", "public"], &store).await.unwrap();
        assert!(store.image_is_public("acme", "nginx").await.unwrap());
        assert!(store.os.get(&pub_path).await.is_ok(), "public marker missing after flip");
        assert!(store.os.get(&priv_path).await.is_err(), "private marker left behind after flip");

        run(&["admin", "set-image-visibility", "acme/nginx", "private"], &store).await.unwrap();
        assert!(!store.image_is_public("acme", "nginx").await.unwrap());
        assert!(store.os.get(&priv_path).await.is_ok(), "private marker missing after flip");
        assert!(store.os.get(&pub_path).await.is_err(), "public marker left behind after flip");
        let e = run(&["admin", "set-image-visibility", "acme/nginx", "sideways"], &store)
            .await
            .expect_err("only public|private are valid");
        assert!(e.to_string().contains("public or private"), "{e}");

        // An upstream configured but no secret in this shell: must go to the fleet (the routed
        // `imagevisibility` endpoint) and fail loudly when it cannot reach it — never write here.
        std::env::set_var("RUSTIC_GIT_UPSTREAM", "http://127.0.0.1:1");
        let e = run(&["admin", "set-image-visibility", "acme/nginx", "public"], &store)
            .await
            .expect_err("an unreachable fleet must fail, not fall back to a direct write");
        assert!(!store.image_is_public("acme", "nginx").await.unwrap(), "nothing written here: {e}");
        assert!(e.to_string().contains("set-image-visibility"), "{e}");
        assert!(!e.to_string().contains("no routed endpoint"), "{e}");
        std::env::remove_var("RUSTIC_GIT_UPSTREAM");

        store.pool.close().await;
    }

    #[tokio::test]
    async fn admin_credentials_refuse_an_invalid_owner() {
        let store = store().await;
        assert!(run(&["admin", "add-token", "api"], &store).await.is_err());
        assert!(run(&["admin", "add-token", "no/slash"], &store).await.is_err());
        assert!(run(&["admin", "add-token", "alice"], &store).await.is_ok());
        store.pool.close().await;
    }
}

#[cfg(test)]
mod backfill_tests {
    use super::{backfill_repo_markers, tests::store};
    use rustic_git::directory::Repo;
    use rustic_git::index::{self, Kind};
    use mongodb::bson::DateTime;
    use slatedb::object_store::ObjectStoreExt;

    fn row(name: &str, public: bool) -> Repo {
        Repo {
            id: format!("alice/{name}"),
            owner: "alice".into(),
            name: name.into(),
            public,
            description: format!("the {name} repo"),
            created_by: "alice".into(),
            created_at: DateTime::from_millis(1_700_000_000_000),
        }
    }

    /// The cutover case in one test: visibility lands on the right path, a second run is a no-op,
    /// the row's own fields reach the body, and a marker with no row is left ALONE (removing one
    /// is the GC sweep's job, and it is keep-biased).
    #[tokio::test]
    async fn backfill_writes_markers_without_deleting_strangers() {
        let store = store().await;
        let orphan = index::path(true, Kind::Repo, "alice", "gone");
        index::write(
            &store.os,
            Kind::Repo,
            "alice",
            &index::Marker {
                name: "gone".into(),
                public: true,
                created_by: "alice".into(),
                created_ms: 1,
                description: String::new(),
                manifests: 0,
                updated_ms: 0,
            },
        )
        .await
        .unwrap();

        let rows = vec![row("web", true), row("secret", false)];
        assert_eq!(backfill_repo_markers(&store, &rows).await, 2);

        assert!(store.os.get(&index::path(true, Kind::Repo, "alice", "web")).await.is_ok());
        assert!(store.os.get(&index::path(false, Kind::Repo, "alice", "web")).await.is_err());
        assert!(store.os.get(&index::path(false, Kind::Repo, "alice", "secret")).await.is_ok());
        assert!(store.os.get(&index::path(true, Kind::Repo, "alice", "secret")).await.is_err());
        assert!(store.os.get(&orphan).await.is_ok(), "a repo with no row must not be unlisted");

        let m = index::read(&store.os, Kind::Repo, "alice", "web").await.unwrap();
        assert_eq!(m.description, "the web repo");
        assert_eq!(m.created_by, "alice");
        assert_eq!(m.created_ms, 1_700_000_000_000);
        assert_eq!(m.manifests, 0, "manifests are image-only");

        // Re-runnable: it overwrites, never appends, so a second pass changes nothing.
        assert_eq!(backfill_repo_markers(&store, &rows).await, 2);
        assert_eq!(index::read(&store.os, Kind::Repo, "alice", "web").await.unwrap(), m);
        assert!(store.os.get(&orphan).await.is_ok());
        store.pool.close().await;
    }
}
