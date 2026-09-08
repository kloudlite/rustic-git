//! `OwnerKeys` → the file sshd reads. The projection is the api's; this node's whole job is to
//! make the bytes on disk equal the spec and say so in status.

use super::{status::patch_status, Ctx};
use kloudlite_workspaces::{crd, k8s};
use kube::{Api, ResourceExt};
use std::collections::HashSet;
use std::sync::Arc;

/// In place, never by rename: pods hold the inode through their hostPath mount.
///
// ponytail: between the `O_TRUNC` and the `write_all` the file is empty, so a login racing the
// write is refused. Sub-millisecond, and the next attempt succeeds. Upgrade path if that ever
// matters: keep the file open, write from offset 0 and `set_len` after, or stage the bytes in a
// temp file and `copy_file_range` them into the same inode.
pub fn write_keys_file(pool: &str, owner: &str, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let path = k8s::keys_file(pool, owner);
    std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap())?;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    // `mode` on the open only applies to a file this call CREATED; an existing one keeps whatever
    // it had, so the mode is restated unconditionally.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    // sshd reads the file as the login user (`kl`), through StrictModes off; the uid is what lets
    // the read succeed at all. Only when it is not already right — the agent runs as root in the
    // pod, but `cargo test` runs as an ordinary user who may not give a file away, and a chown
    // that cannot change anything must not be the reason a test fails.
    if f.metadata()?.uid() != k8s::SSH_UID as u32 {
        match std::os::unix::fs::chown(&path, Some(k8s::SSH_UID as u32), Some(k8s::SSH_UID as u32)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && unsafe { libc::geteuid() } != 0 => {
                tracing::debug!(%path, "keys.chown.skipped");
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Owner directories under `keys_root` with no live `OwnerKeys` behind them. Their file is blanked
/// rather than removed: a pod already running holds the inode, and an empty file is "nobody is
/// admitted" where a deleted one would be a mount the kubelet refuses on the next start.
fn stale_owners(pool: &str, live: &HashSet<String>) -> Vec<String> {
    let Ok(dir) = std::fs::read_dir(k8s::keys_root(pool)) else { return vec![] };
    dir.flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|owner| !live.contains(owner))
        .collect()
}

/// The write is unconditional — no read-back compare first: it is idempotent and cheap, and
/// `condition_since` keeps `lastTransitionTime` when nothing actually transitioned.
///
/// Status is per CLUSTER while the file is per NODE, so what lands here is the last node to
/// converge, under one field manager. That answers the only question asked of it — "is this key
/// live" — because every node runs the same loop off the same watch within seconds.
async fn converge(ctx: &Ctx, api: &Api<crd::OwnerKeys>, obj: crd::OwnerKeys) {
    let owner = obj.name_any();
    let generation = obj.spec.generation;
    let prev = obj.status.as_ref().and_then(|s| s.conditions.iter().find(|c| c.type_ == crd::KEYS_SYNCED));
    let (ok, reason, msg) = match write_keys_file(&ctx.pool, &owner, &obj.spec.authorized_keys) {
        Ok(()) if obj.spec.authorized_keys.is_empty() => (true, "NoKeys", "no member has a key; nobody is admitted"),
        Ok(()) => (true, "Applied", "written to this node's keys file"),
        Err(e) => {
            tracing::warn!(%owner, error = %e, "keys.write.failed");
            (false, "WriteFailed", "could not write this node's keys file")
        }
    };
    let mut status = serde_json::json!({
        "conditions": [crd::condition_since(prev, crd::KEYS_SYNCED, ok, reason, msg, generation)],
    });
    // Only on success, and by ABSENCE on failure: this is a forced server-side apply, so a null
    // here would clear the last generation that did land rather than leave it standing.
    if ok {
        status["observedGeneration"] = generation.into();
    }
    if let Err(e) = patch_status(api, &owner, "OwnerKeys", status).await {
        tracing::warn!(%owner, error = %e, "keys.status.failed");
    }
}

/// An owner whose projection is gone is an owner nobody may log in as. No status patch — the
/// object it would be written to is what just disappeared.
fn revoke(pool: &str, owner: &str) {
    if let Err(e) = write_keys_file(pool, owner, "") {
        tracing::warn!(%owner, error = %e, "keys.revoke.failed");
    }
}

/// Every `OwnerKeys` in the cluster, converged on every event and on a ten-minute tick — the tick
/// is what heals a file deleted by hand, a write that failed transiently, and a deletion this node
/// was not watching for (a watch drops events it was disconnected across; the disk must not).
pub async fn run(ctx: Arc<Ctx>) {
    use futures::StreamExt;
    use kube::runtime::{watcher, WatchStreamExt};
    let api: Api<crd::OwnerKeys> = Api::all(ctx.client.clone());
    let watched = api.clone();
    // The RAW event stream, not `applied_objects()`: a deletion has to reach the disk, and that
    // helper drops exactly the event that says so.
    run_with(ctx, api, move || {
        watcher(watched.clone(), watcher::Config::default()).default_backoff().boxed()
    })
    .await
}

type KeyEvents = futures::stream::BoxStream<'static, Result<kube::runtime::watcher::Event<crd::OwnerKeys>, kube::runtime::watcher::Error>>;

/// The watch is a nudge; the tick is what makes this correct. So a stream that ENDS is rebuilt,
/// never fatal: kube-runtime's backoff gives up after its elapsed limit, which a burst of
/// `too old resource version … Expired` reached on 2026-09-08. This loop used to `return` there,
/// and the ten-minute tick died with it — keys stopped converging on every node for two hours,
/// `authorized_keys` went stale, and every default-image workspace started meanwhile parked at
/// `KeysNotReady`. Nothing about a watch is allowed to end this task.
async fn run_with<F>(ctx: Arc<Ctx>, api: Api<crd::OwnerKeys>, mut watch: F)
where
    F: FnMut() -> KeyEvents,
{
    use futures::StreamExt;
    use kube::runtime::watcher;
    let mut events = watch();
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(600));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(watcher::Event::Apply(obj) | watcher::Event::InitApply(obj))) => converge(&ctx, &api, obj).await,
                Some(Ok(watcher::Event::Delete(obj))) => revoke(&ctx.pool, &obj.name_any()),
                Some(Ok(watcher::Event::Init | watcher::Event::InitDone)) => {}
                Some(Err(e)) => tracing::warn!(error = %e, "keys.watch.failed"),
                None => {
                    // Paced so a stream that ends immediately cannot spin; the tick keeps
                    // converging throughout, so the delay costs freshness and nothing else.
                    tracing::warn!("keys.watch.ended");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    events = watch();
                }
            },
            _ = tick.tick() => {
                if let Ok(list) = api.list(&Default::default()).await {
                    let live: HashSet<String> = list.items.iter().map(|o| o.name_any()).collect();
                    for owner in stale_owners(&ctx.pool, &live) {
                        revoke(&ctx.pool, &owner);
                    }
                    for obj in list.items {
                        converge(&ctx, &api, obj).await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 2026-09-08 outage in one test: the watch stream ends, and the loop must build a new one
    /// instead of returning and taking the ten-minute resync tick down with it.
    #[tokio::test(start_paused = true)]
    async fn a_watch_that_ends_is_rebuilt_rather_than_fatal() {
        use futures::StreamExt;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, _rec) = crate::testsupport::test_ctx(tmp.path(), "node-a", vec![]);
        let api: Api<crd::OwnerKeys> = Api::all(ctx.client.clone());
        let built = Arc::new(AtomicUsize::new(0));
        let seen = built.clone();
        let watch = move || {
            seen.fetch_add(1, Ordering::SeqCst);
            futures::stream::empty().boxed()
        };
        // `run_with` never returns by design, so the timeout is how the test ends.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(60), run_with(ctx, api, watch)).await;
        assert!(built.load(Ordering::SeqCst) > 1, "the watch must be rebuilt, not returned from");
    }

    /// Written IN PLACE: the pod holds the file's inode through its hostPath mount, so a rename
    /// would leave sshd reading the old file forever. Same inode before and after, and a shorter
    /// second write truncates rather than leaving the old tail behind.
    #[test]
    fn the_keys_file_keeps_its_inode_and_truncates() {
        use std::os::unix::fs::MetadataExt;
        let pool = tempfile::tempdir().unwrap();
        let pool = pool.path().to_str().unwrap();
        write_keys_file(pool, "acme", "ssh-ed25519 AAAA a\nssh-ed25519 BBBB b\n").unwrap();
        let path = kloudlite_workspaces::k8s::keys_file(pool, "acme");
        let ino = std::fs::metadata(&path).unwrap().ino();
        write_keys_file(pool, "acme", "ssh-ed25519 CCCC c\n").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), ino);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ssh-ed25519 CCCC c\n");
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    }

    /// An empty set is an EMPTY FILE, written: a missing file parks the pod, an empty one lets it
    /// run with nobody admitted — which is the truth for an owner with no keys.
    #[test]
    fn an_empty_set_writes_an_empty_file() {
        let pool = tempfile::tempdir().unwrap();
        let pool = pool.path().to_str().unwrap();
        write_keys_file(pool, "acme", "").unwrap();
        assert_eq!(std::fs::read_to_string(kloudlite_workspaces::k8s::keys_file(pool, "acme")).unwrap(), "");
    }

    /// A deletion this node missed is caught by the tick instead: a file with no `OwnerKeys` left
    /// behind it would admit whoever it last named, forever.
    #[test]
    fn an_owner_with_no_object_left_is_stale_and_is_blanked() {
        let pool = tempfile::tempdir().unwrap();
        let pool = pool.path().to_str().unwrap();
        for owner in ["alice", "acme"] {
            write_keys_file(pool, owner, "ssh-ed25519 AAAA a\n").unwrap();
        }
        let live = HashSet::from(["alice".to_string()]);
        assert_eq!(stale_owners(pool, &live), ["acme"]);
        revoke(pool, "acme");
        assert_eq!(std::fs::read_to_string(kloudlite_workspaces::k8s::keys_file(pool, "acme")).unwrap(), "");
        assert_eq!(stale_owners(pool, &live), ["acme"], "still stale: the file stays, blanked");
        // The live owner is untouched.
        assert!(!std::fs::read_to_string(kloudlite_workspaces::k8s::keys_file(pool, "alice")).unwrap().is_empty());
    }

    /// No `keys/` directory yet (a node that has never converged) is not an error.
    #[test]
    fn a_pool_with_no_keys_directory_has_nothing_stale() {
        let pool = tempfile::tempdir().unwrap();
        assert!(stale_owners(pool.path().to_str().unwrap(), &HashSet::new()).is_empty());
    }
}
