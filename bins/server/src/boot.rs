//! Host-key/admin-CLI plumbing for `main()`: reading or generating the SSH host key, the
//! `admin` subcommand dispatch and the fleet-vs-direct guard those subcommands share. Split out
//! of the old `main.rs` at existing function boundaries.

use crate::config::env;
use crate::gc::RepackExt;
use crate::registry::store::ImageExt;
use crate::store::Store;
use crate::vol_agent::JobsState;
use crate::Result;
use std::sync::Arc;

/// Builds this node's `JobsState` for the agent work surface (Task 14): a Cosmos-backed
/// `MetaStore` when `COSMOS_ENDPOINT` is set (same selection code as `bins/api`'s — every server
/// node constructs its own client against the same Cosmos DB, no ownership coordination needed,
/// since the workspaces metadata is not a per-repo SlateDB), otherwise `None` — the routes stay
/// mounted and answer 503 rather than not existing at all. Also spawns the 30s requeue sweep
/// (moved off `bins/api`, which no longer runs it) when a store is configured.
pub async fn build_jobs_state() -> Result<Arc<JobsState>> {
    let store: Option<Arc<dyn rustic_git_workspaces::store::MetaStore>> =
        match std::env::var("COSMOS_ENDPOINT") {
            Ok(endpoint) if !endpoint.is_empty() => {
                let key = std::env::var("COSMOS_KEY")
                    .map_err(|_| crate::err("COSMOS_KEY required with COSMOS_ENDPOINT"))?;
                let db = env("COSMOS_DB", "rustic-git");
                tracing::info!(db = %db, "workspaces metadata in cosmos db");
                let s = rustic_git_workspaces::cosmos::CosmosStore::new(&endpoint, &key, &db)
                    .await
                    .map_err(|e| crate::err(format!("connecting to cosmos: {e:?}")))?;
                Some(Arc::new(s))
            }
            _ => {
                tracing::warn!(
                    "COSMOS_ENDPOINT unset: agents can only authenticate with a break-glass token"
                );
                None
            }
        };
    Ok(Arc::new(JobsState::new(store)))
}

// ponytail: no CryptoRng impl for OsRng is reachable through the rand_core
// version russh/ssh-key 0.7.0-rc.11 pin (0.10.1, which has no OsRng at all);
// shell out to ssh-keygen (present on any host running sshd) instead of
// pulling in a duplicate rand_core dependency just for key generation.
pub fn host_key(path: &str) -> Result<russh::keys::PrivateKey> {
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
            return Err(crate::err("ssh-keygen failed to generate host key"));
        }
    }
    Ok(russh::keys::PrivateKey::read_openssh_file(p)?)
}

/// The fingerprint of an OpenSSH public key line, or an error naming what is wrong with it. Used
/// to validate and identify a key before it is stored. A body-identical copy lives in
/// `crates/api/src/credentials.rs` (that crate cannot depend on this binary) — a change here must
/// be mirrored there. `crate::auth` is `rustic-git-storage`, which stays free of the ssh key
/// parsing dependency on purpose, so it is not a home for this.
pub(crate) fn ssh_fingerprint(line: &str) -> Result<String> {
    let key = russh::keys::PublicKey::from_openssh(line.trim())
        .map_err(|_| crate::err("that does not look like an OpenSSH public key"))?;
    Ok(key.fingerprint(russh::keys::HashAlg::Sha256).to_string())
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
pub(crate) fn fleet_guard(cmd: &str, path: &str) -> Result<()> {
    fleet_check(cmd, path, std::env::var("RUSTIC_GIT_UPSTREAM").ok(), std::env::var("RUSTIC_GIT_PEER_SECRET").ok())
}

/// The decision itself, with the environment already read — so the test can state both variables
/// without mutating this process's environment.
fn fleet_check(cmd: &str, path: &str, upstream: Option<String>, secret: Option<String>) -> Result<()> {
    if upstream.is_some() || secret.is_some() {
        return Err(crate::err(format!(
            "{cmd}: a fleet is configured (RUSTIC_GIT_UPSTREAM or RUSTIC_GIT_PEER_SECRET set) but \
             there is no routed endpoint to deliver this to the node serving {path} — refusing to \
             run it here. Run this only when no node is currently serving that repo."
        )));
    }
    eprintln!(
        "{cmd}: no RUSTIC_GIT_UPSTREAM or RUSTIC_GIT_PEER_SECRET set — running against {path} \
         directly, assuming NO node is currently serving it. If one is, opening its database here \
         fences the serving node's writer."
    ); // CLI output: a person ran this admin subcommand; RUST_LOG must not be able to suppress it.
    Ok(())
}

/// Ceiling on the whole `post_to_owner` call, and on reading its reply — this binary must not
/// depend on `rustic-git-api` (that crate carries `pgp`/`mongodb`/`russh` this process has no
/// other reason to link) just to reuse its identical constant.
const UPSTREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Refuse to buffer an error reply past this size rather than hold it in memory — a
/// body-identical bound to `rustic_git_api::forward::read_bounded`'s, kept in sync by hand.
const MAX_REPLY: usize = 8 << 20;

/// Buffer an upstream reply, refusing anything past `MAX_REPLY` instead of holding it unbounded.
async fn read_bounded(mut r: reqwest::Response) -> Result<String> {
    let mut out = Vec::new();
    while let Some(chunk) = r.chunk().await? {
        if out.len() + chunk.len() > MAX_REPLY {
            return Err(crate::err("upstream reply is too large"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Deliver a flip to the node that owns `path`'s database: POST it to the peer Service and let
/// the `route` middleware carry it. Carries the owner as the peer identity because
/// `imagevisibility` authorizes on it (the repo route ignores it). A peer that accepts and never
/// answers must not hang the command forever, so the call is bounded like the api's upstream calls.
pub(crate) async fn post_to_owner(
    cmd: &str,
    owner: &str,
    route: &str,
    upstream: Option<String>,
    secret: Option<String>,
) -> Result<()> {
    let upstream = upstream.unwrap_or_else(|| "http://rustic-git:8081".into());
    let res = reqwest::Client::builder()
        .timeout(UPSTREAM_TIMEOUT)
        .build()?
        .post(format!("{}{route}", upstream.trim_end_matches('/')))
        .header(rustic_git_core::peer::PEER_HEADER, secret.unwrap_or_default())
        .header(rustic_git_core::peer::OWNER_HEADER, owner)
        .send()
        .await
        .map_err(|e| crate::err(format!("{cmd}: {e}")))?;
    let status = res.status();
    if status.is_success() {
        return Ok(());
    }
    let body = read_bounded(res).await.unwrap_or_default();
    Err(crate::err(format!("{cmd}: {status}: {body}")))
}

pub async fn run(a: &[&str], store: &Arc<Store>) -> Result<()> {
    match a {
        ["admin", "fork", src, dst] => {
            let (so, sn) = src.split_once('/').ok_or("owner/name")?;
            let (o, n) = dst.split_once('/').ok_or("owner/name")?;
            fleet_guard("admin fork", dst)?;
            let src = store.open_repo(so, sn).await?.ok_or("source repository not found")?;
            store.fork(&src, o, n).await
        }
        ["admin", "repack", path] => {
            let (o, n) = path.split_once('/').ok_or("owner/name")?;
            fleet_guard("admin repack", path)?;
            let (before, after) = store.repack(o, n).await?;
            println!("repacked {path}: {before} packs -> {after}");
            Ok(())
        }
        ["admin", "delete-repo", path] => {
            let (o, n) = path.split_once('/').ok_or("owner/name")?;
            fleet_guard("admin delete-repo", path)?;
            store.delete_repo(o, n).await
        }
        // Clean up after a repo that was deleted BEFORE delete removed the database files: the
        // directory survives, so the GC sweep reads it as an existing repo that merely lost its
        // marker and recreates one, and the repo reappears in every listing. Refuses to touch a
        // repo that still exists, so it can only ever remove what is already gone.
        ["admin", "purge-ghost-repo", path] => {
            let (o, n) = path.split_once('/').ok_or("owner/name")?;
            fleet_guard("admin purge-ghost-repo", path)?;
            if store.repo_exists(o, n).await? {
                return Err(crate::err(format!(
                    "{path} still exists — purge only removes the remains of a deleted repo"
                )));
            }
            let _ = crate::index::remove(store, crate::index::Kind::Repo, o, n).await;
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
            let wal = format!("{}/wal", crate::ownership::PATH);
            let count = |os: std::sync::Arc<dyn slatedb::object_store::ObjectStore>, p: String| async move {
                use futures::StreamExt;
                os.list(Some(&slatedb::object_store::path::Path::from(p)))
                    .filter_map(|r| async move { r.ok() })
                    .count()
                    .await
            };

            let admin = slatedb::admin::AdminBuilder::new(
                crate::ownership::PATH,
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
            fleet_guard("admin create-repo", path)?;
            store.create_repo(o, n).await
        }
        ["admin", "revoke-tokens", owner] => {
            let n = store.revoke_tokens_for(owner).await?;
            println!("revoked {n} token(s) for {owner}");
            Ok(())
        }
        ["admin", "add-token", owner] => {
            // Same rule the api tier applies: a credential for an owner no URL can name is a
            // credential nothing can use, and a reserved name (`api`, `v2`) would be worse.
            if !crate::store::valid_owner(owner) {
                return Err(crate::err(format!("{owner}: not a valid owner name")));
            }
            println!("{}", store.create_token(owner).await?);
            Ok(())
        }
        ["admin", "add-key", owner, file] => {
            if !crate::store::valid_owner(owner) {
                return Err(crate::err(format!("{owner}: not a valid owner name")));
            }
            let line = std::fs::read_to_string(file)?;
            let fp = ssh_fingerprint(&line)?;
            store.add_ssh_key(owner, &fp).await
        }
        ["admin", "purge-cache", path] => {
            let (o, n) = path.split_once('/').ok_or("owner/name")?;
            store.cache.bump_generation(&format!("{o}/{n}")).await
        }
        ["admin", "set-visibility", path, vis] => {
            let (o, n) = path.split_once('/').ok_or("owner/name")?;
            if !matches!(*vis, "public" | "private") {
                return Err(crate::err("visibility must be public or private"));
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
                ); // CLI output: a person ran this admin subcommand; RUST_LOG must not be able to suppress it.
                return store.set_public(o, n, *vis == "public").await;
            }
            post_to_owner("set-visibility", o, &format!("/api/{o}/{n}/visibility?visibility={vis}"), upstream, secret).await
        }
        ["admin", "set-image-visibility", path, vis] => {
            let (o, n) = path.split_once('/').ok_or("owner/image")?;
            if !matches!(*vis, "public" | "private") {
                return Err(crate::err("visibility must be public or private"));
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
                ); // CLI output: a person ran this admin subcommand; RUST_LOG must not be able to suppress it.
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
        _ => Err(crate::err(
            "usage: rustic-git serve | admin create-repo <owner>/<name> | admin fork <src>/<name> <owner>/<name> | admin delete-repo <owner>/<name> | admin purge-ghost-repo <owner>/<name> | admin ownership-gc [min-age-secs] | admin repack <owner>/<name> | admin add-token <owner> | admin revoke-tokens <owner> | admin add-key <owner> <pubkey-file> | admin set-visibility <owner>/<name> public|private | admin set-image-visibility <owner>/<image> public|private | admin purge-cache <owner>/<name>",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{fleet_check, run};

    #[test]
    fn fleet_guard_refuses_when_either_var_is_set() {
        assert!(fleet_check("admin repack", "alice/web", None, None).is_ok());
        assert!(fleet_check("admin repack", "alice/web", Some("http://x".into()), None).is_err());
        assert!(fleet_check("admin repack", "alice/web", None, Some("secret".into())).is_err());
        assert!(fleet_check(
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

    pub(crate) async fn store() -> std::sync::Arc<crate::store::Store> {
        // Leaked so the store outlives the temp dir without a struct to hold both.
        let tmp = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        std::sync::Arc::new(
            crate::store::Store::open(
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
        use crate::index::{self, Kind};
        use slatedb::object_store::ObjectStoreExt;
        use crate::registry::store::ImageExt;
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
