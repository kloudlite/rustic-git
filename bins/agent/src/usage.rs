//! What a volume OCCUPIES, stamped on `Volume.status.usedBytes` by the node that holds it.
//!
//! Disk quota used to be charged from `spec.quotaGb` — the sum of declared per-volume ceilings —
//! so an account holding a few gigabytes was refused at "80 of 100 in use" (owner ruling
//! 2026-09-17). Ceilings are not reservations: the number a person is charged for has to come off
//! the filesystem, and btrfs already keeps it per subvolume in the qgroups the quota limit uses.
//!
//! Read, never computed: no `du`, which walks every inode (the reason `snapshot.rs` carried a
//! ponytail marker against a `sizeBytes` for years). `btrfs qgroup show -f -re --raw <subvol>` is
//! one syscall-ish read of counters btrfs maintains anyway.
//!
//! Written under its OWN field manager: `controller::patch_status` applies FORCED under
//! `AGENT_FIELD_MANAGER` and server-side apply PRUNES fields that manager owns and no longer
//! sets — stamping usage under it would make the volume reconciler's very next pass delete the
//! stamp. And written only when the number moved by a megabyte, and only from a path that had
//! already read the disk anyway (a cut, a snapshot delete): disk is NEVER enforced from this
//! number and must therefore cost nothing to observe — no beat of its own, no refresh for an idle
//! volume, no write that only restates what the fleet's reflectors already hold (the 2026-09-11
//! echo storm).

use crate::controller::Ctx;
use kloudlite_workspaces::crd;
use kube::api::{Api, Patch, PatchParams};
use std::path::Path;
use std::sync::Arc;

/// Separate from `crd::AGENT_FIELD_MANAGER` on purpose — see the module doc.
const USAGE_FIELD_MANAGER: &str = "kloudlite-agent-usage";

/// Below this, the number has not meaningfully moved and a write would only cost the fleet a watch
/// event apiece.
const STAMP_DELTA_BYTES: u64 = 1 << 20;

/// `(referenced, exclusive)` out of `btrfs qgroup show -f -re --raw <path>`: the first row whose
/// first column is a `level/id` qgroup id. `None` on anything unparsable — a zero here would read
/// as an empty volume and hand out allocation nobody has.
pub fn parse_qgroup(text: &str) -> Option<(u64, u64)> {
    text.lines().find_map(|line| {
        let mut f = line.split_whitespace();
        let id = f.next()?;
        let (lvl, qid) = id.split_once('/')?;
        lvl.parse::<u64>().ok()?;
        qid.parse::<u64>().ok()?;
        Some((f.next()?.parse().ok()?, f.next()?.parse().ok()?))
    })
}

fn qgroup_of(path: &Path) -> Option<(u64, u64)> {
    let out = std::process::Command::new("btrfs")
        .args(["qgroup", "show", "-f", "-re", "--raw"])
        .arg(path)
        .output()
        .ok()?;
    parse_qgroup(&String::from_utf8_lossy(&out.stdout))
}

/// What the volume occupies: the largest `referenced` among its subvolumes (live worktrees and
/// snapshots), plus every other subvolume's `exclusive`.
///
/// ponytail: an approximation of the level-1 qgroup this pool does not create — the biggest tree's
/// full size plus only what each sibling holds that nothing else does. Exact would be
/// `btrfs qgroup create 1/N` per volume and assigning every subvolume to it at create, checkout and
/// snapshot time; do that if the number is ever billed rather than merely capped. Blocking: shells
/// out once per subvolume, so callers run it on a blocking thread.
pub fn volume_usage(pool_root: &Path, id: &str) -> Option<u64> {
    let voldir = pool_root.join("vol").join(id);
    let mut rows: Vec<(u64, u64)> = Vec::new();
    for sub in ["live", "snap"] {
        let Ok(rd) = std::fs::read_dir(voldir.join(sub)) else { continue };
        for e in rd.flatten() {
            if e.file_name().to_str().is_some_and(|n| n.starts_with('.')) {
                continue;
            }
            if let Some(row) = qgroup_of(&e.path()) {
                rows.push(row);
            }
        }
    }
    // A volume whose subvolumes could not be read at all is UNKNOWN, not empty: stamp nothing and
    // let the previous reading stand.
    let biggest = rows.iter().map(|(r, _)| *r).max()?;
    let pos = rows.iter().position(|(r, _)| *r == biggest).expect("the max came from this list");
    Some(biggest + rows.iter().enumerate().filter(|(i, _)| *i != pos).map(|(_, (_, x))| *x).sum::<u64>())
}

/// The whole no-echo-storm rule, pure so it has a test: write only when the number moved by a
/// megabyte, or when nothing has been stamped at all. Deliberately NOT time-based — a stamp that
/// still says what the last one said teaches no reader anything, and `usedAt` is what says how old
/// the reading is.
pub fn should_stamp(current: u64, status: Option<&crd::VolumeStatus>) -> bool {
    match status.and_then(|st| st.used_bytes) {
        Some(prev) => current.abs_diff(prev) >= STAMP_DELTA_BYTES,
        None => true,
    }
}

/// Stamp `used` on the volume, unless `should_stamp` says the fleet would learn nothing from it.
/// Best-effort by construction: usage is an observed fact refreshed on a beat, so a failed write
/// costs one beat of freshness and nothing else.
pub async fn stamp_usage(ctx: &Arc<Ctx>, id: &str, used: u64) {
    let api: Api<crd::Volume> = Api::all(ctx.client.clone());
    let cur = match api.get_opt(id).await {
        Ok(Some(v)) => v,
        // Gone, or a view we could not read — either way, nothing to stamp.
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(volume = %id, error = %e, "usage.read.failed");
            return;
        }
    };
    if !should_stamp(used, cur.status.as_ref()) {
        return;
    }
    let now = chrono::Utc::now();
    let body = serde_json::json!({
        "apiVersion": format!("{}/{}", crd::GROUP, crd::VERSION),
        "kind": "Volume",
        "status": {"usedBytes": used, "usedAt": now.to_rfc3339()},
    });
    match api
        .patch_status(id, &PatchParams::apply(USAGE_FIELD_MANAGER).force(), &Patch::Apply(&body))
        .await
    {
        Ok(_) => tracing::debug!(volume = %id, used_bytes = used, "usage.stamped"),
        Err(e) => tracing::warn!(volume = %id, error = %e, "usage.stamp.failed"),
    }
}

/// Read this node's copy of the volume and stamp it. The two halves callers always want together;
/// the btrfs read goes to a blocking thread because it shells out once per subvolume.
pub async fn read_and_stamp(ctx: &Arc<Ctx>, id: &str) {
    let (root, vol) = (ctx.engine.pool.root.clone(), id.to_string());
    match tokio::task::spawn_blocking(move || volume_usage(&root, &vol)).await {
        Ok(Some(used)) => stamp_usage(ctx, id, used).await,
        Ok(None) => {}
        Err(e) => tracing::warn!(volume = %id, error = %e, "usage.read.panicked"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::test_ctx;
    use kloudlite_workspaces::kube_test::Route;

    const SHOW: &str = "qgroupid         rfer         excl \n--------         ----         ---- \n0/257      2147483648      1048576 \n";

    #[test]
    fn the_first_qgroup_row_is_the_subvolumes_own() {
        assert_eq!(parse_qgroup(SHOW), Some((2147483648, 1048576)));
        assert_eq!(parse_qgroup("ERROR: quotas not enabled\n"), None);
        assert_eq!(parse_qgroup(""), None);
    }

    fn status(used: Option<u64>, at: Option<&str>) -> crd::VolumeStatus {
        crd::VolumeStatus { used_bytes: used, used_at: at.map(str::to_string), ..Default::default() }
    }

    /// The echo-storm rule (2026-09-11), and the owner's ruling that an unenforced number must
    /// cost nothing: a reading that has not moved by a megabyte is never written, however old the
    /// stamp is.
    #[test]
    fn a_stamp_that_would_teach_nobody_anything_is_not_written() {
        let ancient = "2020-01-01T00:00:00Z";
        assert!(!should_stamp(1_000_000_000, Some(&status(Some(1_000_000_000), Some(ancient)))), "age alone is not a reason");
        assert!(!should_stamp(1_000_000_000, Some(&status(Some(1_000_500_000), Some(ancient)))), "half a megabyte is noise");
        assert!(should_stamp(1_002_000_000, Some(&status(Some(1_000_000_000), Some(ancient)))));
        assert!(should_stamp(1_000_000_000, Some(&status(None, None))), "never stamped");
        assert!(should_stamp(1_000_000_000, None), "no status at all");
    }

    fn volume_route(status: serde_json::Value) -> Route {
        Route {
            method: "GET",
            path: "/apis/kloudlite.io/v1alpha1/volumes/vol-1".into(),
            status: 200,
            body: serde_json::json!({
                "apiVersion": "kloudlite.io/v1alpha1",
                "kind": "Volume",
                "metadata": {"name": "vol-1"},
                "spec": {"owner": "alice", "quotaGb": 10, "nodeName": "node-a", "region": "r1", "replicas": 1},
                "status": status,
            }),
        }
    }

    /// The fake qgroup reader is the caller: `stamp_usage` takes the number, so a test says what
    /// btrfs would have said without one.
    #[tokio::test]
    async fn an_unchanged_volume_is_read_and_not_written() {
        let tmp = tempfile::tempdir().unwrap();
        let at = "2026-09-17T00:00:00Z";
        let routes = vec![volume_route(serde_json::json!({"phase": "ready", "usedBytes": 5_000_000_000u64, "usedAt": at}))];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        stamp_usage(&ctx, "vol-1", 5_000_100_000).await;
        assert_eq!(rec.calls(), vec!["GET /apis/kloudlite.io/v1alpha1/volumes/vol-1".to_string()]);
    }

    #[tokio::test]
    async fn a_moved_volume_is_stamped() {
        let tmp = tempfile::tempdir().unwrap();
        let at = "2026-09-17T00:00:00Z";
        let routes = vec![
            volume_route(serde_json::json!({"phase": "ready", "usedBytes": 5_000_000_000u64, "usedAt": at})),
            Route {
                method: "PATCH",
                path: "/apis/kloudlite.io/v1alpha1/volumes/vol-1/status".into(),
                status: 200,
                body: serde_json::json!({}),
            },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        stamp_usage(&ctx, "vol-1", 9_000_000_000).await;
        let sent = rec.sent("PATCH", "/apis/kloudlite.io/v1alpha1/volumes/vol-1/status");
        assert_eq!(sent.len(), 1, "{:?}", rec.calls());
        assert_eq!(sent[0]["status"]["usedBytes"], 9_000_000_000u64);
        assert!(sent[0]["status"]["usedAt"].is_string());
        // Status only, and nothing of the spec: the whole apply is the two observed fields.
        assert!(sent[0].get("spec").is_none());
    }
}
