//! What a bench workspace's second container says about itself, and the one-time move of a legacy
//! bench folder into the workspace's volume.
//!
//! A bench is a Workspace with `spec.bench` set (`crd::is_bench`) whose pod carries a `bench`
//! container beside the `workspace` one. Its lifecycle channel therefore moved from the POD
//! (`Succeeded`, exit 0) to `status.containerStatuses[name=bench]`: the pod's `restartPolicy` is
//! the workspace's `Always`, so `harness-bench` no longer exits when it goes idle — it keeps
//! serving and reports through its readiness probe, and only a HELD FOLDER still exits (75), which
//! the kubelet restarts with backoff exactly as it should.
//!
//! The idle clock is the pod's own `Ready` condition transition, never this node's clock, so a
//! replayed pass writes the identical `status.idleSince`.

use super::super::{delete_ignoring_404, Ctx, ReconcileErr, TICK};
use super::{write_ws_status, ws_conditions};
use k8s_openapi::api::core::v1::{ContainerStateTerminated, ContainerStatus, Pod};
use k8s_openapi::jiff::Timestamp;
use kloudlite_workspaces::crd;
use kloudlite_workspaces::k8s;
use kube::runtime::controller::Action;
use kube::{Api, ResourceExt};
use std::sync::Arc;

/// `harness-bench`'s exit when another pod holds the folder lock; its message names the holder.
const EXIT_LOCKED: i32 = 75;

/// How long readiness must have been false before idleness is believed. The probe's period is 5 s
/// and the verdict wants two of them; the slack on top is what keeps a container that has only
/// just started — readiness is false there too — reading as `Starting` rather than asleep.
const IDLE_SETTLE_SECS: i64 = 15;

#[derive(Debug, PartialEq)]
pub(crate) enum BenchVerdict {
    /// Nothing to decide: starting, serving, or a fault the ordinary pod path already reports.
    Serving,
    /// Asleep since this RFC 3339 instant, read off the pod.
    Idle(String),
    /// Another pod holds the folder lock; the message names the holder.
    Locked(String),
    /// The sessions container keeps dying. Carries what it died OF — the terminated message,
    /// which is the only place the cause appears at all.
    ///
    /// Distinct from `Serving` because a crash loop used to read as one: the workspace sat in
    /// `creating` for its whole 240 s ceiling and then timed out as "never came up", while the
    /// kubelet had the reason on the container status the entire time (2026-09-18).
    CrashLooping(String),
}

fn bench_status(pod: &Pod) -> Option<&ContainerStatus> {
    pod.status.as_ref()?.container_statuses.as_ref()?.iter().find(|c| c.name == k8s::BENCH_CONTAINER)
}

/// What the sessions container last died of, when the kubelet is backing off from restarting it.
///
/// Both halves are required: `waiting` alone is also how a pulling image looks, and a non-zero
/// last exit alone is how a container that has since recovered looks. Together they are a pod that
/// is going nowhere.
///
/// A zero exit is not a crash — `--idle-secs` ends the process on purpose.
fn crash_looping(c: &ContainerStatus) -> Option<String> {
    let waiting = c.state.as_ref()?.waiting.as_ref()?;
    if waiting.reason.as_deref() != Some("CrashLoopBackOff") {
        return None;
    }
    let t = terminated(c)?;
    if t.exit_code == 0 {
        return None;
    }
    // The message, else the code: `harness-bench`'s EACCES arrives as a message, but a container
    // killed by a signal has none and the number is all there is.
    Some(match t.message.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => m.to_string(),
        None => format!("exit {}", t.exit_code),
    })
}

fn terminated(c: &ContainerStatus) -> Option<&ContainerStateTerminated> {
    c.state
        .as_ref()
        .and_then(|s| s.terminated.as_ref())
        .or_else(|| c.last_state.as_ref().and_then(|s| s.terminated.as_ref()))
}

/// When the pod's `Ready` condition last went false, as the object spells it.
fn not_ready_since(pod: &Pod) -> Option<String> {
    let c = pod.status.as_ref()?.conditions.as_ref()?.iter().find(|c| c.type_ == "Ready")?;
    if c.status == "True" {
        return None;
    }
    serde_json::to_value(c.last_transition_time.as_ref()?).ok()?.as_str().map(str::to_string)
}

pub(crate) fn bench_verdict(pod: &Pod, now: Timestamp) -> BenchVerdict {
    let Some(c) = bench_status(pod) else { return BenchVerdict::Serving };
    // The lock channel is still an exit code, because a held folder is exactly the case the
    // kubelet SHOULD keep retrying — so the answer is in `lastState` once it has restarted.
    if let Some(t) = terminated(c) {
        if t.exit_code == EXIT_LOCKED {
            return BenchVerdict::Locked(t.message.clone().unwrap_or_default());
        }
    }
    // A container the kubelet is backing off from is crash-looping, and the reason it gives is the
    // one thing a person can act on. Read from `lastState` because the CURRENT state is the
    // backoff's `waiting`, which says only that it is waiting.
    //
    // After the lock check above and before idleness below: a held folder is a crash loop the
    // kubelet SHOULD keep retrying, so it keeps its own answer.
    if let Some(t) = crash_looping(c) {
        return BenchVerdict::CrashLooping(t);
    }
    // Idle is the READINESS channel, and only while the container is actually RUNNING: a crash
    // loop is `ready=false` too, and treating that as idleness would delete the pod, hide the
    // crash behind an `Idle` phase and start it again on the next wake with nothing said.
    // `started` is the startup probe's one-way flip: false means the container has never served,
    // and reading that as idleness would delete the pod of a bench that is merely slow — or
    // wedged — and hide the fault behind a phase that looks normal.
    if c.ready || !c.state.as_ref().is_some_and(|s| s.running.is_some()) || c.started != Some(true) {
        return BenchVerdict::Serving;
    }
    let Some(at) = not_ready_since(pod) else { return BenchVerdict::Serving };
    match at.parse::<Timestamp>() {
        Ok(t) if (now - t).get_seconds() >= IDLE_SETTLE_SECS => BenchVerdict::Idle(at),
        _ => BenchVerdict::Serving,
    }
}

/// A bench `!wants_pod`: asleep, or paused. Everything else it owns stands — the volume, the
/// worktree, the snapshots — and only the pod goes.
pub(crate) async fn park_bench(w: &crd::Workspace, prev: crd::WorkspaceStatus, gen: i64, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let ns = crd::ws_namespace(&w.spec.owner, &w.spec.team);
    delete_ignoring_404(&Api::<Pod>::namespaced(ctx.client.clone(), &ns), &w.name_any()).await?;
    // Idleness and a pause are the same pod decision and two different answers to "why": the
    // person's next connection wakes the first and cannot wake the second.
    let (phase, reason, message) = if w.spec.access == crd::Access::Paused {
        (crd::Phase::Stopped, "Paused", "this bench is paused")
    } else {
        (crd::Phase::Idle, crd::BENCH_IDLE, "no client and nothing running for benchIdleSecs; the next connection starts it")
    };
    // `Replicated` is recomputed here, not merely kept: idle is a bench's NORMAL resting state, and
    // `kept_conditions` would carry the running pass's `False/Running` forever — so the volume
    // decision sweep reads "waiting for a replica", never releases the volume, and a node holding
    // any idle bench can never be drained nor its bench recovered after a node death.
    let mut conditions = ws_conditions(&prev, crd::condition("Ready", false, reason, message, gen));
    if let Some(id) = prev.volume_ref.clone() {
        let replicated =
            super::super::replicated_condition(ctx, &id, &w.name_any(), super::replicas_of(ctx, &id), &prev.conditions, gen).await?;
        conditions = super::replaced(&conditions, replicated);
    }
    let st = crd::WorkspaceStatus { phase, observed_generation: Some(gen), pod_ref: None, conditions, ..prev };
    write_ws_status(w, st, ctx).await?;
    Ok(Action::await_change())
}

/// Turns `bench_verdict` into the pass's outcome. `None` = nothing to say; the ordinary readiness
/// path decides.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn bench_verdict_action(
    w: &crd::Workspace,
    pod: &Pod,
    pods: &Api<Pod>,
    pod_name: &str,
    ns: &str,
    gen: i64,
    prev: &crd::WorkspaceStatus,
    ctx: &Arc<Ctx>,
) -> Result<Option<Action>, ReconcileErr> {
    match bench_verdict(pod, Timestamp::now()) {
        BenchVerdict::Serving => Ok(None),
        BenchVerdict::Idle(at) => {
            delete_ignoring_404(pods, pod_name).await?;
            let st = crd::WorkspaceStatus {
                phase: crd::Phase::Idle,
                observed_generation: Some(gen),
                pod_ref: None,
                idle_since: Some(at),
                conditions: ws_conditions(prev, crd::condition("Ready", false, crd::BENCH_IDLE, "no client and nothing running for benchIdleSecs; the next connection starts it", gen)),
                ..prev.clone()
            };
            write_ws_status(w, st, ctx).await?;
            Ok(Some(Action::await_change()))
        }
        BenchVerdict::CrashLooping(why) => {
            // `Starting`, not `Error`: the kubelet is still retrying and a restart may yet
            // succeed — an image that was slow to pull, a node that was briefly out of memory.
            // What changes is that the REASON is said now, every pass, instead of the workspace
            // sitting in `creating` until its ceiling ran out and reporting only that.
            let st = crd::WorkspaceStatus {
                phase: crd::Phase::Starting,
                observed_generation: None,
                pod_ref: Some(format!("{ns}/{pod_name}")),
                conditions: ws_conditions(prev, crd::condition("Ready", false, crd::BENCH_CRASH_LOOPING, &why, gen)),
                ..prev.clone()
            };
            write_ws_status(w, st, ctx).await?;
            Ok(Some(Action::requeue(TICK)))
        }
        BenchVerdict::Locked(holder) => {
            let message = if holder.is_empty() { "another pod holds this bench's folder".to_string() } else { format!("folder held by {holder}") };
            let st = crd::WorkspaceStatus {
                phase: crd::Phase::Starting,
                observed_generation: None,
                pod_ref: Some(format!("{ns}/{pod_name}")),
                conditions: ws_conditions(prev, crd::condition("Ready", false, crd::FOLDER_LOCKED, &message, gen)),
                ..prev.clone()
            };
            write_ws_status(w, st, ctx).await?;
            Ok(Some(Action::requeue(TICK)))
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    const AT: &str = "2026-09-13T10:00:00Z";

    /// `ready`, the container's state, and an optional last exit.
    fn pod(ready: bool, running: bool, exit: Option<i32>) -> Pod {
        started_pod(ready, running, exit, true)
    }

    /// The same, with `started` (the startup probe's flip) spelled out.
    fn started_pod(ready: bool, running: bool, exit: Option<i32>, started: bool) -> Pod {
        let state = if running { serde_json::json!({"running": {"startedAt": AT}}) } else { serde_json::json!({"waiting": {"reason": "CrashLoopBackOff"}}) };
        serde_json::from_value(serde_json::json!({
            "apiVersion": "v1", "kind": "Pod", "metadata": {"name": "bench-1"},
            "spec": {"containers": []},
            "status": {
                "phase": "Running",
                "conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" }, "lastTransitionTime": AT}],
                "containerStatuses": [{
                    "name": k8s::BENCH_CONTAINER, "ready": ready, "started": started, "restartCount": 0, "image": "b", "imageID": "",
                    "state": state,
                    "lastState": exit.map(|code| serde_json::json!({"terminated": {"exitCode": code, "message": "node-b", "finishedAt": AT}})).unwrap_or(serde_json::Value::Null),
                }],
            },
        }))
        .unwrap()
    }

/// A bench whose sessions container keeps dying said nothing for 240 s and then timed out as a
    /// workspace that never came up. The cause is on the container status the whole time — every
    /// team bench crash-looped on `EACCES ... mkdir '.bench'` and the fleet's only signal was a
    /// phase that reads as "still starting" (2026-09-18).
    #[test]
    fn a_crash_looping_bench_is_reported_rather_than_left_looking_slow() {
        let long_after: Timestamp = "2026-09-13T11:00:00Z".parse().unwrap();
        // Waiting in CrashLoopBackOff with a non-zero last exit: the message is what it died of.
        assert_eq!(
            bench_verdict(&pod(false, false, Some(1)), long_after),
            BenchVerdict::CrashLooping("node-b".into())
        );
        // The LOCK still wins: a held folder is the one exit the kubelet should keep retrying,
        // and the person needs the holder's name rather than "it is crashing".
        assert_eq!(bench_verdict(&pod(false, false, Some(EXIT_LOCKED)), long_after), BenchVerdict::Locked("node-b".into()));
        // A container that is up, or that exited cleanly, is not crash-looping.
        assert_eq!(bench_verdict(&pod(true, true, None), long_after), BenchVerdict::Serving);
        assert_eq!(bench_verdict(&pod(false, false, Some(0)), long_after), BenchVerdict::Serving, "a clean exit is not a crash");
    }

    #[test]
    fn readiness_is_the_idle_channel_and_only_for_a_container_that_is_still_running() {
        let long_after: Timestamp = "2026-09-13T11:00:00Z".parse().unwrap();
        let just_after: Timestamp = "2026-09-13T10:00:05Z".parse().unwrap();
        assert_eq!(bench_verdict(&pod(true, true, None), long_after), BenchVerdict::Serving, "serving");
        assert_eq!(bench_verdict(&pod(false, true, None), just_after), BenchVerdict::Serving, "not ready yet is starting, not asleep");
        assert_eq!(bench_verdict(&pod(false, true, None), long_after), BenchVerdict::Idle(AT.into()));
        assert_eq!(
            bench_verdict(&pod(false, false, Some(1)), long_after),
            BenchVerdict::CrashLooping("node-b".into()),
            "a crash loop is named, not left to look like a slow start"
        );
        assert_eq!(bench_verdict(&pod(false, false, Some(EXIT_LOCKED)), long_after), BenchVerdict::Locked("node-b".into()));
        // The lock wins even once the kubelet has it running again: the restart is the backoff,
        // not a recovery, and the person needs the holder's name either way.
        assert_eq!(bench_verdict(&pod(false, true, Some(EXIT_LOCKED)), long_after), BenchVerdict::Locked("node-b".into()));
        // A container that has never served is starting (or wedged), never asleep — whatever the
        // readiness clock says. The kubelet collapses every non-zero `--ping` to `ready=false`.
        assert_eq!(
            bench_verdict(&started_pod(false, true, None, false), long_after),
            BenchVerdict::Serving,
            "never started is not idleness"
        );
    }

    /// The shape a real bench pod has: TWO containers, the workspace one still ready and the bench
    /// one not, with the pod-level `Ready` and `ContainersReady` conditions both False since the
    /// flip. The single-container fixture above cannot tell a verdict that reads the right
    /// container from one that reads the first, nor one that reads `ContainersReady` by accident.
    #[test]
    fn a_real_two_container_bench_pod_reads_as_idle() {
        let at = "2026-09-17T03:56:10Z";
        let container = |name: &str, ready: bool| {
            serde_json::json!({
                "name": name, "ready": ready, "started": true, "restartCount": 0,
                "image": "i", "imageID": "", "state": {"running": {"startedAt": "2026-09-17T03:20:00Z"}},
            })
        };
        let pod: Pod = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1", "kind": "Pod", "metadata": {"name": "bench-fbb352553329"},
            "spec": {"containers": []},
            "status": {
                "phase": "Running",
                // The order a kubelet writes them in, `Ready` last — and both dated by the flip of
                // the one container that went unready.
                "conditions": [
                    {"type": "Initialized", "status": "True", "lastTransitionTime": "2026-09-17T03:20:00Z"},
                    {"type": "PodReadyToStartContainers", "status": "True", "lastTransitionTime": "2026-09-17T03:20:00Z"},
                    {"type": "ContainersReady", "status": "False", "lastTransitionTime": at},
                    {"type": "Ready", "status": "False", "lastTransitionTime": at},
                ],
                "containerStatuses": [container("workspace", true), container(k8s::BENCH_CONTAINER, false)],
            },
        }))
        .unwrap();

        let settled: Timestamp = "2026-09-17T03:56:40Z".parse().unwrap();
        assert_eq!(bench_verdict(&pod, settled), BenchVerdict::Idle(at.into()));
        // And the workspace container going unready instead is NOT idleness: that is an ordinary
        // pod fault, which the readiness path reports.
        let mut serving = pod.clone();
        if let Some(cs) = serving.status.as_mut().and_then(|s| s.container_statuses.as_mut()) {
            cs[0].ready = false;
            cs[1].ready = true;
        }
        assert_eq!(bench_verdict(&serving, settled), BenchVerdict::Serving);
    }
}
