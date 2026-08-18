use rustic_git::{store::Store, Result};
use std::sync::Arc;

fn env(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

/// Read `[profile]` from an AWS INI file; returns key=value pairs.
fn aws_ini(file: &str, profile: &str) -> Vec<(String, String)> {
    let Some(home) = std::env::var_os("HOME") else {
        return vec![];
    };
    let Ok(text) = std::fs::read_to_string(std::path::Path::new(&home).join(".aws").join(file))
    else {
        return vec![];
    };
    let mut out = vec![];
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            let name = line.trim_matches(&['[', ']'][..]).trim();
            inside = name == profile || name == format!("profile {profile}");
        } else if inside {
            if let Some((k, v)) = line.split_once('=') {
                out.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
    }
    out
}

/// If no AWS_ACCESS_KEY_ID in env, export credentials/region from ~/.aws for $AWS_PROFILE (default "default").
fn load_aws_profile() {
    if std::env::var_os("AWS_ACCESS_KEY_ID").is_some() {
        return;
    }
    let profile = env("AWS_PROFILE", "default");
    let map = [
        ("aws_access_key_id", "AWS_ACCESS_KEY_ID"),
        ("aws_secret_access_key", "AWS_SECRET_ACCESS_KEY"),
        ("aws_session_token", "AWS_SESSION_TOKEN"),
        ("region", "AWS_REGION"),
        ("endpoint_url", "AWS_ENDPOINT"),
    ];
    for (k, v) in aws_ini("credentials", &profile)
        .into_iter()
        .chain(aws_ini("config", &profile))
    {
        if let Some((_, env_key)) = map.iter().find(|(ini, _)| *ini == k) {
            if std::env::var_os(env_key).is_none() {
                std::env::set_var(env_key, v);
            }
        }
    }
    // ponytail: static keys + region only; SSO/assume-role profiles need the AWS SDK credential chain
}

fn object_store() -> Result<Arc<dyn slatedb::object_store::ObjectStore>> {
    load_aws_profile();
    let url = std::env::var("RUSTIC_GIT_S3_URL").map_err(|_| {
        rustic_git::err("RUSTIC_GIT_S3_URL required (e.g. s3://bucket, or mem:// for testing)")
    })?;
    let os: Arc<dyn slatedb::object_store::ObjectStore> = if url == "mem://" {
        Arc::new(slatedb::object_store::memory::InMemory::new())
    } else if let Some(bucket) = url.strip_prefix("s3://") {
        // Built by hand rather than via resolve_object_store so the request timeout can be
        // raised: repack uploads a whole repository in one PUT, and object_store's 180s default
        // aborts that on a slow or distant link.
        use slatedb::object_store::{aws::AmazonS3Builder, ClientOptions};
        let timeout = env("RUSTIC_GIT_S3_TIMEOUT_SECS", "900")
            .parse()
            .map_err(|_| rustic_git::err("RUSTIC_GIT_S3_TIMEOUT_SECS must be a number"))?;
        let mut b = AmazonS3Builder::from_env()
            .with_bucket_name(bucket)
            .with_client_options(
                ClientOptions::new().with_timeout(std::time::Duration::from_secs(timeout)),
            );
        if let Ok(ep) = std::env::var("AWS_ENDPOINT") {
            b = b.with_endpoint(ep).with_virtual_hosted_style_request(false);
        }
        Arc::new(b.build()?)
    } else {
        slatedb::Db::resolve_object_store(&url)?
    };
    Ok(os)
}

async fn open_store(background: bool) -> Result<Arc<Store>> {
    Ok(Arc::new(
        Store::open(
            object_store()?,
            env("RUSTIC_GIT_CACHE_DIR", "./cache").into(),
            background,
        )
        .await?,
    ))
}

// ponytail: no CryptoRng impl for OsRng is reachable through the rand_core
// version russh/ssh-key 0.7.0-rc.11 pin (0.10.1, which has no OsRng at all);
// shell out to ssh-keygen (present on any host running sshd) instead of
// pulling in a duplicate rand_core dependency just for key generation.
fn host_key(path: &str) -> Result<russh::keys::PrivateKey> {
    let p = std::path::Path::new(path);
    if !p.exists() {
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
async fn serve() -> Result<()> {
    let store = Arc::new(Store::open(object_store()?, env("RUSTIC_GIT_CACHE_DIR", "./cache").into(), true).await?);
    store.spawn_health_probe();

    let peer_addr = env("RUSTIC_GIT_PEER_ADDR", "0.0.0.0:8081");
    let peer_port: u16 = peer_addr.rsplit(':').next().and_then(|p| p.parse().ok())
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
        ("rustic-git-0".to_string(), secret, rustic_git::ownership::OwnershipStore::Solo)
    } else {
        let need = |k: &str| {
            std::env::var(k).ok().filter(|s| !s.is_empty())
                .ok_or_else(|| rustic_git::err(format!("{k} is required with RUSTIC_GIT_PEER_SVC")))
        };
        let me = need("RUSTIC_GIT_SELF")?;
        let secret = need("RUSTIC_GIT_PEER_SECRET")?;
        // Fails loudly on a malformed name: the leader is derived from it, and a name without an
        // ordinal would silently make this pod its own leader — two leaders, two maps.
        let leader = rustic_git::ownership::leader_of(&me)?;
        let store = rustic_git::ownership::OwnershipStore::open(store.os.clone(), me == leader).await?;
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
    let replicas: u32 = std::env::var("RUSTIC_GIT_REPLICAS").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(1);
    let app = Arc::new(rustic_git::App::new(store.clone(), Arc::new(ownership), me, addr_of, peer_secret, replicas));
    // The leader publishes where it can be reached, so followers do not have to ask cluster DNS on
    // the claim path. Its own pod IP: the name would just send them back through the resolver whose
    // negative cache is the problem.
    if app.is_leader() {
        if let Ok(ip) = std::env::var("RUSTIC_GIT_POD_IP") {
            if !ip.is_empty() {
                if let Err(e) = app.ownership.put_leader_addr(&format!("{ip}:{peer_port}")).await {
                    eprintln!("publishing the leader address: {e}"); // ponytail: eprintln
                }
            }
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
    let peer_stream = tokio::net::TcpListener::bind(rustic_git::proxy::stream_addr(&peer_addr)).await?;
    let key = host_key(&env("RUSTIC_GIT_HOST_KEY", "./host_key"))?;
    eprintln!("http on {} ssh on {} — peers on {} and {}, up to {} warm databases",
        http.local_addr()?, ssh.local_addr()?, peer_http.local_addr()?, peer_stream.local_addr()?, store.pool.max_warm());

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
    tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("sigterm handler");
        term.recv().await;
        pool_for_term.close().await; // release every repo so peers can take them without fencing us
        let _ = term_tx.send(true);  // then let the listeners drain what is in flight
    });
    let wait = |mut rx: tokio::sync::watch::Receiver<bool>| async move { while !*rx.borrow() { if rx.changed().await.is_err() { break; } } };
    let (a2, a3, a4) = (app.clone(), app.clone(), app.clone());
    let http_srv = axum::serve(http, rustic_git::http::router(a2)).with_graceful_shutdown(wait(term_rx.clone()));
    let peer_srv = axum::serve(peer_http, rustic_git::http::peer_router(a3)).with_graceful_shutdown(wait(term_rx.clone()));
    // Both HTTP servers as ONE select arm: select! returns when its first arm resolves, and if
    // each server were its own arm the first to finish draining would end the select and
    // pool.close() would run under the other's in-flight requests. try_join waits for both.
    tokio::select! {
        r = async { tokio::try_join!(http_srv, peer_srv) } => { r?; }
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

/// Renewal, and pruning on the leader — the two background halves of the lifecycle invariant.
/// The work itself lives on `App`; these are only the clocks.
fn spawn_lease_tasks(app: Arc<rustic_git::App>) {
    use rustic_git::ownership::{LEASE_TTL, RENEW_EVERY};
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(RENEW_EVERY).await;
            // A renewal that cannot reach the leader is not fatal: the lease runs to its TTL and
            // the next beat is three seconds away. Missing every beat for a whole TTL is what lets
            // another node claim, which is the intended outcome.
            if let Err(e) = a.renew_once().await {
                eprintln!("renewing leases: {e}"); // ponytail: eprintln
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

async fn run(a: &[&str], store: &Arc<Store>) -> Result<()> {
    match a {
        ["admin", "fork", src, dst] => {
            let (so, sn) = src.split_once('/').ok_or("owner/name")?;
            let (o, n) = dst.split_once('/').ok_or("owner/name")?;
            let src = store.open_repo(so, sn).await?.ok_or("source repository not found")?;
            store.fork(&src, o, n).await
        }
        ["admin", "repack", path] => {
            let (o, n) = path.split_once('/').ok_or("owner/name")?;
            let (before, after) = store.repack(o, n).await?;
            println!("repacked {path}: {before} packs -> {after}");
            Ok(())
        }
        ["admin", "delete-repo", path] => {
            let (o, n) = path.split_once('/').ok_or("owner/name")?;
            store.delete_repo(o, n).await
        }
        ["admin", "create-repo", path] => {
            let (o, n) = path.split_once('/').ok_or("owner/name")?;
            store.create_repo(o, n).await
        }
        ["admin", "add-token", owner] => {
            println!("{}", store.create_token(owner).await?);
            Ok(())
        }
        ["admin", "add-key", owner, file] => {
            store.add_ssh_key(owner, &std::fs::read_to_string(file)?).await
        }
        _ => Err(rustic_git::err(
            "usage: rustic-git serve | admin create-repo <owner>/<name> | admin fork <src>/<name> <owner>/<name> | admin delete-repo <owner>/<name> | admin repack <owner>/<name> | admin add-token <owner> | admin add-key <owner> <pubkey-file>",
        )),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
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
