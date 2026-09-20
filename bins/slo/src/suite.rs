//! The ordered stage list per suite.
//!
//! Teardown is NOT in this list. The release profile is `panic = "abort"`, so nothing in-process
//! can survive a panicking stage — the journey runs in a child process and the parent runs
//! teardown after it, whatever the child did. `suite()` is therefore the child's list only.

use std::time::Duration;

use futures::future::BoxFuture;
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, StatefulSet};
use kloudlite_workspaces::api::workloads::{Kind, KNOWN_CENTRAL};
use kloudlite_workspaces::history::slo::SkipReason;
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
        Stage { name: stages::CONTROLLER, run: |c| Box::pin(stages::controller::run(c)) },
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

/// What one stage may cost before it is starving the stages after it. The fast suite's whole
/// budget is 780 s across eleven stages, so a stage near this has taken the run.
pub const STAGE_BUDGET: Duration = Duration::from_secs(700);

/// The reason every id a spent budget cost is skipped with.
pub const OVER_BUDGET: &str = "run budget exhausted";
pub const WEEKLY_IN_FLIGHT: &str = "a weekly drill is in flight";
pub const MONTHLY_IN_FLIGHT: &str = "a monthly drill is in flight";
/// The detail on every id a run skips because the fleet is mid-roll — every suite, since 2026-09-11:
/// an hourly `ws.stop.p95` failed one second after the node's agent restarted in a roll.
pub const ROLLOUT_IN_FLIGHT: &str = "a rollout is in flight";
/// The detail every id carries when a run of the SAME suite is already going.
///
/// `concurrencyPolicy: Forbid` stops a CronJob overlapping its own jobs and nothing else: a Job
/// created by hand (`kubectl create job --from=cronjob/…`, which is how a drill or a debug run is
/// started) is a different Job object that Forbid never sees. Two runs of one suite share the
/// tenant, its key, its quota and its `run-{id}` objects, so they corrupt each other exactly as
/// fast-vs-hourly did before the yield. Every suite yields here, the drills included — a drill
/// that WAITED for its twin would only hold the tenant longer.
pub const SAME_SUITE_IN_FLIGHT: &str = "another run of this suite is in flight";

/// The hourly journey's partition into groups lives in the catalogue, beside the journey itself —
/// the admin console slices a group run's journey with the same `group_of` this pod walked by.
pub use kloudlite_workspaces::slo::catalogue::{group_of, HOURLY_GROUPS};

impl Ctx {
    /// Whether this pod walks `id`: always, unless it is one group of a grouped hourly run.
    pub fn walks(&self, id: &str) -> bool {
        self.group.is_none_or(|g| group_of(id) == g)
    }
}

/// Boot for every group (it signs in and sweeps); any other stage only where one of its ids is ours.
fn walks_stage(c: &Ctx, kind: Suite, name: &str) -> bool {
    c.group.is_none()
        || name == "0 · Boot"
        || journey(kind).iter().any(|(n, ids)| *n == name && ids.iter().any(|id| c.walks(id)))
}

/// `(ts, group)` of one grouped hourly run's id.
fn hourly_group(id: &str) -> Option<(i64, u8)> {
    let (ts, g) = id.strip_prefix("hourly-")?.split_once("-g")?;
    Some((ts.parse().ok()?, g.parse().ok()?))
}

/// The pods of one Job start within seconds of each other; the next scheduled Job is an hour on.
const SIBLING_WINDOW_SECS: i64 = 900;

/// The group `other` walks, when it is ANOTHER group of this run's own hourly Job.
///
/// ponytail: siblings are told apart by start time, not by Job name, so a hand-created Job whose
/// pods start within 15 minutes of the scheduled one reads as siblings rather than a twin; the
/// Job name through the downward API is the upgrade if that ever happens.
pub fn sibling(mine: &str, other: &str) -> Option<u8> {
    let ((a, ga), (b, gb)) = (hourly_group(mine)?, hourly_group(other)?);
    (ga != gb && (a - b).abs() < SIBLING_WINDOW_SECS).then_some(gb)
}

/// Wait, bounded, while group `g` of this run's hourly Job is still running. For the few steps that
/// touch what a sibling is standing inside: a bounded wait costs this pod minutes, where running
/// through would file a failure for the sibling's reason. No-op for an ungrouped run.
pub async fn wait_for_group(c: &Ctx, g: u8, cap: Duration) {
    if c.group.is_none_or(|mine| mine == g) {
        return;
    }
    let started = std::time::Instant::now();
    loop {
        let read = runs_matching(c, Suite::Hourly, |id| sibling(&c.run_id, id) == Some(g)).await;
        if let Some(why) = wait_verdict(read, started.elapsed(), cap) {
            tracing::info!(group = g, waited_secs = started.elapsed().as_secs(), reason = why, "slo.group.wait.ended");
            return;
        }
        tracing::info!(group = g, waited_secs = started.elapsed().as_secs(), "slo.group.waiting");
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

/// How long a pod waits for a sibling group's FIRST row: the Job's pods start together, but one
/// that is still pulling or scheduling has filed nothing yet.
const SIBLING_GRACE: Duration = Duration::from_secs(120);

/// Why a wait for a sibling ends, or `None` to keep waiting. Keep-biased: an unreadable admin
/// process is a reason to wait (up to `cap`), never to walk into what the sibling is doing.
/// `read` is `(running, seen)` for the sibling's rows.
fn wait_verdict(read: Option<(bool, bool)>, elapsed: Duration, cap: Duration) -> Option<&'static str> {
    if elapsed >= cap {
        return Some("cap reached");
    }
    match read {
        None | Some((true, _)) => None,
        Some((false, true)) => Some("sibling done"),
        Some((false, false)) if elapsed < SIBLING_GRACE => None,
        Some((false, false)) => Some("no sibling found"),
    }
}

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
    skip_remaining_because(c, kind, remaining, OVER_BUDGET, SkipReason::Budget)
}

pub fn skip_remaining_because(c: &mut Ctx, kind: Suite, remaining: &[Stage], why: &str, reason: SkipReason) -> usize {
    let catalogue = journey(kind);
    let mut skipped = 0;
    for stage in remaining {
        c.stage = stage.name.to_string();
        let ids = catalogue.iter().find(|(name, _)| *name == stage.name).map(|(_, ids)| ids.clone());
        // Only this group's: a sibling group's id skipped here would read as that id's sample.
        let ids: Vec<&'static str> = ids.unwrap_or_default().into_iter().filter(|id| c.walks(id)).collect();
        for id in ids {
            c.skip_because(id, why, reason);
            skipped += 1;
        }
    }
    skipped
}

/// Is a run of `suite` in flight right now, as the admin process records it? A `running` row
/// older than the suite's own deadline is a crash the parent never closed, and does not count;
/// the answer is `false` on any error, because a probe that cannot ask must still probe.
pub async fn suite_in_flight(c: &Ctx, suite: Suite) -> bool {
    // Never THIS run: the parent files a `running` row before the child walks, so a run asking "is
    // my suite busy?" would always find itself and yield forever. Nor a sibling group of this
    // hourly Job, which runs beside it on purpose.
    in_flight_where(c, suite, |id| id != c.run_id && sibling(&c.run_id, id).is_none()).await
}

/// The same question about the runs of `suite` whose id `matches`.
async fn in_flight_where(c: &Ctx, suite: Suite, matches: impl Fn(&str) -> bool) -> bool {
    runs_matching(c, suite, matches).await.is_some_and(|(running, _)| running)
}

/// `(any matching row is a live run, any matching row exists at all)`, or `None` when the admin
/// process could not be read — which each caller decides for itself.
async fn runs_matching(c: &Ctx, suite: Suite, matches: impl Fn(&str) -> bool) -> Option<(bool, bool)> {
    // Eight rows, not three: the four groups of one hourly Job are four rows.
    let url = stages::admin(c, &format!("/admin/slo/runs?suite={}&limit=8", suite.as_str()));
    let v = match stages::get(c, &url, &c.admin_jwt()).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(suite = suite.as_str(), error = %format!("{e:#}"), "slo.inflight.check.failed");
            return None;
        }
    };
    let rows = v.get("runs").and_then(|r| r.as_array()).cloned().or_else(|| v.as_array().cloned()).unwrap_or_default();
    // The CronJob deadlines from deploy/kloudlite.yaml: a `running` row older than its own
    // deadline is a crash the parent never closed.
    let deadline: i64 = match suite {
        Suite::Fast => 900,
        Suite::Hourly | Suite::Weekly => 3_600,
        Suite::Monthly => 7_200,
    };
    // The longest a live run may go without reporting. A report lands after every stage and, in
    // between, on the run's own heartbeat (`crate::report::HEARTBEAT_EVERY`); a killed run's row
    // would otherwise sit `running` until `started` fell out of the deadline window above, and
    // every run of that suite yielded for the whole hour — what a hand-deleted Job did on 2026-09-06.
    let heartbeat = match suite {
        Suite::Fast => STALE_FAST,
        _ => STALE_OTHER,
    };
    let heartbeat = chrono::Duration::from_std(heartbeat).unwrap_or(chrono::Duration::minutes(10));
    let rows: Vec<_> = rows.into_iter().filter(|r| r.get("run_id").and_then(|s| s.as_str()).is_some_and(&matches)).collect();
    let seen = !rows.is_empty();
    let running = rows.iter().any(|r| {
        let running = r.get("state").and_then(|s| s.as_str()) == Some("running");
        let fresh = r
            .get("started")
            .and_then(|s| s.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .is_some_and(|t| chrono::Utc::now().signed_duration_since(t).num_seconds() < deadline);
        // And the row's own heartbeat: `updated` moves on every report. A row that has not been
        // written inside `heartbeat` belongs to a pod that is gone, whatever its state says. An
        // ABSENT `updated` is read as fresh — an older admin process that does not serve the
        // column must not make every run ignore every other one.
        let beating = r
            .get("updated")
            .and_then(|s| s.as_str())
            .map(|s| match chrono::DateTime::parse_from_rfc3339(s) {
                Ok(t) => chrono::Utc::now().signed_duration_since(t) < heartbeat,
                // ClickHouse's own `toString` is `YYYY-MM-DD hh:mm:ss.SSS`, not RFC 3339.
                Err(_) => chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
                    .map(|t| chrono::Utc::now().naive_utc().signed_duration_since(t) < heartbeat)
                    .unwrap_or(true),
            })
            .unwrap_or(true);
        running && fresh && beating
    });
    Some((running, seen))
}

/// A row not written for this long belongs to a pod that is gone.
const STALE_FAST: Duration = Duration::from_secs(300);
const STALE_OTHER: Duration = Duration::from_secs(600);

/// The suites `kind` must not run beside, longest first, and the detail every skipped id carries.
///
/// Only the DRILLS are here: weekly cordons a node and monthly decommissions one, so a fast or
/// hourly workspace placed beside them fails for the drill's reason rather than its own, and a
/// sample that measures the drill is worse than no sample.
///
/// The fast run no longer yields to the hourly (2026-09-16). Since the owner-pair split the two
/// walk as different tenants with their own quotas, so they collide on nothing; the only thing
/// the yield still bought was not measuring the same target twice, and a second availability
/// sample is not a defect. What it cost was measurement: each hourly takes ~11 min as four
/// parallel pods and swallowed the two or three fast runs inside its window — 32 % of the fast
/// runs in the six hours of 2026-09-16 filed nothing and showed on the console as skipped.
pub fn yields_to(kind: Suite) -> &'static [(Suite, &'static str)] {
    match kind {
        Suite::Fast => &[(Suite::Monthly, MONTHLY_IN_FLIGHT), (Suite::Weekly, WEEKLY_IN_FLIGHT)],
        Suite::Hourly => &[(Suite::Monthly, MONTHLY_IN_FLIGHT), (Suite::Weekly, WEEKLY_IN_FLIGHT)],
        Suite::Weekly | Suite::Monthly => &[],
    }
}

/// A drill waits for a fast or hourly run already in flight to finish before its first
/// destructive stage, bounded by one fast deadline: cancelling a run mid-journey would file a
/// failed sample for the drill's reason, and a drill that starts a minute late loses nothing.
pub async fn wait_for_shorter_runs(c: &Ctx, me: Suite) {
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(900) {
        // The other drill too: weekly and monthly share the drill tenant and both touch nodes,
        // so two of them at once would undo each other's undo.
        let other = if me == Suite::Weekly { Suite::Monthly } else { Suite::Weekly };
        let busy = suite_in_flight(c, Suite::Fast).await
            || suite_in_flight(c, Suite::Hourly).await
            || suite_in_flight(c, other).await;
        if !busy {
            return;
        }
        tracing::info!(waited_secs = started.elapsed().as_secs(), "slo.drill.waiting");
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
    tracing::warn!("slo.drill.waited.out");
}

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
/// yields to a drill — and the hourly, weekly and monthly never do, because their window is
/// the operator's own choice. `false` on any error: a probe that cannot ask must still probe.
/// How long an answer is reused. The guard is asked before every stage and on every failed step,
/// and each ask is five reads of two API servers — a stage with twenty failing steps made a
/// hundred of them (2026-09-12). Far shorter than a roll, far longer than a burst of failures.
const ROLLOUT_CACHE: Duration = Duration::from_secs(10);

pub async fn rollout_in_flight(c: &mut Ctx) -> bool {
    if !c.roll_check {
        return false;
    }
    if let Some((at, v)) = c.rollout_cache {
        if at.elapsed() < ROLLOUT_CACHE {
            return v;
        }
    }
    let v = match rollout_check(c).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "slo.rollout.check.failed");
            false
        }
    };
    c.rollout_cache = Some((std::time::Instant::now(), v));
    v
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
    // The region's other two rolled workloads. The gateway is what `gw.tunnel.p95` dials: its
    // restart at 11:15:38 on 2026-09-11 was a 20 s timeout filed as a failure, because only the
    // agent was asked.
    for name in ["kloudlite-gateway", "kloudlite-builder-gate"] {
        let d = Api::<Deployment>::namespaced(k3s.clone(), "kloudlite-system").get(name).await?;
        if mid_rollout(deployment_counts(&d)) {
            tracing::info!(workload = name, "slo.rollout.in_flight");
            return Ok(true);
        }
    }
    Ok(false)
}

/// The child's whole journey: run each stage, hand the parent what it measured, report — and stop
/// starting stages once the wall-clock budget is spent.
///
/// Here rather than in `main` so it can be watched under a budget that is already spent, which is
/// the one path a deployment cannot be asked to reproduce.
pub async fn walk(c: &mut Ctx, kind: Suite, budget: Duration) {
    let stages: Vec<Stage> = suite(kind).into_iter().filter(|s| walks_stage(c, kind, s.name)).collect();
    // For the whole walk, including a yield or a sibling wait: the row must stay fresh.
    let _beat = c.heartbeat();
    // Its own suite FIRST, and for every suite: a twin is the one collision no ladder covers.
    let mut yield_to = suite_in_flight(c, kind).await.then_some(SAME_SUITE_IN_FLIGHT);
    for (longer, why) in yields_to(kind) {
        if yield_to.is_some() {
            break;
        }
        if suite_in_flight(c, *longer).await {
            yield_to = Some(*why);
            break;
        }
    }
    if yield_to.is_none() && rollout_in_flight(c).await {
        yield_to = Some(ROLLOUT_IN_FLIGHT);
    }
    if yields_to(kind).is_empty() {
        wait_for_shorter_runs(c, kind).await;
    }
    if let Some(why) = yield_to {
        let skipped = skip_remaining_because(c, kind, &stages, why, SkipReason::InFlight);
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
        // The same question the run asked before it started, asked again at every stage: a roll
        // that begins mid-run turns THAT stage into a measurement of the roll. Only that stage
        // (2026-09-12) — abandoning the rest of the journey meant one roll that started in stage 2
        // cost the console every id from 2 to 10, when the roll was usually over by stage 4 — and
        // only inside the run's one downgrade window, so a fleet that never settles is measured
        // rather than skipped forever.
        if i > 0 && rollout_in_flight(c).await && c.roll_window_open() {
            let skipped =
                skip_remaining_because(c, kind, &stages[i..=i], ROLLOUT_IN_FLIGHT, SkipReason::InFlight);
            tracing::warn!(stage = stage.name, skipped, reason = ROLLOUT_IN_FLIGHT, "slo.stage.yielded");
            hand_over(c);
            report(c, stage.name).await;
            continue;
        }
        c.stage = stage.name.to_string();
        let started = std::time::Instant::now();
        (stage.run)(c).await;
        let took = started.elapsed();
        tracing::info!(stage = stage.name, failed = c.failed(), duration_ms = took.as_millis() as u64, "slo.stage.done");
        // A stage is one slice of a budget the pod's own `activeDeadlineSeconds` bounds, and a
        // stage that eats most of one starves every stage after it of its samples. Logged rather
        // than enforced: cutting a stage short would drop ids silently, which is the hole the
        // budget skips exist to avoid — this is the line that says WHICH stage to go and look at.
        if took > STAGE_BUDGET {
            tracing::warn!(stage = stage.name, duration_secs = took.as_secs(), budget_secs = STAGE_BUDGET.as_secs(), "slo.stage.overran");
        }
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

/// The fast suite found a live roll lock before it even spawned a child (R-1): file the whole run
/// as skipped, naming the holder, rather than the old `Ctx::new` failure that cost every fast
/// sample in the hourly's up-to-55-minute hold. Called directly by `main::parent` — there is no
/// child here to hand steps over from, so this both marks and reports in one call.
pub async fn report_held(c: &mut Ctx, kind: Suite, holder: &str) {
    let stages = suite(kind);
    let why = format!("roll coordination is held by {holder}");
    let skipped = skip_remaining_because(c, kind, &stages, &why, SkipReason::InFlight);
    tracing::warn!(skipped, holder, "slo.run.yielded");
    report(c, TEARDOWN).await;
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

    /// The WALKER honours the partition: each group files only its own ids and the four together
    /// file every hourly id exactly once. The partition itself is the catalogue's own test; this
    /// one is here because only the probe can walk a journey.
    #[tokio::test]
    async fn the_hourly_groups_partition_the_journey() {
        let mut want: Vec<String> =
            journey(Suite::Hourly).into_iter().flat_map(|(_, ids)| ids).map(str::to_string).collect();
        want.sort();
        let mut seen: Vec<String> = vec![];
        for g in 0..HOURLY_GROUPS {
            let mut c = crate::testkit::ctx().await;
            (c.suite, c.group, c.run_id) = (Suite::Hourly, Some(g), format!("hourly-1-g{g}"));
            c.retry_delay = Duration::from_millis(1);
            walk(&mut c, Suite::Hourly, Duration::ZERO).await;
            assert!(!c.steps.is_empty(), "group {g} walks nothing");
            assert!(c.steps.iter().all(|s| group_of(&s.slo_id) == g), "group {g} filed a sibling's id");
            seen.extend(c.steps.iter().map(|s| s.slo_id.clone()));
        }
        seen.sort();
        assert_eq!(seen, want, "the groups do not cover the hourly catalogue exactly once");
    }

    /// Keep-biased: an unreadable admin process or a live sibling keeps the wait going; a sibling
    /// with no row gets a grace for its first one; the cap always ends it.
    #[test]
    fn a_sibling_wait_ends_only_for_a_reason() {
        let (s, cap) = (Duration::from_secs(10), Duration::from_secs(900));
        assert_eq!(wait_verdict(None, s, cap), None, "unreadable is not done");
        assert_eq!(wait_verdict(Some((true, true)), s, cap), None);
        assert_eq!(wait_verdict(Some((false, true)), s, cap), Some("sibling done"));
        assert_eq!(wait_verdict(Some((false, false)), s, cap), None, "its first row may be coming");
        assert_eq!(wait_verdict(Some((false, false)), SIBLING_GRACE, cap), Some("no sibling found"));
        assert_eq!(wait_verdict(None, cap, cap), Some("cap reached"));
    }

    /// A live pod's row must never read as stale: the heartbeat beats well inside every window.
    #[test]
    fn the_heartbeat_is_well_inside_the_staleness_window() {
        assert!(crate::report::HEARTBEAT_EVERY * 2 < STALE_FAST.min(STALE_OTHER));
    }

    /// Every id a stage entry gates on is catalogued and in the group whose block it guards — a
    /// typo would silently land in group 0 and drop the block from its own group.
    #[test]
    fn every_gate_names_a_catalogued_id_of_its_group() {
        use kloudlite_workspaces::slo::catalogue::find;
        let gates: [(&str, u8, &[&str]); 8] = [
            ("bench.create", 3, &["bench.start.p95", "bench.tunnel"]),
            ("ws.create.p95", 0, &["ws.exec.ok", "ws.push.p95", "quota.refused"]),
            ("env.create.p95", 0, &["env.exec.ok", "env.clone.p95"]),
            ("env.intercept", 1, &stages::env_intercept::INTERCEPT_IDS),
            ("builder.hidden", 0, &[]),
            ("request.approve", 0, &["superadmin.grant"]),
            ("bench.idle.wake", 3, &["bench.session.roundtrip", "bench.exchange.both_views", "bench.two_clients"]),
            ("bench.tool.token", 3, &["bench.tool.audience", "bench.tool.revoked"]),
        ];
        for (gate, g, block) in gates {
            assert!(find(gate).is_some(), "{gate} is not catalogued");
            for id in std::iter::once(&gate).chain(block) {
                assert!(find(id).is_some(), "{id} is not catalogued");
                assert_eq!(group_of(id), g, "{id}");
            }
        }
        assert_eq!(group_of("bench.workspace.tool_roundtrip"), 0);
        assert_eq!(group_of("ws.seed.failed"), 2);
        // Its own team, never group 0's `-team` or group 1's `-icept`, which pause the same member.
        assert_eq!(group_of("team.member.paused"), 2);
    }

    #[test]
    fn only_another_group_of_the_same_job_is_a_sibling() {
        assert_eq!(sibling("hourly-1000-g0", "hourly-1030-g3"), Some(3));
        assert_eq!(sibling("hourly-1030-g3", "hourly-1000-g0"), Some(0));
        assert_eq!(sibling("hourly-1000-g0", "hourly-1001-g0"), None, "a twin is not a sibling");
        assert_eq!(sibling("hourly-1000-g0", "hourly-4600-g1"), None, "last hour's Job");
        assert_eq!(sibling("hourly-1000", "hourly-1000-g1"), None, "an ungrouped run");
        assert_eq!(sibling("fast-1000", "hourly-1000-g1"), None);
    }

    /// And the ordinary path still walks: a budget nobody has spent runs the stages. Asserted on
    /// the one stage that needs no fleet at all, so the test stays a unit test.
    #[tokio::test]
    async fn a_budget_with_time_left_is_not_spent() {
        let c = crate::testkit::ctx().await;
        assert!(!over_budget(&c, Duration::from_secs(3600)));
        assert!(over_budget(&c, Duration::ZERO));
    }

    /// A `running` row whose heartbeat has stopped is a pod that is gone, not a run in flight —
    /// otherwise one hand-deleted Job blocks its whole suite until the row's `started` ages out,
    /// which cost the hourly suite an hour of samples.
    #[tokio::test]
    async fn a_running_row_that_stopped_beating_does_not_block() {
        use axum::routing::get;
        use std::sync::Arc;
        let beat: Arc<std::sync::Mutex<String>> = Arc::new(std::sync::Mutex::new(String::new()));
        let when = beat.clone();
        let app = axum::Router::new().route(
            "/admin/slo/runs",
            get(move || {
                let updated = when.lock().expect("lock").clone();
                async move {
                    axum::Json(serde_json::json!({ "runs": [{
                        "run_id": "hourly-1",
                        "state": "running",
                        "started": chrono::Utc::now().to_rfc3339(),
                        "updated": updated,
                    }]}))
                }
            }),
        );
        let mut c = crate::testkit::ctx_against(app).await;
        c.cfg.admin_url = c.cfg.api_url.clone();
        *beat.lock().expect("lock") = chrono::Utc::now().to_rfc3339();
        assert!(suite_in_flight(&c, Suite::Hourly).await, "a beating run must block");
        *beat.lock().expect("lock") =
            (chrono::Utc::now() - chrono::Duration::minutes(30)).to_rfc3339();
        assert!(!suite_in_flight(&c, Suite::Hourly).await, "a dead run blocked its suite");
        // ClickHouse's own format, and an absent column (an older admin process).
        *beat.lock().expect("lock") =
            (chrono::Utc::now() - chrono::Duration::minutes(30)).format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        assert!(!suite_in_flight(&c, Suite::Hourly).await, "the stored format was not read");
    }

    /// The ladder itself: only the drills are a reason to yield. The fast run stopped yielding to
    /// the hourly on 2026-09-16 and a re-added entry here would cost fast samples again.
    #[test]
    fn only_the_drills_are_yielded_to() {
        assert_eq!(
            yields_to(Suite::Fast).iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            vec![Suite::Monthly, Suite::Weekly],
            "the fast run yields to the drills only"
        );
        assert_eq!(
            yields_to(Suite::Hourly).iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            vec![Suite::Monthly, Suite::Weekly]
        );
        assert!(yields_to(Suite::Weekly).is_empty() && yields_to(Suite::Monthly).is_empty());
    }

    /// A run must never see ITSELF as a reason to yield. The parent files a `running` row before
    /// the child walks, so without the exclusion every run of every suite would skip every id
    /// forever — and a run of the same suite that is NOT this one must still stop it, which is the
    /// collision `concurrencyPolicy: Forbid` cannot see (a hand-created Job is a different Job).
    #[tokio::test]
    async fn a_run_yields_to_its_twin_and_never_to_itself() {
        use axum::routing::get;
        use std::sync::Arc;
        let seen: Arc<std::sync::Mutex<String>> = Arc::new(std::sync::Mutex::new(String::new()));
        let rows = seen.clone();
        let app = axum::Router::new().route(
            "/admin/slo/runs",
            get(move || {
                let id = rows.lock().expect("lock").clone();
                async move {
                    axum::Json(serde_json::json!({ "runs": [{
                        "run_id": id,
                        "state": "running",
                        "started": chrono::Utc::now().to_rfc3339(),
                    }]}))
                }
            }),
        );
        let mut c = crate::testkit::ctx_against(app).await;
        c.cfg.admin_url = c.cfg.api_url.clone();
        // The only row running is this run's own: not a reason to yield.
        *seen.lock().expect("lock") = c.run_id.clone();
        assert!(!suite_in_flight(&c, c.suite).await, "a run saw itself");
        // Somebody else's run of the same suite: it is.
        *seen.lock().expect("lock") = format!("{}-999", c.suite.as_str());
        assert!(suite_in_flight(&c, c.suite).await, "a twin was not seen");
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
