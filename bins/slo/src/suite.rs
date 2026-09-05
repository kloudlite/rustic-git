//! The ordered stage list per suite.
//!
//! Teardown is NOT in this list. The release profile is `panic = "abort"`, so nothing in-process
//! can survive a panicking stage — the journey runs in a child process and the parent runs
//! teardown after it, whatever the child did. `suite()` is therefore the child's list only.

use std::time::Duration;

use futures::future::BoxFuture;
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, StatefulSet};
use kloudlite_workspaces::api::workloads::{Kind, KNOWN_CENTRAL};
use kloudlite_workspaces::slo::catalogue::{journey, Suite};
use kube::api::Api;

use crate::ctx::Ctx;
use crate::stages;

pub struct Stage {
    /// "5 · Workspace" — stored verbatim as the run's and every step's `stage`, so a failed run
    /// reads as a place in the journey.
    pub name: &'static str,
    pub run: fn(&mut Ctx) -> BoxFuture<'_, ()>,
}

/// The stage name the parent files teardown's own steps under.
pub const TEARDOWN: &str = "11 · Teardown";

/// Set to `1` to insert a stage that panics, which is how the out-of-process split is tested at
/// all: nothing else in the binary can be made to abort on demand. Never set in a deployment.
const PANIC_ENV: &str = "KLOUDLITE_SLO_TEST_PANIC";

/// The fast stages, which every suite runs: weekly and monthly are the fast journey PLUS their
/// own extra stages, never a different journey — an SLO whose only samples came from a monthly
/// run would have nothing to compare against.
fn fast() -> Vec<Stage> {
    vec![
        Stage { name: "0 · Boot", run: |c| Box::pin(stages::boot(c)) },
        Stage { name: stages::IDENTITY, run: |c| Box::pin(stages::identity::run(c)) },
        Stage { name: stages::GIT, run: |c| Box::pin(stages::git::run(c)) },
        Stage { name: stages::PULL_REQUEST, run: |c| Box::pin(stages::pr::run(c)) },
        Stage { name: stages::REGISTRY, run: |c| Box::pin(stages::registry::run(c)) },
        Stage { name: stages::WORKSPACE, run: |c| Box::pin(stages::workspace::run(c)) },
        Stage { name: stages::ENVIRONMENT, run: |c| Box::pin(stages::environment::run(c)) },
        Stage { name: stages::LIFECYCLE, run: |c| Box::pin(stages::lifecycle::run(c)) },
        Stage { name: stages::ADMIN, run: |c| Box::pin(stages::admin::run(c)) },
        Stage { name: stages::SECURITY, run: |c| Box::pin(stages::security::run(c)) },
        Stage { name: stages::EDGE, run: |c| Box::pin(stages::edge::run(c)) },
    ]
}

pub fn suite(kind: Suite) -> Vec<Stage> {
    let mut stages = fast();
    // Weekly and monthly are the fast journey PLUS their own stage, appended in that order —
    // monthly is weekly plus one, never a third journey, which is the same rule `journey()` in the
    // catalogue is built on.
    // Hourly is the fast journey plus Experience and nothing else — it never walks the weekly or
    // monthly stages, which is why this is its own arm rather than another step in the ladder.
    if kind == Suite::Hourly {
        stages.push(Stage { name: stages::EXPERIENCE, run: |c| Box::pin(stages::experience::run(c)) });
    }
    if matches!(kind, Suite::Weekly | Suite::Monthly) {
        stages.push(Stage { name: stages::WEEKLY, run: |c| Box::pin(stages::weekly::run(c)) });
    }
    if kind == Suite::Monthly {
        stages.push(Stage { name: stages::MONTHLY, run: |c| Box::pin(stages::monthly::run(c)) });
    }
    if std::env::var(PANIC_ENV).as_deref() == Ok("1") {
        stages.push(Stage { name: "· Panic", run: |_| Box::pin(async { panic!("test panic") }) });
    }
    stages
}

/// The wall-clock budget the parent gives the child, when nothing sets one.
///
/// 780 s inside the fast suite's 900 s `activeDeadlineSeconds`: the deadline kills the POD, which
/// costs the run its teardown and its report, and the budget is what makes the child stop first so
/// the parent still gets both. Every suite's yaml sets its own; this is only the fallback for a
/// deployment that forgot to.
pub const DEFAULT_BUDGET_SECS: u64 = 780;

/// The reason every id a spent budget cost is skipped with.
pub const OVER_BUDGET: &str = "run budget exhausted";
/// The detail on every id a fast run skips because an hourly run is in flight.
pub const HOURLY_IN_FLIGHT: &str = "an hourly run is in flight";
/// The detail on every id a fast run skips because the fleet is mid-roll.
pub const ROLLOUT_IN_FLIGHT: &str = "a rollout is in flight";

/// The namespace every `KNOWN_CENTRAL` workload lives in on AKS.
const CENTRAL_NS: &str = "kloudlite";

/// Whether the run has spent its wall-clock budget.
///
/// Measured from `Ctx::started`, which is the PARENT's clock (it is encoded in the run id): the
/// budget bounds the run, not the child, and the parent's own boot is part of the pod's deadline.
pub fn over_budget(c: &Ctx, budget: Duration) -> bool {
    let spent = chrono::Utc::now().signed_duration_since(c.started).to_std().unwrap_or_default();
    spent >= budget
}

/// Mark every id of the stages that will NOT run, and answer how many.
///
/// A skipped id is still a sample the console can read — "the run ran out of time" is a fact about
/// the fleet — while a missing one is a hole `SloProbeMissing` would report as the CronJob never
/// having fired. Ids come from the catalogue rather than from the stage code, because a stage that
/// never ran cannot say what it would have reported.
pub fn skip_remaining(c: &mut Ctx, kind: Suite, remaining: &[Stage]) -> usize {
    skip_remaining_because(c, kind, remaining, OVER_BUDGET)
}

pub fn skip_remaining_because(c: &mut Ctx, kind: Suite, remaining: &[Stage], why: &str) -> usize {
    let catalogue = journey(kind);
    let mut skipped = 0;
    for stage in remaining {
        c.stage = stage.name.to_string();
        let ids = catalogue.iter().find(|(name, _)| *name == stage.name).map(|(_, ids)| ids.clone());
        for id in ids.unwrap_or_default() {
            c.skip(id, why);
            skipped += 1;
        }
    }
    skipped
}

/// Is an hourly run in flight right now? Asked by the fast suite before it starts anything.
///
/// The two suites run as different tenants, so they no longer collide on a key or a grant — but
/// they still share the region's nodes, and a fast workspace placed beside the hourly's five
/// waits on `Insufficient cpu` and fails its own ceiling. The hourly journey covers every fast id
/// at the same targets, so the fast run YIELDS: every id skipped, no sample filed, nothing
/// measured twice. A `running` row older than an hour is a crash the parent never closed, and
/// does not count; the answer is `false` on any error, because a probe that cannot ask must
/// still probe.
pub async fn hourly_in_flight(c: &Ctx) -> bool {
    let url = stages::admin(c, "/admin/slo/runs?suite=hourly&limit=3");
    let v = match stages::get(c, &url, &c.admin_jwt).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "slo.hourly.check.failed");
            return false;
        }
    };
    let rows = v.get("runs").and_then(|r| r.as_array()).cloned().or_else(|| v.as_array().cloned()).unwrap_or_default();
    tracing::info!(rows = rows.len(), first = %rows.first().map(|r| r.to_string()).unwrap_or_default(), "slo.hourly.check");
    rows.iter().any(|r| {
        let running = r.get("state").and_then(|s| s.as_str()) == Some("running");
        let fresh = r
            .get("started")
            .and_then(|s| s.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .is_some_and(|t| chrono::Utc::now().signed_duration_since(t).num_seconds() < 3600);
        running && fresh
    })
}

/// `(updated, ready, desired)` for a workload, or `None` when it has no status yet.
///
/// `None` reads as "not rolling": a status the API server has not filled in is a fact about the
/// read, not about the fleet, and the probe's default everywhere here is to probe.
type Counts = Option<(i32, i32, i32)>;

fn deployment_counts(o: &Deployment) -> Counts {
    let st = o.status.as_ref()?;
    let desired = o.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
    Some((st.updated_replicas.unwrap_or(0), st.ready_replicas.unwrap_or(0), desired))
}

fn statefulset_counts(o: &StatefulSet) -> Counts {
    let st = o.status.as_ref()?;
    let desired = o.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
    Some((st.updated_replicas.unwrap_or(0), st.ready_replicas.unwrap_or(0), desired))
}

fn daemonset_counts(o: &DaemonSet) -> Counts {
    let st = o.status.as_ref()?;
    Some((st.updated_number_scheduled.unwrap_or(0), st.number_ready, st.desired_number_scheduled))
}

/// Mid-roll: some pod is not yet on the new template, or not yet ready.
fn mid_rollout(c: Counts) -> bool {
    c.is_some_and(|(updated, ready, desired)| updated < desired || ready < desired)
}

/// Is the fleet mid-roll right now? Asked by the fast suite before it starts anything.
///
/// A roll moves DB ownership between pods and restarts every tier in turn; the requests a fast
/// run makes through it are exactly the ones the deploy work makes survivable, and a sample taken
/// during one measures the roll rather than the service. So the fast run yields, the same way it
/// yields to an hourly run — and the hourly, weekly and monthly never do, because their window is
/// the operator's own choice. `false` on any error: a probe that cannot ask must still probe.
pub async fn rollout_in_flight(c: &Ctx) -> bool {
    match rollout_check(c).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "slo.rollout.check.failed");
            false
        }
    }
}

async fn rollout_check(c: &Ctx) -> anyhow::Result<bool> {
    // The EXPLICIT in-cluster client, not `Ctx::kube`: that one follows KUBECONFIG into k3s, where
    // none of the central tier runs (`drill::incluster`).
    let aks = crate::drill::incluster()?;
    for (name, kind) in KNOWN_CENTRAL {
        let rolling = match kind {
            Kind::StatefulSet => statefulset_counts(&Api::namespaced(aks.clone(), CENTRAL_NS).get(name).await?),
            Kind::Deployment => deployment_counts(&Api::<Deployment>::namespaced(aks.clone(), CENTRAL_NS).get(name).await?),
            Kind::DaemonSet => daemonset_counts(&Api::<DaemonSet>::namespaced(aks.clone(), CENTRAL_NS).get(name).await?),
        };
        if mid_rollout(rolling) {
            tracing::info!(workload = name, "slo.rollout.in_flight");
            return Ok(true);
        }
    }
    // The region's agent, through the mounted k3s kubeconfig. `None` is a deployment gap, not a
    // roll — the same rule every other step that needs a kubeconfig follows.
    let Some(k3s) = &c.kube else { return Ok(false) };
    let ds = Api::<DaemonSet>::namespaced(k3s.clone(), "kube-system").get("kloudlite-agent").await?;
    if mid_rollout(daemonset_counts(&ds)) {
        tracing::info!(workload = "kloudlite-agent", "slo.rollout.in_flight");
        return Ok(true);
    }
    Ok(false)
}

/// The child's whole journey: run each stage, hand the parent what it measured, report — and stop
/// starting stages once the wall-clock budget is spent.
///
/// Here rather than in `main` so it can be watched under a budget that is already spent, which is
/// the one path a deployment cannot be asked to reproduce.
pub async fn walk(c: &mut Ctx, kind: Suite, budget: Duration) {
    let stages = suite(kind);
    let yield_to = if kind != Suite::Fast {
        None
    } else if hourly_in_flight(c).await {
        Some(HOURLY_IN_FLIGHT)
    } else if rollout_in_flight(c).await {
        Some(ROLLOUT_IN_FLIGHT)
    } else {
        None
    };
    if let Some(why) = yield_to {
        let skipped = skip_remaining_because(c, kind, &stages, why);
        tracing::warn!(skipped, reason = why, "slo.run.yielded");
        hand_over(c);
        let last = c.stage.clone();
        report(c, &last).await;
        return;
    }
    for (i, stage) in stages.iter().enumerate() {
        // Checked BEFORE a stage, never inside one: a stage cut in half reports some of its ids
        // and silently drops the rest, which is the hole these skips exist to avoid.
        if over_budget(c, budget) {
            let skipped = skip_remaining(c, kind, &stages[i..]);
            tracing::warn!(budget_secs = budget.as_secs(), skipped, "slo.run.budget.spent");
            hand_over(c);
            // Under the LAST stage `skip_remaining` stamped, which is where the run stopped.
            let last = c.stage.clone();
            report(c, &last).await;
            return;
        }
        c.stage = stage.name.to_string();
        let started = std::time::Instant::now();
        (stage.run)(c).await;
        tracing::info!(stage = stage.name, failed = c.failed(), duration_ms = started.elapsed().as_millis() as u64, "slo.stage.done");
        // Before the PUT, not after: if the report is what is broken, the parent still gets every
        // step this run measured.
        hand_over(c);
        report(c, stage.name).await;
    }
}

/// A mid-run report. A failed one does NOT stop the run — the parent's final PUT may well succeed,
/// and stopping here would cost teardown the rest of the journey for nothing — but the process
/// must still exit 3.
async fn report(c: &mut Ctx, stage: &str) {
    if let Err(e) = c.report(stage, false).await {
        tracing::error!(error = %format!("{e:#}"), "slo.report.failed");
        c.report_failed = true;
    }
}

/// Everything the child owes the parent, on disk: the steps it measured and the names it made.
/// `State` is also written after every STEP (`Ctx::save_state`); this is the stage boundary's own
/// copy, and the one that carries `steps.json`.
fn hand_over(c: &mut Ctx) {
    match serde_json::to_vec(&c.steps) {
        Ok(b) => {
            if let Err(e) = std::fs::write(c.steps_path(), b) {
                tracing::warn!(op = "write", error = %e, "slo.steps.failed");
            }
        }
        Err(e) => tracing::warn!(op = "encode", error = %e, "slo.steps.failed"),
    }
    c.save_state();
}

#[cfg(test)]
mod tests {
    use super::*;
    use kloudlite_workspaces::slo::catalogue::journey;

    /// A run whose budget is already spent starts NO stage, and every id the journey would have
    /// reported is skipped with the reason — exactly once each. A missing id is a hole
    /// `SloProbeMissing` reads as the CronJob never firing; a duplicate is two samples for one
    /// thing that never happened.
    #[tokio::test]
    async fn a_spent_budget_starts_no_stage_and_skips_every_remaining_id_once() {
        let mut c = crate::testkit::ctx().await;
        c.suite = Suite::Hourly;
        // The report PUT has nowhere to land here; without this the test waits out the whole
        // backoff schedule for a run that measured nothing.
        c.retry_delay = Duration::from_millis(1);
        // Nothing is reachable in a test, so a stage that DID run would leave failing samples
        // behind; the assertion below is what says none did.
        walk(&mut c, Suite::Hourly, Duration::ZERO).await;

        let expected: Vec<&str> =
            journey(Suite::Hourly).into_iter().flat_map(|(_, ids)| ids).collect();
        for id in &expected {
            let rows: Vec<_> = c.steps.iter().filter(|s| s.slo_id == *id).collect();
            assert_eq!(rows.len(), 1, "{id} was not skipped exactly once");
            assert!(rows[0].skipped, "{id} ran");
            assert_eq!(rows[0].detail, OVER_BUDGET);
        }
        assert_eq!(c.steps.len(), expected.len(), "an id nobody asked for was reported");
        assert_eq!(c.failed(), 0, "a skip is not a failure");
    }

    /// And the ordinary path still walks: a budget nobody has spent runs the stages. Asserted on
    /// the one stage that needs no fleet at all, so the test stays a unit test.
    #[tokio::test]
    async fn a_budget_with_time_left_is_not_spent() {
        let c = crate::testkit::ctx().await;
        assert!(!over_budget(&c, Duration::from_secs(3600)));
        assert!(over_budget(&c, Duration::ZERO));
    }

    /// The yield's whole judgement, over the three status shapes a roll actually moves. A status
    /// the API server has not written yet must read as "not rolling" — otherwise a probe that
    /// caught a workload mid-create would yield forever.
    #[test]
    fn only_a_workload_short_of_desired_is_mid_rollout() {
        use k8s_openapi::api::apps::v1::{
            DaemonSetSpec, DaemonSetStatus, DeploymentSpec, DeploymentStatus, StatefulSetSpec, StatefulSetStatus,
        };

        let deploy = |updated, ready| Deployment {
            spec: Some(DeploymentSpec { replicas: Some(3), ..Default::default() }),
            status: Some(DeploymentStatus {
                updated_replicas: Some(updated),
                ready_replicas: Some(ready),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(mid_rollout(deployment_counts(&deploy(2, 3))));
        assert!(mid_rollout(deployment_counts(&deploy(3, 2))));
        assert!(!mid_rollout(deployment_counts(&deploy(3, 3))));
        assert!(!mid_rollout(deployment_counts(&Deployment::default())));

        let sts = StatefulSet {
            spec: Some(StatefulSetSpec { replicas: Some(3), ..Default::default() }),
            status: Some(StatefulSetStatus {
                updated_replicas: Some(1),
                ready_replicas: Some(3),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(mid_rollout(statefulset_counts(&sts)));
        assert!(!mid_rollout(statefulset_counts(&StatefulSet::default())));

        let ds = |updated, ready| DaemonSet {
            spec: Some(DaemonSetSpec::default()),
            status: Some(DaemonSetStatus {
                desired_number_scheduled: 4,
                updated_number_scheduled: Some(updated),
                number_ready: ready,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(mid_rollout(daemonset_counts(&ds(3, 4))));
        assert!(mid_rollout(daemonset_counts(&ds(4, 3))));
        assert!(!mid_rollout(daemonset_counts(&ds(4, 4))));
        assert!(!mid_rollout(daemonset_counts(&DaemonSet::default())));
    }
}
