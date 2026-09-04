//! The hourly folds: one `usage_hourly` row per owner per dimension, one `fleet_hourly` row per
//! region.
//!
//! BOTH RECOMPUTE FROM THE CRDs EVERY RUN. Nothing here reads an earlier row and adds to it — a
//! stored counter can only be wrong in the direction that hands out allocation nobody has
//! (CLAUDE.md), and the same argument applies to a chart that would then show it.
//!
//! The row builders are pure and the loop around them is thin, so the shape is testable without a
//! cluster; `run_beats` is the only part that needs one.

use crate::api::admin::{clusters, owners};
use chrono::Timelike;
use crate::api::ApiState;
use crate::crd::QuotaSpec;
use crate::quota::{Dim, Usage};
use std::sync::Arc;

/// ClickHouse `DateTime` over HTTP: seconds, space-separated, no zone.
const TS_FMT: &str = "%Y-%m-%d %H:%M:%S";

const HOUR: std::time::Duration = std::time::Duration::from_secs(3600);

pub struct UsageInput {
    pub owner: String,
    pub is_team: bool,
    pub used: Usage,
    pub limit: QuotaSpec,
}

pub struct FleetInput {
    pub region: String,
    pub nodes_total: u32,
    pub nodes_ready: u32,
    pub agents_ready: u32,
    pub live_workspaces: u32,
    pub live_environments: u32,
    pub snapshots: u32,
    pub disk_gb: u64,
    pub cpu: u32,
    pub memory_gb: u32,
    pub pool_used_bytes: u64,
    pub pool_total_bytes: u64,
}

/// `(dimension word, used, limit)` for one owner. The words are `Dim::word`'s, which the 409
/// message and the request form already use — a second vocabulary here would split every chart.
fn dimensions(u: &UsageInput) -> [(&'static str, f64, f64); 6] {
    [
        (Dim::Workspaces.word(), u.used.workspaces as f64, u.limit.workspaces as f64),
        (Dim::Environments.word(), u.used.environments as f64, u.limit.environments as f64),
        (Dim::Snapshots.word(), u.used.snapshots as f64, u.limit.snapshots as f64),
        (Dim::DiskGb.word(), u.used.disk_gb as f64, u.limit.disk_gb as f64),
        (Dim::Cpu.word(), u.used.cpu as f64, u.limit.cpu as f64),
        (Dim::MemoryGb.word(), u.used.memory_gb as f64, u.limit.memory_gb as f64),
    ]
}

pub fn usage_rows(ts: chrono::DateTime<chrono::Utc>, owners: &[UsageInput]) -> Vec<serde_json::Value> {
    let ts = ts.format(TS_FMT).to_string();
    owners
        .iter()
        .flat_map(|u| {
            let (owner, is_team, ts) = (u.owner.clone(), u8::from(u.is_team), ts.clone());
            dimensions(u).into_iter().map(move |(dimension, used, limit)| {
                serde_json::json!({
                    "ts": ts, "owner": owner, "is_team": is_team,
                    "dimension": dimension, "used": used, "limit": limit,
                })
            })
        })
        .collect()
}

pub fn fleet_rows(ts: chrono::DateTime<chrono::Utc>, fleet: &[FleetInput]) -> Vec<serde_json::Value> {
    let ts = ts.format(TS_FMT).to_string();
    fleet
        .iter()
        .map(|f| {
            serde_json::json!({
                "ts": ts, "region": f.region,
                "nodes_total": f.nodes_total, "nodes_ready": f.nodes_ready,
                "agents_ready": f.agents_ready,
                "live_workspaces": f.live_workspaces, "live_environments": f.live_environments,
                "snapshots": f.snapshots,
                "disk_gb": f.disk_gb, "cpu": f.cpu, "memory_gb": f.memory_gb,
                "pool_used_bytes": f.pool_used_bytes, "pool_total_bytes": f.pool_total_bytes,
            })
        })
        .collect()
}

/// A live worktree by the same rule `admin::clusters`'s row uses (`status.phase != Stopped` plus,
/// for a workspace, an assigned pod) — restated here rather than exported, since it is a two-line
/// predicate and the alternative is a third module both sides import from.
fn live_workspace(w: &crate::crd::Workspace) -> bool {
    let Some(st) = w.status.as_ref() else { return false };
    st.phase != crate::crd::Phase::Stopped && st.pod_ref.is_some()
}

fn live_environment(e: &crate::crd::Environment) -> bool {
    e.status.as_ref().is_some_and(|st| st.phase != crate::crd::Phase::Stopped)
}

/// One region's fleet numbers, built from the SAME lists `owners::fleet` already listed for the
/// usage fold (`f.ws`/`f.envs`/`f.vols`/`f.snaps`) plus the per-region agent/node counts
/// `clusters::cluster_rows` already computes — never a second round trip to Kubernetes for either
/// half.
fn fleet_inputs(rows: Vec<clusters::ClusterRow>, f: &owners::Fleet) -> Vec<FleetInput> {
    rows.into_iter()
        .map(|r| {
            let region = r.region;
            let vols_here: Vec<&crate::crd::Volume> = f.vols.iter().filter(|v| v.spec.region == region).collect();
            let disk_gb = vols_here.iter().map(|v| v.spec.quota_gb).sum();
            let vol_names: std::collections::HashSet<String> =
                vols_here.iter().map(|v| kube::ResourceExt::name_any(*v)).collect();
            let snapshots =
                f.snaps.iter().filter(|s| s.is_snapshot() && vol_names.contains(&s.spec.volume)).count() as u32;

            let ws_here: Vec<&crate::crd::Workspace> = f.ws.iter().filter(|w| w.spec.region == region).collect();
            let envs_here: Vec<&crate::crd::Environment> = f.envs.iter().filter(|e| e.spec.region == region).collect();
            let live_workspaces = ws_here.iter().filter(|w| live_workspace(w)).count() as u32;
            let live_environments = envs_here.iter().filter(|e| live_environment(e)).count() as u32;

            let mut millis = 0u64;
            let mut mib = 0u64;
            for w in &ws_here {
                if w.spec.desired_state == crate::crd::DesiredState::Running {
                    millis += crate::quota::millicores(&w.spec.resources.cpu_limit);
                    mib += crate::quota::mebibytes(&w.spec.resources.memory_limit);
                }
            }
            let unit = crate::k8s::env_unit_resources();
            for e in &envs_here {
                if e.spec.desired_state == crate::crd::DesiredState::Running {
                    let n = e.spec.services.len() as u64;
                    millis += n * crate::quota::millicores(&unit.cpu_limit);
                    mib += n * crate::quota::mebibytes(&unit.memory_limit);
                }
            }

            FleetInput {
                region,
                nodes_total: r.nodes_total.max(0) as u32,
                nodes_ready: r.nodes_ready.max(0) as u32,
                agents_ready: r.agents_ready.max(0) as u32,
                live_workspaces,
                live_environments,
                snapshots,
                disk_gb,
                cpu: millis.div_ceil(1000) as u32,
                memory_gb: mib.div_ceil(1024) as u32,
                // ponytail: pool_used_bytes/pool_total_bytes are the agents' own gauge samples,
                // which land in ClickHouse once Task 7 (the OTel collector wiring) and Task 10 (the
                // agent's pool gauge) exist. Until then a fleet row still needs writing on schedule
                // — a zero here reads as "not yet reporting", never as a fabricated capacity number.
                pool_used_bytes: 0,
                pool_total_bytes: 0,
            }
        })
        .collect()
}

/// Sleeps until the next :00:00 UTC plus a few seconds of random jitter. `tokio::time::interval`
/// fires its first tick immediately on creation, so this is what actually aligns the beat — every
/// replica of the admin process boots at its own moment, but this makes them all converge on the
/// hour instead of each running its own hourly clock from boot time. The jitter then keeps them
/// from landing on the SAME instant: `ReplacingMergeTree` would collapse the resulting duplicate
/// row fine, but there is no reason to make ClickHouse absorb N simultaneous inserts on the dot.
async fn align_to_next_hour() {
    use rand::Rng;
    let secs_into_hour = (chrono::Utc::now().timestamp().rem_euclid(3600)) as u64;
    let jitter = rand::thread_rng().gen_range(0..30);
    tokio::time::sleep(std::time::Duration::from_secs(3600 - secs_into_hour + jitter)).await;
}

/// The hourly loop. Both folds are re-run from the cluster every hour; a failure logs and waits for
/// the next hour rather than retrying tightly, because the next run recomputes everything anyway.
pub async fn run_beats(state: Arc<ApiState>) {
    align_to_next_hour().await;
    let mut iv = tokio::time::interval(HOUR);
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // A missing ClickHouse is a boot-time configuration fact, not a per-tick failure: an operator
    // needs to hear it once, not every hour forever, so only the first skip is loud.
    let mut history_missing_warned = false;
    loop {
        iv.tick().await;
        if state.history.is_none() {
            if !history_missing_warned {
                tracing::warn!(reason = "no-clickhouse", "history.beats.skipped");
                history_missing_warned = true;
            } else {
                tracing::debug!(reason = "no-clickhouse", "history.beats.skipped");
            }
            continue;
        }
        tick_once(&state).await;
    }
}

/// One hour's worth of work: fold the CRDs and write both tables. Split out of `run_beats` so a
/// test can drive a single tick directly against a canned ClickHouse server and a mocked kube API,
/// instead of waiting on a real clock and a real cluster.
pub async fn tick_once(state: &Arc<ApiState>) {
    let Some(h) = state.history.as_deref() else { return };
    // Truncated to the hour, never `now()`: both tables are ReplacingMergeTrees whose sort key ends
    // in `ts`, so two admin replicas beating in one hour write rows identical on the whole key and
    // the duplicate folds away. A per-replica second would make them two rows the merge can never
    // fold, and every `max()` over the hour would then answer from whichever replica ran later.
    let ts = chrono::Utc::now()
        .with_minute(0)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or_else(chrono::Utc::now);

    let Some(client) = state.kube.as_ref() else {
        tracing::warn!(reason = "no-kube-client", "history.beats.skipped");
        return;
    };
    let f = match owners::fleet(client).await {
        Ok(f) => f,
        Err(_) => {
            tracing::warn!(reason = "owners-fold-failed", "history.beats.skipped");
            return;
        }
    };

    let mut usage = Vec::with_capacity(f.owners.len());
    for owner in &f.owners {
        let directory_says = crate::api::scope::is_team(state, owner).await;
        let is_team = owners::team_of(owner, directory_says);
        let own = f.quota_by_name.get(owner).cloned();
        let limit = own.unwrap_or_else(|| owners::fallback_quota(&f.quota_by_name, is_team));
        let used = f.usage_by_owner.get(owner).cloned().unwrap_or_default();
        usage.push(UsageInput { owner: owner.clone(), is_team, used, limit });
    }
    if let Err(e) = h.insert("usage_hourly", &usage_rows(ts, &usage)).await {
        tracing::warn!(table = "usage_hourly", error = %e, "history.write.failed");
    }

    match clusters::cluster_rows(state).await {
        Ok(rows) => {
            let fleet = fleet_inputs(rows, &f);
            if let Err(e) = h.insert("fleet_hourly", &fleet_rows(ts, &fleet)).await {
                tracing::warn!(table = "fleet_hourly", error = %e, "history.write.failed");
            }
        }
        Err(_) => tracing::warn!(table = "fleet_hourly", reason = "clusters-fold-failed", "history.beats.skipped"),
    }
}
