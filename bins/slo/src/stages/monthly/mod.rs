//! Stage 13 · Monthly: the backups nobody watches, and the three drills that break the fleet on
//! purpose.
//!
//! Every row in `deploy/BACKUPS.md` is a RETENTION SETTING, which is the kind of thing that fails
//! silently by definition — a switch somebody turned off during an incident stays off until the
//! restore that needed it. The four `bak.*` ids are that page, read rather than claimed.
//!
//! The three drills are the other half: the fleet's resilience is only true if somebody keeps
//! proving it, and the one absolute rule is that a drill undoes itself on every path out of it
//! (`crate::drill`) — a monthly probe that left a node tainted is an outage nobody would think to
//! look for.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use futures::{FutureExt, TryStreamExt};
use serde_json::{json, Value};

use super::{admin, api, get, poll_json, post};
use crate::ctx::Ctx;
use crate::{drill, tools};

mod outages;
pub(crate) use outages::*;
mod nodes;
pub(crate) use nodes::*;
mod backups;
pub(crate) use backups::*;


/// The container and the fixed slot names `deploy/k3s/backup-controlplane.sh` writes: 24 hourly
/// slots covering a day and 7 daily ones covering a week, all overwritten in place. Repeated here
/// rather than derived — the shell script is the contract, and a probe that inferred the names
/// would go green the day somebody renamed them.
pub(super) const BACKUP_CONTAINER: &str = "k3s-backup";

pub(super) const SLOT_SUFFIX: &str = ".tgz.enc";
/// The timer runs hourly, so anything past two hours has MISSED one — the same threshold
/// `deploy/BACKUPS.md`'s verification step names. In MINUTES because `num_hours` truncates: a
/// backup 119 minutes old and one 60 minutes old are both "1 hour", and the comparison would let
/// the age drift most of an hour past the threshold before anyone heard about it.
pub(super) const MAX_TARBALL_AGE_MINS: i64 = 120;


pub(super) const READ_CEILING: Duration = Duration::from_secs(60);
/// The dead-node drill waits out `nodeDeadSecs` and then a start elsewhere; the drain waits out the
/// agent's own beat, which the console gives ten minutes.
pub(super) const DRAIN_CAP: Duration = Duration::from_secs(600);
/// Two marker-reconcile beats plus drift — see `without_redis`. The `repo_created` half of the
/// feed comes off the index markers, never the stream, and one beat was not a wait.
pub(super) const FEED_FALLBACK: Duration = Duration::from_secs(150);


/// Long enough that a fleet leaning on Redis for anything load-bearing would show it, short enough
/// that the CronJob's two hours still fit the dead-node drill after it.
pub(super) const REDIS_DOWN: Duration = Duration::from_secs(300);


/// A step's ceiling for a body that has an undo: always the body's own plus a minute. `Ctx::step`
/// times out by DROPPING the step's future, so an outer timeout that fired first would take the
/// undo with it — the drill's own cap has to be the one that wins.
pub(super) fn step_cap(body: Duration) -> Duration {
    body + Duration::from_secs(60)
}


pub async fn run(c: &mut Ctx) {
    backups(c).await;
    dead_node(c).await;
    interrupted(c).await;
    drain(c).await;
    decommission(c).await;
    redis_down(c).await;
    clickhouse_down(c).await;
}


/// The second policy this probe ever writes, deleted on every path out and blind in teardown.
pub const CH_NETPOL: &str = "slo-drill-clickhouse";


/// The reason every id that needs a genuinely dead node carries, naming where the recipe lives.
pub(super) const NODE_LEVEL_DRILL: &str = "a dead node needs the operator's node-level drill: stop the kubelet on one pool node — recipe in deploy/k3s/README.md";


/// The one NetworkPolicy this probe ever writes, named here because teardown deletes it blind on
/// every run — including runs that never went near a drill.
pub const NETPOL: &str = "slo-drill-redis";


#[cfg(test)]
mod tests {
    use super::*;

    /// Every id exactly once, whatever is configured. With no Azure credential, no kubeconfig and
    /// no Redis host, the console still owes seven rows — a stage that dropped ids when its
    /// preconditions were absent would make an unconfigured probe look like a healthy one.
    #[tokio::test]
    async fn monthly_produces_every_id_once() {
        let mut c = crate::testkit::ctx().await;
        c.kube = None;
        tokio::time::pause();
        run(&mut c).await;
        let ids: Vec<&str> = c.steps.iter().map(|s| s.slo_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "bak.tarball.age",
                "bak.daily.slots",
                "bak.versioning",
                "bak.cosmos",
                "drill.dead.node",
                "ws.interrupted",
                "env.clone.interrupted",
                "drill.drain",
                "cluster.decommission",
                "drill.redis.down",
                "drill.clickhouse.down",
            ]
        );
        assert_eq!(c.failed(), 0, "an unconfigured probe skips; it does not breach");
    }

    /// The slot names are the shell script's contract, and the daily check is about EXISTENCE:
    /// a missing weekday means a whole day's run has never succeeded, which "the newest backup is
    /// recent" cannot see.
    /// The old session/env labels are rejected on purpose: a node still wearing only those from
    /// before the pool rename must not be picked, or the drill would fence a node the agent never
    /// reconciles.
    #[test]
    fn only_the_pool_label_picks_a_node() {
        fn node(labels: &[(&str, &str)]) -> k8s_openapi::api::core::v1::Node {
            k8s_openapi::api::core::v1::Node {
                metadata: kube::api::ObjectMeta {
                    labels: Some(labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()),
                    ..Default::default()
                },
                ..Default::default()
            }
        }
        assert!(is_pool_node(&node(&[("kloudlite.io/pool", "true")])));
        assert!(!is_pool_node(&node(&[])), "the control plane carries no pool label");
        assert!(
            !is_pool_node(&node(&[("kloudlite.io/session", "true"), ("kloudlite.io/env", "true")])),
            "the retired labels no longer qualify a node"
        );
    }

    #[test]
    fn slots_before_the_encrypted_unit_existed_are_not_due_yet() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-06T20:38:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(slots_due(now, None).len(), 7, "no encrypted blob: everything is due, and reported");
        let a_month = Some(now - chrono::Duration::days(30));
        assert_eq!(slots_due(now, a_month).len(), 7);
        let today = Some(now - chrono::Duration::hours(1));
        assert_eq!(slots_due(now, today), vec!["daily-Sun.tgz.enc".to_string()], "installed today: only today");
        let two_days = Some(now - chrono::Duration::days(2));
        assert_eq!(slots_due(now, two_days), ["daily-Sun", "daily-Sat", "daily-Fri"].map(|d| format!("{d}.tgz.enc")));
    }

    #[test]
    fn a_repo_created_row_is_matched_by_bare_name_and_owner_href() {
        let feed = serde_json::json!([
            {"kind": "pull_merged", "repo": "web", "href": "/alice/web/pulls/1"},
            {"kind": "repo_created", "repo": "web", "href": "/alice/web"},
        ]);
        assert!(created(&feed, "alice", "web"));
        assert!(!created(&feed, "bob", "web"), "same name under another owner is not this repo");
        assert!(!created(&feed, "alice", "api"));
    }

    #[test]
    fn the_daily_slots_are_the_seven_the_backup_script_writes() {
        let have = slots_due(Utc::now(), None);
        assert_eq!(have.len(), 7);
        assert!(have.contains(&"daily-Mon.tgz.enc".to_string()));
        assert!(have.contains(&format!("daily-{}{SLOT_SUFFIX}", Utc::now().format("%a"))), "today's weekday is one of them");
    }

    /// A policy built from a hostname denies nothing: `ipBlock` takes CIDRs, and `dig +short` on a
    /// CNAME chain prints the intermediate NAMES alongside the addresses.
    #[test]
    fn the_redis_policy_only_ever_excepts_addresses() {
        let spec = deny_egress(&["10.0.0.7".into(), "10.0.0.8".into()]);
        let except = spec.pointer("/egress/0/to/0/ipBlock/except").and_then(Value::as_array).expect("except");
        assert_eq!(except.len(), 2);
        assert_eq!(except[0], "10.0.0.7/32");
        // DNS stays open: a pod that cannot resolve is a DNS outage, not the Redis one being drilled.
        assert!(spec.to_string().contains("\"port\":53"), "{spec}");
    }

    /// Only `repo_created`, and only this repo's. The PR half of the feed is stream-only on
    /// purpose, so asserting on it would fail a drill the system passed by design.
    #[test]
    fn only_this_repos_creation_counts() {
        let feed = json!([
            { "kind": "repo_created", "repo": "run-monthly-1-redis", "href": "/slo-probe/run-monthly-1-redis" },
            { "kind": "pull_merged", "repo": "run-monthly-1-redis", "href": "/slo-probe/run-monthly-1-redis/pulls/1" },
        ]);
        assert!(created(&feed, "slo-probe", "run-monthly-1-redis"));
        assert!(!created(&feed, "slo-probe", "run-monthly-2-redis"));
    }
}
