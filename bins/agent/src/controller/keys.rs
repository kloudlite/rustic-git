//! `OwnerKeys` → the file sshd reads. The projection is the api's; this node's whole job is to
//! make the bytes on disk equal the spec and say so in status.

use super::{status::patch_status, Ctx};
use kloudlite_workspaces::{crd, k8s};
use kube::{Api, ResourceExt};
use std::sync::Arc;

/// In place, never by rename: pods hold the inode through their hostPath mount.
pub fn write_keys_file(pool: &str, owner: &str, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = k8s::keys_file(pool, owner);
    std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap())?;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    // `mode` on the open only applies to a file this call CREATED; an existing one keeps whatever
    // it had, so the mode is restated unconditionally.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    // sshd reads the file as the login user (`kl`), through StrictModes off; the uid is what lets
    // the read succeed at all.
    std::os::unix::fs::chown(&path, Some(k8s::SSH_UID as u32), Some(k8s::SSH_UID as u32))?;
    Ok(())
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
    let status = serde_json::json!({
        "observedGeneration": ok.then_some(generation),
        "conditions": [crd::condition_since(prev, crd::KEYS_SYNCED, ok, reason, msg, generation)],
    });
    if let Err(e) = patch_status(api, &owner, "OwnerKeys", status).await {
        tracing::warn!(%owner, error = %e, "keys.status.failed");
    }
}

/// Every `OwnerKeys` in the cluster, converged on every event and on a ten-minute tick — the tick
/// is what heals a file deleted by hand or a write that failed transiently.
pub async fn run(ctx: Arc<Ctx>) {
    use futures::StreamExt;
    use kube::runtime::{watcher, WatchStreamExt};
    let api: Api<crd::OwnerKeys> = Api::all(ctx.client.clone());
    let mut events =
        std::pin::pin!(watcher(api.clone(), watcher::Config::default()).default_backoff().applied_objects());
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(600));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(obj)) => converge(&ctx, &api, obj).await,
                Some(Err(e)) => tracing::warn!(error = %e, "keys.watch.failed"),
                None => return,
            },
            _ = tick.tick() => {
                if let Ok(list) = api.list(&Default::default()).await {
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
}
