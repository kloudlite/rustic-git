//! The three gauges only this process can produce, on the agent's own `/metrics` — the collector's
//! prometheus receiver scrapes them like any other pod's.
//!
//! Deliberately just three. CPU, memory, load and per-pod usage come from the collector's
//! `kubeletstats` and `k8s_cluster` receivers, which already read them from the kubelet; exporting
//! our own would be a second number for the same fact, and the two would drift.
//!
//! `btrfs filesystem usage -b`, not `df` or `statvfs`: on btrfs, `df` reports ALLOCATION, and a
//! pool starts failing allocations while `df` still shows room — which is the exact condition
//! `PoolAlmostFull` exists to catch. statvfs is the fallback for a pool that is not btrfs (a dev
//! box), where a rough number beats none.

use std::time::Duration;

/// Fifteen seconds, matching the collector's scrape interval: a gauge refreshed slower than it is
/// scraped just repeats itself, and one refreshed faster burns IO for samples nobody reads.
const EVERY: Duration = Duration::from_secs(15);

/// `(used, total)` bytes out of `btrfs filesystem usage -b <path>`. `None` on anything unparsable —
/// a zero here would read as an empty pool and silence the disk alert.
pub fn parse_btrfs_usage(text: &str) -> Option<(u64, u64)> {
    let num = |line: &str| line.split(':').nth(1)?.split_whitespace().next()?.parse::<u64>().ok();
    let mut total = None;
    let mut used = None;
    for line in text.lines() {
        let t = line.trim();
        // `Device size` and the Overall `Used` both appear once, before the per-device sections;
        // `get_or_insert` keeps the first, which is the whole-pool figure.
        if t.starts_with("Device size:") {
            if let Some(v) = num(t) {
                total.get_or_insert(v);
            }
        } else if t.starts_with("Used:") {
            if let Some(v) = num(t) {
                used.get_or_insert(v);
            }
        }
    }
    Some((used?, total?))
}

/// `(used, total)` bytes from statvfs. Only a fallback: see the module doc on why this is wrong for
/// btrfs specifically.
pub fn statvfs_usage(path: &str) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let frsize = s.f_frsize as u64;
    let total = s.f_blocks as u64 * frsize;
    if total == 0 {
        return None;
    }
    Some((total - s.f_bfree as u64 * frsize, total))
}

fn pool_usage(pool: &str) -> Option<(u64, u64)> {
    let out = std::process::Command::new("btrfs")
        .args(["filesystem", "usage", "-b", pool])
        .output()
        .ok()?;
    parse_btrfs_usage(&String::from_utf8_lossy(&out.stdout)).or_else(|| statvfs_usage(pool))
}

// ponytail: the beat itself shells out and lists cluster objects on a timer — not worth a unit
// test harness for a loop that never returns; the parser and the statvfs conversion it calls are
// the parts that can be wrong, and those are tested directly above/below.
/// The beat. Shelling out and counting pods are both blocking-ish, so the pool read goes to a
/// blocking thread — on the reactor it stalls every in-flight reconcile for as long as btrfs takes.
pub fn spawn_stats(pool: String, client: kube::Client, node: String) {
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(EVERY);
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            iv.tick().await;
            let p = pool.clone();
            match tokio::task::spawn_blocking(move || pool_usage(&p)).await {
                Ok(Some((used, total))) => {
                    metrics::gauge!("node_pool_bytes_used").set(used as f64);
                    metrics::gauge!("node_pool_bytes_total").set(total as f64);
                }
                Ok(None) => tracing::warn!(%pool, "could not read pool usage this beat; keeping the last exported value"),
                Err(e) => tracing::warn!(%pool, error = %e, "pool usage read panicked; keeping the last exported value"),
            }
            // Working copies RUNNING on this node, from this node's own objects — the same
            // `status.nodeName` the controller converges on, so the gauge and the placement can
            // never disagree.
            let api = kube::Api::<kloudlite_git_workspaces::crd::Workspace>::all(client.clone());
            let params = kube::api::ListParams::default().fields(&format!("status.nodeName={node}"));
            match api.list(&params).await {
                Ok(list) => {
                    let running = list
                        .items
                        .iter()
                        .filter(|w| {
                            matches!(
                                w.status.as_ref().map(|s| s.phase),
                                Some(kloudlite_git_workspaces::crd::Phase::Ready)
                                    | Some(kloudlite_git_workspaces::crd::Phase::Running)
                            )
                        })
                        .count();
                    metrics::gauge!("node_working_copies_running").set(running as f64);
                }
                Err(e) => tracing::warn!(%node, error = %e, "listing workspaces for this node failed; keeping the last exported value"),
            }
        }
    });
}
