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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Separate from `crd::AGENT_FIELD_MANAGER` on purpose — see the module doc.
const USAGE_FIELD_MANAGER: &str = "kloudlite-agent-usage";

/// Below this, the number has not meaningfully moved and a write would only cost the fleet a watch
/// event apiece.
const STAMP_DELTA_BYTES: u64 = 1 << 20;

/// `(referenced, exclusive)` out of `btrfs qgroup show -f -re --raw <path>`: the first row whose
/// first column is a `level/id` qgroup id. `None` on anything unparsable — a zero here would read
/// as an empty volume and hand out allocation nobody has.
///
/// A `referenced` of ZERO is treated as unparsable for that same reason. It is not a size a live
/// subvolume can have — the metadata alone is 16 KiB — and it is exactly what btrfs reports after
/// its accounting is invalidated (`WARNING: qgroup data inconsistent, rescan recommended`, on
/// stderr, exit 0). Two of the fleet's three nodes were in that state and every volume on them
/// stamped as empty, which is how a workspace holding 200 MB read back 16384 bytes (2026-09-18).
///
/// `None` means UNKNOWN, and the caller keeps the last good stamp rather than publishing a zero.
pub fn parse_qgroup(text: &str) -> Option<(u64, u64)> {
    text.lines().find_map(|line| {
        let mut f = line.split_whitespace();
        let id = f.next()?;
        let (lvl, qid) = id.split_once('/')?;
        lvl.parse::<u64>().ok()?;
        qid.parse::<u64>().ok()?;
        let (referenced, exclusive) = (f.next()?.parse::<u64>().ok()?, f.next()?.parse().ok()?);
        (referenced > 0).then_some((referenced, exclusive))
    })
}

fn qgroup_of(path: &Path) -> Option<(u64, u64)> {
    let out = std::process::Command::new("btrfs")
        .args(["qgroup", "show", "-f", "-re", "--raw"])
        .arg(path)
        .output()
        .ok()?;
    let row = out.status.success().then(|| parse_qgroup(&String::from_utf8_lossy(&out.stdout))).flatten();
    if row.is_none() {
        // Said once per unreadable subvolume per pass, with btrfs's own words.
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.lines().find(|l| !l.trim().is_empty()).unwrap_or("no measurement in the output");
        tracing::warn!(path = %path.display(), reason = %why.trim(), "usage.unreadable");
        if why.contains("rescan recommended") {
            rescan(path);
        }
    }
    row
}

/// Unix seconds of the last rescan this agent started; 0 = never. One pool per agent, so this is
/// per pool.
static RESCAN_AT: AtomicU64 = AtomicU64::new(0);

/// A rescan walks the whole filesystem; one per this window is plenty for a counter nobody enforces.
const RESCAN_FLOOR_SECS: u64 = 600;

/// Starts a background `btrfs quota rescan` (no `-w`: the kernel runs it and refuses a second
/// while one is in flight). Leaving it to an operator meant nobody ran it: session-0 logged
/// `usage.unreadable` 165 times in five hours and `vol.usage.stamped` failed the hourly
/// (2026-09-23). Every subvolume delete can invalidate the accounting again, so this is not a
/// one-off repair but the agent's own upkeep.
fn rescan(path: &Path) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let last = RESCAN_AT.load(Ordering::Relaxed);
    if now.saturating_sub(last) < RESCAN_FLOOR_SECS
        || RESCAN_AT.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err()
    {
        return;
    }
    match std::process::Command::new("btrfs").args(["quota", "rescan"]).arg(path).output() {
        Ok(o) if o.status.success() => tracing::info!(path = %path.display(), "usage.rescan.started"),
        Ok(o) => tracing::warn!(path = %path.display(), error = %String::from_utf8_lossy(&o.stderr).trim(), "usage.rescan.refused"),
        Err(e) => tracing::warn!(path = %path.display(), error = %e, "usage.rescan.refused"),
    }
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
    volume_usage_with(pool_root, id, qgroup_of)
}

fn volume_usage_with(
    pool_root: &Path,
    id: &str,
    mut measure: impl FnMut(&Path) -> Option<(u64, u64)>,
) -> Option<u64> {
    let voldir = pool_root.join("vol").join(id);
    let mut rows = Vec::new();
    for sub in ["live", "snap"] {
        let rd = match std::fs::read_dir(voldir.join(sub)) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        for entry in rd {
            let entry = entry.ok()?;
            if entry.file_name().to_str().is_some_and(|n| n.starts_with('.')) {
                continue;
            }
            rows.push(measure(&entry.path())?);
        }
    }
    aggregate_usage(&rows)
}

fn aggregate_usage(rows: &[(u64, u64)]) -> Option<u64> {
    let (pos, &(biggest, _)) = rows
        .iter()
        .enumerate()
        .max_by(|(_, (r1, e1)), (_, (r2, e2))| r1.cmp(r2).then_with(|| e2.cmp(e1)))?;
    rows.iter().enumerate().filter(|(i, _)| *i != pos)
        .try_fold(biggest, |total, (_, (_, exclusive))| total.checked_add(*exclusive))
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

    /// btrfs prints this on STDERR and still exits 0, with every counter reading zero, after an
    /// operation that invalidated its accounting — a subvolume delete is the common one. Two of
    /// the fleet's three nodes were in this state, so every volume on them stamped as empty and
    /// `vol.usage.stamped` read 16384 bytes for a workspace holding 200 MB (2026-09-18).
    const INCONSISTENT: &str = "0/58177              0        16384     21474836480           none   vol/x/live/x\n";

    /// A qgroup that has not been rescanned reports ZERO, and zero is the one answer that must
    /// never be published: it reads as an empty volume, which is what hands out allocation nobody
    /// has. `None` keeps the last good stamp instead.
    #[test]
    fn a_zero_referenced_row_is_unknown_rather_than_empty() {
        assert_eq!(parse_qgroup(INCONSISTENT), None, "a zero reading must not pass as a measurement");
        // A real measurement still parses, including a genuinely small one.
        assert_eq!(parse_qgroup(SHOW), Some((2147483648, 1048576)));
        assert_eq!(
            parse_qgroup("0/52501        7733248        36864     53687091200           none   vol/b/live/b\n"),
            Some((7733248, 36864))
        );
    }

    #[test]
    fn the_first_qgroup_row_is_the_subvolumes_own() {
        assert_eq!(parse_qgroup(SHOW), Some((2147483648, 1048576)));
        assert_eq!(parse_qgroup("ERROR: quotas not enabled\n"), None);
        assert_eq!(parse_qgroup(""), None);
    }

    #[test]
    fn incomplete_measurements_never_publish_partial_usage() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("vol/v/live");
        std::fs::create_dir_all(live.join("small")).unwrap();
        std::fs::create_dir_all(live.join("large")).unwrap();
        let read = |path: &Path| (path.file_name().unwrap() == "small").then_some((16384, 16384));
        assert_eq!(volume_usage_with(tmp.path(), "v", read), None);
        assert_eq!(volume_usage_with(tmp.path(), "v", |_| Some((16384, 8192))), Some(24576));
        std::fs::write(tmp.path().join("vol/v/snap"), "not a directory").unwrap();
        assert_eq!(volume_usage_with(tmp.path(), "v", |_| Some((16384, 8192))), None);
    }

    #[test]
    fn empty_and_overflowing_usage_are_unknown() {
        assert_eq!(aggregate_usage(&[]), None);
        assert_eq!(aggregate_usage(&[(u64::MAX, 0), (1, 1)]), None);
        assert_eq!(aggregate_usage(&[(100, 50), (200, 60), (100, 20)]), Some(270));
        assert_eq!(aggregate_usage(&[(100, 50), (100, 20)]), Some(150));
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
    async fn an_unknown_volume_read_does_not_refresh_its_stamp() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        read_and_stamp(&ctx, "vol-1").await;
        assert!(rec.calls().is_empty(), "unknown local usage must not read or refresh status: {:?}", rec.calls());
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
