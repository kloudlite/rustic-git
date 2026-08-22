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
    let (me, peer_secret, ownership) = if svc.is_empty() {
        // Random secret so nothing on the network can drive the peer port.
        use rand::RngCore;
        let mut b = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut b);
        let secret: String = b.iter().map(|x| format!("{x:02x}")).collect();
        (
            "rustic-git-0".to_string(),
            secret,
            rustic_git::ownership::OwnershipStore::Solo,
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
        let leader = rustic_git::ownership::leader_of(&me)?;
        let store =
            rustic_git::ownership::OwnershipStore::open(store.os.clone(), me == leader).await?;
        (me, secret, store)
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
    let replicas: u32 = std::env::var("RUSTIC_GIT_REPLICAS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let app = Arc::new(rustic_git::App::new(
        store.clone(),
        Arc::new(ownership),
        me,
        addr_of,
        peer_secret,
        replicas,
    ));
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
    let a = app.clone();
    tokio::spawn(async move {
        let mut beat = 0u64;
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
/// process with zero ownership coordination, and unlike `set-visibility` there is no routed
/// `/api` endpoint to deliver a fork/repack/delete/create to the owning node, so — mirroring
/// `set-image-visibility`, which is in the same boat — a configured fleet means refuse rather
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
        ["admin", "add-token", owner] => {
            println!("{}", store.create_token(owner).await?);
            Ok(())
        }
        ["admin", "add-key", owner, file] => {
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
            let upstream = upstream.unwrap_or_else(|| "http://rustic-git:8081".into());
            let res = reqwest::Client::builder()
                // A peer that accepts the connection and never answers must not hang the admin
                // command forever. Same bound `api-serve` puts on its upstream calls.
                .timeout(rustic_git::api::UPSTREAM_TIMEOUT)
                .build()?
                .post(format!(
                    "{}/api/{o}/{n}/visibility?visibility={vis}",
                    upstream.trim_end_matches('/')
                ))
                .header(rustic_git::proxy::PEER_HEADER, secret.unwrap_or_default())
                .send()
                .await?;
            let status = res.status();
            if status.is_success() {
                return Ok(());
            }
            let body = res.text().await.unwrap_or_default();
            Err(rustic_git::err(format!("set-visibility: {status}: {body}")))
        }
        ["admin", "set-image-visibility", path, vis] => {
            let (o, n) = path.split_once('/').ok_or("owner/image")?;
            if !matches!(*vis, "public" | "private") {
                return Err(rustic_git::err("visibility must be public or private"));
            }
            // Mirrors `set-visibility` above: same either-variable "is a fleet configured" test,
            // for the same reason (keying on the secret alone would let an operator whose shell
            // doesn't export it take the direct path against a live fleet). But there is no
            // `/visibility` browse route for images to post to — item 1 keeps `images`/`imagetags`
            // from ever opening an image database on an unrouted node, and adding a write endpoint
            // would reopen that same hole. So when a fleet IS configured, this cannot deliver the
            // flip to the owning node at all, and must refuse rather than write here and land mid-
            // `docker push`, fencing the serving node's writer. Only with nothing configured — a
            // single node or an offline run — does it take the direct path, saying out loud what
            // it is assuming, same as `set-visibility` does.
            let upstream = std::env::var("RUSTIC_GIT_UPSTREAM").ok();
            let secret = std::env::var("RUSTIC_GIT_PEER_SECRET").ok();
            if upstream.is_some() || secret.is_some() {
                return Err(rustic_git::err(format!(
                    "set-image-visibility: a fleet is configured (RUSTIC_GIT_UPSTREAM or \
                     RUSTIC_GIT_PEER_SECRET set) but there is no routed endpoint to deliver the \
                     flip to the node serving {path} — refusing to write it here. Run this only \
                     when no node is currently serving that image, or add a routed image-visibility \
                     endpoint."
                )));
            }
            eprintln!(
                "set-image-visibility: no RUSTIC_GIT_UPSTREAM or RUSTIC_GIT_PEER_SECRET set — \
                 writing {path} directly, assuming NO node is currently serving it. If one is, it \
                 keeps answering from its own view for several seconds."
            ); // ponytail: eprintln
            store.set_image_visibility(o, n, *vis == "public").await
        }
        _ => Err(rustic_git::err(
            "usage: rustic-git serve | admin create-repo <owner>/<name> | admin fork <src>/<name> <owner>/<name> | admin delete-repo <owner>/<name> | admin repack <owner>/<name> | admin add-token <owner> | admin add-key <owner> <pubkey-file> | admin set-visibility <owner>/<name> public|private | admin set-image-visibility <owner>/<image> public|private | admin purge-cache <owner>/<name>",
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
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    async fn store() -> std::sync::Arc<rustic_git::store::Store> {
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
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    /// Also covers the fleet-vs-direct guard added to mirror `set-visibility`'s, in ONE test since
    /// it mutates process-wide env vars and a second test doing the same would race it. Unlike
    /// `set-visibility` there's no routed image endpoint, so "fleet configured" means refuse, not
    /// redirect: catches the guard writing anyway when only one of the two vars is set.
    #[tokio::test]
    async fn set_image_visibility_writes_it() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

        // An upstream configured but no secret in this shell: still must refuse, never write here.
        std::env::set_var("RUSTIC_GIT_UPSTREAM", "http://127.0.0.1:1");
        let e = run(&["admin", "set-image-visibility", "acme/nginx", "public"], &store)
            .await
            .expect_err("a fleet configured but no image route must refuse, not write directly");
        assert!(!store.image_is_public("acme", "nginx").await.unwrap(), "nothing written here: {e}");
        std::env::remove_var("RUSTIC_GIT_UPSTREAM");

        store.pool.close().await;
    }
}
