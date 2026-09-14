//! `OwnerKeys` → the file sshd reads. The projection is the api's; this node's whole job is to
//! make the bytes on disk equal the spec and say so in status.

use super::Ctx;
use kloudlite_workspaces::{crd, k8s};
use kube::{Api, ResourceExt};
use std::collections::HashSet;
use std::sync::Arc;

/// In place, never by rename: pods hold the inode through their hostPath mount. Answers whether it
/// wrote: a file already holding these bytes, mode and uid is left alone — no truncate, no fsync.
/// On 2026-09-14 the tick blanked ~316 stale owners a minute per node, an fsync each on btrfs, on
/// a runtime worker: ~1 s a minute of a worker whose LIFO slot held a kube connection task, which
/// is `kube.timeout layer=inner dials=0` at boot+k·60 s. Every caller runs this on the blocking pool.
///
// ponytail: between the `O_TRUNC` and the `write_all` the file is empty, so a login racing the
// write is refused. Sub-millisecond, and the next attempt succeeds. Upgrade path if that ever
// matters: keep the file open, write from offset 0 and `set_len` after, or stage the bytes in a
// temp file and `copy_file_range` them into the same inode.
pub fn write_keys_file(pool: &str, owner: &str, contents: &str) -> std::io::Result<bool> {
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let path = k8s::keys_file(pool, owner);
    // No `unwrap`: a path with no parent is a bug in `keys_file`, and a panic inside the keys
    // reconciler takes the whole controller down rather than failing this one owner.
    let dir = std::path::Path::new(&path)
        .parent()
        .ok_or_else(|| std::io::Error::other(format!("{path} has no parent directory")))?;
    std::fs::create_dir_all(dir)?;
    if let Ok(m) = std::fs::metadata(&path) {
        // A non-root test process cannot give a file away, so the uid only counts as root.
        let uid_ok = m.uid() == k8s::SSH_UID as u32 || unsafe { libc::geteuid() } != 0;
        if m.is_file()
            && m.len() == contents.len() as u64
            && m.mode() & 0o777 == 0o600
            && uid_ok
            && std::fs::read(&path).is_ok_and(|b| b == contents.as_bytes())
        {
            return Ok(false);
        }
    }
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
    Ok(true)
}

/// Owner directories under `keys_root` with no live `OwnerKeys` behind them.
fn stale_owners(pool: &str, live: &HashSet<String>) -> Vec<String> {
    let Ok(dir) = std::fs::read_dir(k8s::keys_root(pool)) else { return vec![] };
    dir.flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|owner| !live.contains(owner))
        .collect()
}

/// The write skips a file already in the desired state and runs on the blocking pool; the
/// `condition_since` keeps `lastTransitionTime` when nothing actually transitioned.
///
/// Status is per CLUSTER while the file is per NODE, so what lands here is the last node to
/// converge, under one field manager. That answers the only question asked of it — "is this key
/// live" — because every node runs the same loop off the same watch within seconds.
async fn converge(ctx: &Ctx, api: &Api<crd::OwnerKeys>, obj: crd::OwnerKeys) {
    let started = std::time::Instant::now();
    let owner = obj.name_any();
    let generation = obj.spec.generation;
    let prev = obj.status.as_ref().and_then(|s| s.conditions.iter().find(|c| c.type_ == crd::KEYS_SYNCED));
    let (pool, o, keys) = (ctx.pool.clone(), owner.clone(), obj.spec.authorized_keys.clone());
    let written = tokio::task::spawn_blocking(move || write_keys_file(&pool, &o, &keys))
        .await
        .unwrap_or_else(|e| Err(std::io::Error::other(e.to_string())));
    let (ok, reason, msg) = match written {
        Ok(_) if obj.spec.authorized_keys.is_empty() => (true, "NoKeys", "no member has a key; nobody is admitted"),
        Ok(_) => (true, "Applied", "written to this node's keys file"),
        Err(e) => {
            tracing::warn!(%owner, error = %e, "keys.write.failed");
            (false, "WriteFailed", "could not write this node's keys file")
        }
    };
    // The file is per NODE and was just written; the status is per CLUSTER and is only worth a
    // write when it does not already say this. Every node converges on every event, and a status
    // write IS an event on every node: with an unconditional patch here, three nodes echoed one
    // another at 180 patches a second for 24 minutes on 2026-09-11 (see `crd::condition_now`
    // for the nanosecond half of that loop). A status that already carries this generation and
    // this reason is left alone, and the loop has nothing to feed on.
    let settled = ok
        && obj.status.as_ref().and_then(|s| s.observed_generation) == Some(generation)
        && prev.is_some_and(|c| c.status == "True" && c.reason == reason);
    if settled {
        tracing::debug!(%owner, generation, reason, "keys.converged.unchanged");
        return;
    }
    let mut status = serde_json::json!({
        "conditions": [crd::condition_since(prev, crd::KEYS_SYNCED, ok, reason, msg, generation)],
    });
    // Only on success, and by ABSENCE on failure: this is a forced server-side apply, so a null
    // here would clear the last generation that did land rather than leave it standing.
    if ok {
        status["observedGeneration"] = generation.into();
    }
    // Patched here rather than through `patch_status`, which flattens the API error to a string:
    // a 404 has to be told apart, because it is the one failure that is not worth a word of
    // warning. On 2026-09-09 one deleted `OwnerKeys` logged `keys.status.failed` 10133 times in
    // thirty minutes.
    let body = serde_json::json!({
        "apiVersion": format!("{}/{}", crd::GROUP, crd::VERSION),
        "kind": "OwnerKeys",
        "status": status,
    });
    let params = kube::api::PatchParams::apply(crd::AGENT_FIELD_MANAGER).force();
    match api.patch_status(&owner, &params, &kube::api::Patch::Apply(&body)).await {
        // One line per event per node, so a node whose file lags is named rather than inferred
        // from file mtimes after the fact.
        Ok(_) => {
            let ms = started.elapsed().as_millis() as u64;
            tracing::info!(%owner, generation, reason, ms, "keys.converged");
            // A converge that took this long was waiting on the API server, and the loop that
            // runs it is single-file: nothing else converged meanwhile. Named, so a queued node
            // is read from its own log rather than inferred from another node's silence.
            if ms > 5_000 {
                tracing::warn!(%owner, ms, "keys.converge.slow");
            }
        }
        // The object is gone, so there is nothing to say it in and nobody left to admit: the same
        // revoke the Delete event does, which a deletion this node was disconnected across never
        // delivered.
        Err(kube::Error::Api(e)) if e.code == 404 => {
            revoke_off_worker(&ctx.pool, &owner).await;
            tracing::info!(%owner, "keys.status.gone");
        }
        Err(e) => tracing::warn!(%owner, error = %e, "keys.status.failed"),
    }
}

/// The file from a fresh GET, for the moment a pod is about to mount it: the watch is what keeps
/// the file current, but a pod's first sshd read must not depend on a stream that may be sitting
/// stale — every `Permission denied` the probe saw on 2026-09-10 was a pod started while this
/// node's watch had not delivered the owner's newest projection. A missing object writes nothing
/// (the workspace parks on `KeysNotReady` as before); a failed GET is a warning and the file as it
/// stands.
pub async fn converge_owner(ctx: &Ctx, owner: &str) {
    let api: Api<crd::OwnerKeys> = Api::all(ctx.client.clone());
    match api.get_opt(owner).await {
        Ok(Some(obj)) => converge(ctx, &api, obj).await,
        Ok(None) => {}
        Err(e) => tracing::warn!(%owner, error = %e, "keys.get.failed"),
    }
}

/// An owner whose projection is gone is an owner nobody may log in as. No status patch — the
/// object it would be written to is what just disappeared.
fn revoke(pool: &str, owner: &str) -> bool {
    match write_keys_file(pool, owner, "") {
        Ok(wrote) => wrote,
        Err(e) => {
            tracing::warn!(%owner, error = %e, "keys.revoke.failed");
            false
        }
    }
}

/// Blanks only, never removes: a pod on this node may hold the inode, and only the tick knows
/// which pods do.
async fn revoke_off_worker(pool: &str, owner: &str) {
    let (pool, owner) = (pool.to_string(), owner.to_string());
    let _ = tokio::task::spawn_blocking(move || revoke(&pool, &owner)).await;
}

/// Owners whose keys file some pod on this node mounts (`k8s::keys_volume`'s hostPath).
fn referenced_owners(pool: &str, pods: &[k8s_openapi::api::core::v1::Pod]) -> HashSet<String> {
    let prefix = format!("{}/", k8s::keys_root(pool));
    pods.iter()
        .flat_map(|p| p.spec.iter().flat_map(|s| s.volumes.iter().flatten()))
        .filter_map(|v| v.host_path.as_ref()?.path.strip_prefix(&prefix)?.split('/').next().map(str::to_string))
        .collect()
}

#[derive(Debug, Default, PartialEq)]
struct Sweep {
    checked: usize,
    rewritten: usize,
    removed: usize,
}

/// One tick's worth of stale owners, blocking; the caller runs it on the blocking pool.
///
/// `referenced` is `None` when this node's pods could not be listed: UNKNOWN, so nothing is
/// removed and every stale file is only blanked (keep-biased). A directory no `OwnerKeys` names
/// and no pod here mounts is removed outright — on 2026-09-14 ~310 of them were dead probe
/// owners, and blanking them forever was the whole per-minute cost. A pod that mounts one keeps
/// its (empty) file: removing it would strand the inode it holds and fail its next start.
// ponytail: bounded at 1000 per tick; the rest wait a minute. Raise if a node ever holds more.
fn sweep(pool: &str, live: &HashSet<String>, referenced: Option<&HashSet<String>>) -> Sweep {
    let mut out = Sweep::default();
    for owner in stale_owners(pool, live).into_iter().take(1000) {
        out.checked += 1;
        if referenced.is_some_and(|r| !r.contains(&owner)) {
            let dir = format!("{}/{owner}", k8s::keys_root(pool));
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => {
                    out.removed += 1;
                    tracing::info!(%owner, "keys.dir.removed");
                }
                Err(e) => tracing::warn!(%owner, error = %e, "keys.dir.remove_failed"),
            }
        } else if revoke(pool, &owner) {
            out.rewritten += 1;
        }
    }
    out
}

/// `f` on the blocking pool: the async task that awaits it stays free to poll kube connections.
async fn off_worker<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

/// Every `OwnerKeys` in the cluster, converged on every event and on a one-minute tick — the tick
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
        watcher(watched.clone(), crate::controller::watch_config()).default_backoff().boxed()
    })
    .await
}

type KeyEvents = futures::stream::BoxStream<'static, Result<kube::runtime::watcher::Event<crd::OwnerKeys>, kube::runtime::watcher::Error>>;

/// The watch is a nudge; the tick is what makes this correct. So a stream that ENDS is rebuilt,
/// never fatal: kube-runtime's backoff gives up after its elapsed limit, which a burst of
/// `too old resource version … Expired` reached on 2026-09-08. This loop used to `return` there,
/// and the resync tick died with it — keys stopped converging on every node for two hours,
/// `authorized_keys` went stale, and every default-image workspace started meanwhile parked at
/// `KeysNotReady`. Nothing about a watch is allowed to end this task.
async fn run_with<F>(ctx: Arc<Ctx>, api: Api<crd::OwnerKeys>, mut watch: F)
where
    F: FnMut() -> KeyEvents,
{
    use futures::StreamExt;
    use kube::runtime::watcher;
    let mut events = watch();
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(watcher::Event::Apply(obj) | watcher::Event::InitApply(obj))) => converge(&ctx, &api, obj).await,
                Some(Ok(watcher::Event::Delete(obj))) => revoke_off_worker(&ctx.pool, &obj.name_any()).await,
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
                    let started = std::time::Instant::now();
                    let pods: Api<k8s_openapi::api::core::v1::Pod> = Api::all(ctx.client.clone());
                    let lp = kube::api::ListParams::default().fields(&format!("spec.nodeName={}", ctx.node));
                    let referenced = match pods.list(&lp).await {
                        Ok(l) => Some(referenced_owners(&ctx.pool, &l.items)),
                        Err(e) => {
                            tracing::warn!(error = %e, "keys.pods.list_failed");
                            None
                        }
                    };
                    let pool = ctx.pool.clone();
                    if let Some(s) = off_worker(move || sweep(&pool, &live, referenced.as_ref())).await {
                        let ms = started.elapsed().as_millis() as u64;
                        tracing::info!(checked = s.checked, rewritten = s.rewritten, removed = s.removed, ms, "keys.sweep.done");
                    }
                    for obj in list.items {
                        converge(&ctx, &api, obj).await;
                    }
                }
                // A watch can go stale without ever ending: on 2026-09-09, 07:50–12:00, keys
                // stopped converging on two nodes while the stream sat alive and silent. The tick
                // replaces it unconditionally, so a stale watch costs a minute, never hours.
                events = watch();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The echo loop's fuel: a status that already says this generation and reason is NOT
    /// patched again. The file is still written (it is this node's), the API is not touched.
    #[tokio::test]
    async fn a_settled_status_is_not_patched_again() {
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, rec) = crate::testsupport::test_ctx(tmp.path(), "node-a", vec![]);
        let api: Api<crd::OwnerKeys> = Api::all(ctx.client.clone());
        let mut obj: crd::OwnerKeys = serde_json::from_value(serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "OwnerKeys",
            "metadata": {"name": "acme"},
            "spec": {"generation": 7, "authorizedKeys": "ssh-ed25519 AAAA a\n"},
            "status": {"observedGeneration": 7, "conditions": [{
                "type": crd::KEYS_SYNCED, "status": "True", "reason": "Applied", "message": "m",
                "observedGeneration": 7, "lastTransitionTime": "2026-09-11T00:00:00Z"}]}
        }))
        .unwrap();
        converge(&ctx, &api, obj.clone()).await;
        assert!(rec.calls().iter().all(|c| !c.contains("PATCH")), "settled: no status write, got {:?}", rec.calls());
        assert_eq!(std::fs::read_to_string(kloudlite_workspaces::k8s::keys_file(&ctx.pool, "acme")).unwrap(), "ssh-ed25519 AAAA a\n");
        // A new generation is a real change and is written.
        obj.spec.generation = 8;
        converge(&ctx, &api, obj).await;
        assert!(rec.calls().iter().any(|c| c.contains("PATCH")), "a new generation is patched, got {:?}", rec.calls());
    }

    /// The 2026-09-08 outage in one test: the watch stream ends, and the loop must build a new one
    /// instead of returning and taking the resync tick down with it.
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

    /// The 2026-09-09 outage in one test: a stream that never ends and never yields — the shape a
    /// silently stale watch has — must still be replaced, by the tick.
    #[tokio::test(start_paused = true)]
    async fn the_tick_rebuilds_a_watch_that_never_ends() {
        use futures::StreamExt;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, _rec) = crate::testsupport::test_ctx(tmp.path(), "node-a", vec![]);
        let api: Api<crd::OwnerKeys> = Api::all(ctx.client.clone());
        let built = Arc::new(AtomicUsize::new(0));
        let seen = built.clone();
        let watch = move || {
            seen.fetch_add(1, Ordering::SeqCst);
            futures::stream::pending().boxed()
        };
        // The stream is `pending()`: it never ends, so the `None` arm can never run and every
        // rebuild past the first is the tick's. (The interval fires once at zero as well.)
        let _ = tokio::time::timeout(std::time::Duration::from_secs(700), run_with(ctx, api, watch)).await;
        assert!(built.load(Ordering::SeqCst) > 1, "the tick must replace a stream that never ended");
    }

    /// A status write on an object that has been deleted is a 404, and the only right reading of
    /// it is that nobody may log in as that owner any more — the revoke the Delete event would
    /// have done. It must not be a warning, and it must not leave the file standing.
    #[tokio::test]
    async fn a_status_write_on_a_deleted_object_revokes() {
        let tmp = tempfile::tempdir().unwrap();
        // No PATCH route: the mock answers an unrouted call with a 404 `Status`, which is exactly
        // what the API server sends for the status subresource of an object that is gone.
        let (ctx, _rec) = crate::testsupport::test_ctx(tmp.path(), "node-a", vec![]);
        let api: Api<crd::OwnerKeys> = Api::all(ctx.client.clone());
        let obj: crd::OwnerKeys = serde_json::from_value(serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "OwnerKeys",
            "metadata": {"name": "acme"},
            "spec": {"generation": 7i64, "authorizedKeys": "ssh-ed25519 AAAA a\n"},
        }))
        .unwrap();
        converge(&ctx, &api, obj).await;
        let path = kloudlite_workspaces::k8s::keys_file(&ctx.pool, "acme");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "", "the object is gone: nobody is admitted");
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

    /// Already right: no truncate, no fsync (mtime untouched). A change is written in place.
    #[test]
    fn an_already_correct_file_is_not_rewritten_and_a_change_keeps_the_inode() {
        use std::os::unix::fs::MetadataExt;
        let pool = tempfile::tempdir().unwrap();
        let pool = pool.path().to_str().unwrap();
        assert!(write_keys_file(pool, "acme", "ssh-ed25519 AAAA a\n").unwrap());
        let path = k8s::keys_file(pool, "acme");
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(old).unwrap();
        let ino = std::fs::metadata(&path).unwrap().ino();
        assert!(!write_keys_file(pool, "acme", "ssh-ed25519 AAAA a\n").unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), old);
        assert!(write_keys_file(pool, "acme", "ssh-ed25519 BBBB b\n").unwrap());
        assert_ne!(std::fs::metadata(&path).unwrap().modified().unwrap(), old);
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), ino);
        assert!(revoke(pool, "acme"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), ino, "revoke blanks in place");
        assert!(!revoke(pool, "acme"), "a blank file is not blanked again");
    }

    /// Stale dirs go only when nothing names them: no `OwnerKeys` AND no pod here mounts them. A
    /// referenced one keeps its (blanked) file; an unknown pod list removes nothing.
    #[test]
    fn a_stale_dir_is_removed_only_when_no_pod_mounts_it() {
        let pool = tempfile::tempdir().unwrap();
        let pool = pool.path().to_str().unwrap();
        for owner in ["alice", "gone", "held"] {
            write_keys_file(pool, owner, "ssh-ed25519 AAAA a\n").unwrap();
        }
        let live = HashSet::from(["alice".to_string()]);
        let dir = |o: &str| std::path::Path::new(&k8s::keys_root(pool)).join(o);
        // Unknown pods: blank both stale ones, remove nothing.
        assert_eq!(sweep(pool, &live, None), Sweep { checked: 2, rewritten: 2, removed: 0 });
        assert!(dir("gone").exists() && dir("held").exists());
        let pod: k8s_openapi::api::core::v1::Pod = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "p"},
            "spec": {"containers": [], "volumes": [{"name": "authorized-keys",
                "hostPath": {"path": k8s::keys_file(pool, "held"), "type": "File"}}]}
        }))
        .unwrap();
        let refs = referenced_owners(pool, &[pod]);
        assert_eq!(refs, HashSet::from(["held".to_string()]));
        assert_eq!(sweep(pool, &live, Some(&refs)), Sweep { checked: 2, rewritten: 0, removed: 1 });
        assert!(!dir("gone").exists());
        assert_eq!(std::fs::read_to_string(k8s::keys_file(pool, "held")).unwrap(), "");
        assert!(!std::fs::read_to_string(k8s::keys_file(pool, "alice")).unwrap().is_empty());
    }

    /// The stall itself: on a ONE-thread runtime, a slow sweep must not starve other tasks. A timer
    /// due in 20 ms fires while a 400 ms blocking sweep is still running.
    #[tokio::test(flavor = "current_thread")]
    async fn a_slow_sweep_does_not_hold_the_runtime() {
        let t0 = std::time::Instant::now();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            t0.elapsed()
        });
        let slow = off_worker(|| std::thread::sleep(std::time::Duration::from_millis(400)));
        let (fired, done) = tokio::join!(timer, async {
            slow.await;
            t0.elapsed()
        });
        let fired = fired.unwrap();
        assert!(fired < std::time::Duration::from_millis(200) && fired < done, "timer {fired:?}, sweep {done:?}");
    }

    /// No `keys/` directory yet (a node that has never converged) is not an error.
    #[test]
    fn a_pool_with_no_keys_directory_has_nothing_stale() {
        let pool = tempfile::tempdir().unwrap();
        assert!(stale_owners(pool.path().to_str().unwrap(), &HashSet::new()).is_empty());
    }

    /// The pod-start path: the file comes from a GET, not from whatever the watch last delivered.
    #[tokio::test]
    async fn converge_owner_writes_the_file_from_a_fresh_get() {
        use kloudlite_workspaces::kube_test::{get, patch};
        let tmp = tempfile::tempdir().unwrap();
        let obj = serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "OwnerKeys", "metadata": {"name": "alice"},
            "spec": {"generation": 7, "authorizedKeys": "ssh-ed25519 AAAA alice\n"}
        });
        let (ctx, rec) = crate::testsupport::test_ctx(
            tmp.path(),
            "node-a",
            vec![
                get("/apis/kloudlite.io/v1alpha1/ownerkeys/alice", obj.clone()),
                patch("/apis/kloudlite.io/v1alpha1/ownerkeys/alice/status", obj),
            ],
        );
        converge_owner(&ctx, "alice").await;
        let file = std::fs::read_to_string(k8s::keys_file(&ctx.pool, "alice")).unwrap();
        assert_eq!(file, "ssh-ed25519 AAAA alice\n");
        assert!(rec.calls().iter().any(|c| c.starts_with("GET ")), "{:?}", rec.calls());
        // No object: nothing written, so the workspace keeps parking on `KeysNotReady`.
        let (ctx, _) = crate::testsupport::test_ctx(tmp.path(), "node-b", vec![]);
        converge_owner(&ctx, "nobody").await;
        assert!(!std::path::Path::new(&k8s::keys_file(&ctx.pool, "nobody")).exists());
    }
}
