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
use super::{migrate_bench_folder, write_ws_status, ws_conditions};
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
}

fn bench_status(pod: &Pod) -> Option<&ContainerStatus> {
    pod.status.as_ref()?.container_statuses.as_ref()?.iter().find(|c| c.name == k8s::BENCH_CONTAINER)
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

/// The one-time legacy folder move, on the pass that first finds the worktree without a `.bench`.
/// `Some(action)` means this pass is over and NO pod may be created — a pod started against a
/// half-copied folder is a bench that silently lost transcripts.
pub(crate) async fn migrate_bench(
    w: &crd::Workspace,
    volume: &str,
    gen: i64,
    prev: &mut crd::WorkspaceStatus,
    ctx: &Arc<Ctx>,
) -> Result<Option<Action>, ReconcileErr> {
    // No share on this node is not this function's refusal to make: `apply_workspace` already
    // parked on `HomeNotReady` above, and the legacy folder lives on that share.
    let Some(export) = ctx.homes_export.clone() else { return Ok(None) };
    // The legacy folder was keyed by the team `/v1` minted the bench under, and a PERSONAL bench's
    // team is the person's own handle there while `spec.team` on the Workspace is "" — so the
    // handle goes back in, or every personal bench would look for `.benches//{owner}`.
    let team = if w.spec.team.is_empty() { w.spec.owner.clone() } else { w.spec.team.clone() };
    let (pool, owner) = (ctx.pool.clone(), w.spec.owner.clone());
    let worktree = ctx.engine.pool.worktree(volume, &w.name_any());
    let r = tokio::task::spawn_blocking(move || migrate_bench_folder(&pool, &export, &team, &owner, &worktree))
        .await
        .map_err(|e| ReconcileErr(e.to_string()))?;
    match r {
        Ok(None) => Ok(None),
        Ok(Some((files, bytes))) => {
            tracing::info!(workspace = %w.name_any(), files, bytes, "bench.folder.migrated");
            prev.conditions = super::replaced(&prev.conditions, crd::condition(crd::FOLDER_MIGRATED, true, "Migrated", &format!("{files} files ({bytes} bytes) moved into this workspace"), gen));
            Ok(None)
        }
        Err(why) => {
            // Retried next pass rather than settled: an io error on a share is transient far more
            // often than it is permanent, and there is no pod either way until it lands.
            let st = crd::WorkspaceStatus {
                phase: crd::Phase::Creating,
                observed_generation: None,
                conditions: ws_conditions(prev, crd::condition(crd::FOLDER_MIGRATED, false, "MigrationFailed", &why, gen)),
                ..prev.clone()
            };
            write_ws_status(w, st, ctx).await?;
            Ok(Some(Action::requeue(TICK)))
        }
    }
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

    #[test]
    fn readiness_is_the_idle_channel_and_only_for_a_container_that_is_still_running() {
        let long_after: Timestamp = "2026-09-13T11:00:00Z".parse().unwrap();
        let just_after: Timestamp = "2026-09-13T10:00:05Z".parse().unwrap();
        assert_eq!(bench_verdict(&pod(true, true, None), long_after), BenchVerdict::Serving, "serving");
        assert_eq!(bench_verdict(&pod(false, true, None), just_after), BenchVerdict::Serving, "not ready yet is starting, not asleep");
        assert_eq!(bench_verdict(&pod(false, true, None), long_after), BenchVerdict::Idle(AT.into()));
        assert_eq!(bench_verdict(&pod(false, false, Some(1)), long_after), BenchVerdict::Serving, "a crash loop is not idleness");
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
}
