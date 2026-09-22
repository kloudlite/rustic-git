//! The `Bench` reconciler: a person's bench on one team, placed by the claim like any parent but
//! holding no volume. What it converges is small — the folder on the region share, the attach
//! `resolv.conf`, the gateway-only ingress policy, and at most one pod — and what it decides is
//! mostly the pod's lifecycle: `harness-bench` keeps the idle clock and exits 0 when nobody has
//! used it for `benchIdleSecs`; this pass turns that exit into `Idle` and creates no pod again until
//! `/v1` stamps a `wakeAt` later than the exit. `idleSince` is the container's own `finishedAt`, never
//! this node's clock, so a replayed pass writes the identical status.

use super::{delete_ignoring_404, ensure, heal_labels, my_node, owner_ref_of_kind, replaced, write_status, Ctx, ReconcileErr, TICK};
use k8s_openapi::api::core::v1::{ContainerStateTerminated, Node, Pod, Secret};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::runtime::controller::Action;
use kube::api::DeleteParams;
use kube::{Api, Resource, ResourceExt};
use kloudlite_workspaces::crd::{self, BenchAccess, Condition, DesiredState, Phase};
use kloudlite_workspaces::k8s;
use std::sync::Arc;
use std::time::Duration;

/// What the pod says about the bench.
#[derive(Debug)]
pub(crate) enum PodVerdict {
    Create,
    Replace,
    Starting,
    Ready,
    Locked(String),
    Idle(String),
    Absent,
    Remove,
}

/// `harness-bench`'s exit when another pod holds the folder lock; its message names the holder.
const EXIT_LOCKED: i32 = 75;
const SHORT: Duration = Duration::from_secs(2);

fn terminated(pod: &Pod) -> Option<&ContainerStateTerminated> {
    let c = pod.status.as_ref()?.container_statuses.as_ref()?.iter().find(|c| c.name == k8s::BENCH_CONTAINER)?;
    c.state.as_ref().and_then(|s| s.terminated.as_ref()).or_else(|| c.last_state.as_ref().and_then(|s| s.terminated.as_ref()))
}

pub(crate) fn bench_state(b: &crd::Bench, pod: Option<&Pod>) -> PodVerdict {
    let wants = crd::bench_wants_pod(b);
    let Some(pod) = pod else {
        return if wants { PodVerdict::Create } else { PodVerdict::Absent };
    };
    if b.spec.desired_state == DesiredState::Stopped {
        return PodVerdict::Remove;
    }
    if pod.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Succeeded") {
        // A runtime that omits `finishedAt` falls back to the pod's own Ready transition: an empty
        // `idleSince` would read as "never slept" and recreate the pod at once. Neither clock is
        // this node's, so a replayed pass still writes the identical status.
        let ready_at = pod
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .and_then(|cs| cs.iter().find(|c| c.type_ == "Ready"))
            .and_then(|c| c.last_transition_time.as_ref());
        let at = terminated(pod)
            .and_then(|t| t.finished_at.as_ref())
            .or(ready_at)
            .and_then(|t| serde_json::to_value(t).ok())
            .and_then(|v| v.as_str().map(str::to_string));
        return match at {
            Some(at) => PodVerdict::Idle(at),
            None => PodVerdict::Starting,
        };
    }
    if !wants {
        return PodVerdict::Remove;
    }
    // A container's command is immutable, so an access change is a new pod.
    let read_only = pod
        .spec
        .as_ref()
        .and_then(|s| s.containers.iter().find(|c| c.name == k8s::BENCH_CONTAINER))
        .and_then(|c| c.command.as_ref())
        .is_some_and(|cmd| cmd.iter().any(|a| a == "--read-only"));
    if read_only != (b.spec.access == BenchAccess::ReadOnly) {
        return PodVerdict::Replace;
    }
    let ready = pod.status.as_ref().and_then(|s| s.conditions.as_ref()).is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"));
    if ready {
        return PodVerdict::Ready;
    }
    match terminated(pod) {
        Some(t) if t.exit_code == EXIT_LOCKED => PodVerdict::Locked(t.message.clone().unwrap_or_default()),
        _ => PodVerdict::Starting,
    }
}

async fn write(b: &crd::Bench, st: crd::BenchStatus, ctx: &Arc<Ctx>) -> Result<(), ReconcileErr> {
    write_status(b, "Bench", b.status.as_ref(), &st, ctx, |a, b| {
        a.phase == b.phase
            && a.node_name == b.node_name
            && a.pod_ref == b.pod_ref
            && a.idle_since == b.idle_since
            && super::conditions_eq(&a.conditions, &b.conditions)
    })
    .await
}

/// Whether the pod sits on ANOTHER node that is dead or leaving — the same `unplaceable` predicate
/// the sweep released the bench by, so the two can never disagree on which node is gone.
async fn on_unplaceable_node(pod: &Pod, ctx: &Arc<Ctx>) -> Result<bool, ReconcileErr> {
    let on = pod.spec.as_ref().and_then(|s| s.node_name.as_deref()).unwrap_or_default();
    if on.is_empty() || on == ctx.node {
        return Ok(false);
    }
    // Absent reads as not-unplaceable: a deleted Node takes its pods with it, nothing to force.
    let Some(node) = Api::<Node>::all(ctx.client.clone()).get_opt(on).await? else { return Ok(false) };
    Ok(crate::peer::unplaceable(Some(&node), crate::peer::node_dead_secs(&ctx.settings), k8s_openapi::jiff::Timestamp::now()))
}

/// Both halves of an attachment, as `apply_workspace` writes them: egress in the bench's namespace,
/// ingress in the environment's, owned by the Environment because an ownerReference cannot cross
/// namespaces. The bench pod carries `WORKSPACE_LABEL` = its id, so the same selectors apply. The
/// `Attached` condition's message is the bare environment id, which is how a detach or a re-attach
/// finds the grant it left behind; a detach removes the condition once it has cleaned up.
async fn attach_grants(
    b: &crd::Bench,
    name: &str,
    ns: &str,
    policies: &Api<NetworkPolicy>,
    prev: &mut crd::BenchStatus,
    gen: i64,
    ctx: &Arc<Ctx>,
) -> Result<(), ReconcileErr> {
    let env = match b.spec.attached_environment.as_deref() {
        None => None,
        Some(id) => Api::<crd::Environment>::all(ctx.client.clone()).get_opt(id).await?.filter(|e| e.spec.region == ctx.region),
    };
    let was = prev.conditions.iter().find(|c| c.type_ == crd::ATTACHED && c.status == "True").map(|c| c.message.clone());
    let now = env.as_ref().map(|e| e.name_any());
    if let Some(was) = was.as_ref().filter(|w| now.as_ref() != Some(*w)) {
        delete_ignoring_404(&Api::<NetworkPolicy>::namespaced(ctx.client.clone(), &crd::env_namespace(was)), &k8s::attach_policy_name(name)).await?;
    }
    match &env {
        Some(e) => {
            let env_ns = crd::env_namespace(&e.name_any());
            ensure(policies, &k8s::attach_egress(ns, name, &env_ns, &b.spec.owner, &owner_ref_of_kind(b)?), ctx).await?;
            let in_env: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &env_ns);
            ensure(&in_env, &k8s::attach_ingress(&env_ns, ns, name, &b.spec.owner, &owner_ref_of_kind(e)?), ctx).await?;
            prev.conditions = replaced(&prev.conditions, crd::condition(crd::ATTACHED, true, "Converged", &e.name_any(), gen));
        }
        None if prev.conditions.iter().any(|c| c.type_ == crd::ATTACHED) => {
            delete_ignoring_404(policies, &k8s::attach_policy_name(name)).await?;
            prev.conditions.retain(|c| c.type_ != crd::ATTACHED);
        }
        None => {}
    }
    Ok(())
}

pub async fn reconcile_bench(b: Arc<crd::Bench>, ctx: Arc<Ctx>) -> Result<Action, ReconcileErr> {
    // No finalizer: the pod is ownerReference-collected and the folder outlives the bench on purpose.
    if b.meta().deletion_timestamp.is_some() {
        return Ok(Action::await_change());
    }
    let name = b.name_any();
    if super::timed("my_node", &name, my_node(&ctx)).await.dead {
        return Ok(Action::requeue(TICK));
    }
    let (owner, team) = (b.spec.owner.clone(), b.spec.team.clone());
    let gen = b.meta().generation.unwrap_or(0);
    let mut prev = b.status.clone().unwrap_or_default();
    let cond = |t: &str, ok: bool, reason: &str, msg: &str| crd::condition(t, ok, reason, msg, gen);
    let with = |prev: &crd::BenchStatus, c: Condition| replaced(&prev.conditions, c);

    heal_labels(&Api::<crd::Bench>::all(ctx.client.clone()), &*b, &owner, &team, "bench").await?;

    if !super::timed("namespace_ready", &name, crate::binding::namespace_ready(&ctx, &ctx.region, &owner, &team)).await? {
        let c = cond(crate::binding::NAMESPACE_READY, false, "NamespaceNotReady", "waiting for the owner's namespace");
        write(&b, crd::BenchStatus { phase: Phase::Creating, conditions: with(&prev, c), ..prev }, &ctx).await?;
        return Ok(Action::requeue(TICK));
    }

    let Some(export) = ctx.homes_export.clone() else {
        let c = cond("Ready", false, crd::FOLDER_NOT_READY, "this node has no shared-home mount (WS_HOMES_EXPORT)");
        write(&b, crd::BenchStatus { phase: Phase::Creating, conditions: with(&prev, c), ..prev }, &ctx).await?;
        return Ok(Action::requeue(TICK));
    };
    let (pool, t, o) = (ctx.pool.clone(), team.clone(), owner.clone());
    let folder = super::timed("bench_folder", &name, tokio::task::spawn_blocking(move || {
        super::workspace::ensure_bench_folder(&pool, &export, &t, &o, k8s::SSH_UID as u32)
    }))
    .await
    .map_err(|e| ReconcileErr(e.to_string()))?;
    if let Err(why) = folder {
        let c = cond(crd::FOLDER_READY, false, crd::FOLDER_NOT_READY, &why);
        write(&b, crd::BenchStatus { phase: Phase::Creating, conditions: with(&prev, c), ..prev }, &ctx).await?;
        return Ok(Action::requeue(TICK));
    }
    prev.conditions = with(&prev, cond(crd::FOLDER_READY, true, "Ready", "the bench folder exists on the region share"));

    let ns = crd::ws_namespace(&owner, &team);
    let (pool, id, ns_owned, env_ns) = (ctx.pool.clone(), name.clone(), ns.clone(), b.spec.attached_environment.as_deref().map(crd::env_namespace));
    tokio::task::spawn_blocking(move || super::write_resolv_conf(&pool, &id, &ns_owned, env_ns.as_deref()))
        .await
        .map_err(|e| ReconcileErr(e.to_string()))??;

    let policies: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &ns);
    ensure(&policies, &k8s::bench_ingress_policy(&ns, &name), &ctx).await?;
    attach_grants(&b, &name, &ns, &policies, &mut prev, gen, &ctx).await?;

    if Api::<Secret>::namespaced(ctx.client.clone(), &ns).get_opt(k8s::USER_KEY_SECRET).await?.is_none() {
        let c = cond("Ready", false, "KeysNotReady", "the user-key Secret is not in the namespace yet");
        write(&b, crd::BenchStatus { phase: Phase::Creating, conditions: with(&prev, c), ..prev }, &ctx).await?;
        return Ok(Action::requeue(TICK));
    }

    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &ns);
    let pod = pods.get_opt(k8s::BENCH_POD).await?;
    if let Some(p) = &pod {
        if on_unplaceable_node(p, &ctx).await? {
            // A pod on an unreachable node stays Terminating until its Node object goes, and this
            // pass would read it as Starting forever. Grace 0 removes the object; a zombie that is
            // still running there is fenced by the folder lock (exit 75), not by this delete.
            tracing::info!(bench = %name, "bench.pod.forced");
            match pods.delete(k8s::BENCH_POD, &DeleteParams { grace_period_seconds: Some(0), ..Default::default() }).await {
                Err(kube::Error::Api(e)) if e.code != 404 => return Err(kube::Error::Api(e).into()),
                Err(e) if !matches!(e, kube::Error::Api(_)) => return Err(e.into()),
                _ => {}
            }
            return Ok(Action::requeue(SHORT));
        }
    }
    let pod_ref = Some(format!("{ns}/{}", k8s::BENCH_POD));
    match bench_state(&b, pod.as_ref()) {
        PodVerdict::Create => {
            let idle_secs = ctx.settings.load().bench_idle_secs;
            let kompress_url = ctx.settings.load().kompress_url.clone();
            let p = k8s::bench_pod(&b, &name, &ctx.pool, ctx.runtime_class.as_deref(), &ctx.registry_host, idle_secs, &kompress_url).map_err(ReconcileErr)?;
            super::create_if_absent(&pods, &p).await?;
            let c = cond("Ready", false, "Starting", "the bench pod is starting");
            write(&b, crd::BenchStatus { phase: Phase::Starting, pod_ref, idle_since: None, conditions: with(&prev, c), ..prev }, &ctx).await?;
            Ok(Action::requeue(TICK))
        }
        PodVerdict::Idle(at) => {
            delete_ignoring_404(&pods, k8s::BENCH_POD).await?;
            let c = cond("Ready", false, crd::BENCH_IDLE, "no client and nothing running for benchIdleSecs; the next connection starts it");
            write(&b, crd::BenchStatus { phase: Phase::Idle, pod_ref: None, idle_since: Some(at), conditions: with(&prev, c), ..prev }, &ctx).await?;
            Ok(Action::await_change())
        }
        PodVerdict::Absent => {
            let st = if b.spec.desired_state == DesiredState::Stopped {
                let c = cond("Ready", false, "Stopped", "the bench is stopped");
                crd::BenchStatus { phase: Phase::Stopped, pod_ref: None, conditions: with(&prev, c), ..prev }
            } else {
                crd::BenchStatus { phase: Phase::Idle, pod_ref: None, ..prev }
            };
            write(&b, st, &ctx).await?;
            Ok(Action::await_change())
        }
        PodVerdict::Remove | PodVerdict::Replace => {
            delete_ignoring_404(&pods, k8s::BENCH_POD).await?;
            Ok(Action::requeue(SHORT))
        }
        PodVerdict::Starting => {
            let c = cond("Ready", false, "Starting", "the bench pod is starting");
            write(&b, crd::BenchStatus { phase: Phase::Starting, pod_ref, conditions: with(&prev, c), ..prev }, &ctx).await?;
            Ok(Action::requeue(TICK))
        }
        PodVerdict::Ready => {
            let reason = if b.spec.access == BenchAccess::ReadOnly { "ReadOnly" } else { "Running" };
            let c = cond("Ready", true, reason, "the bench is serving");
            write(&b, crd::BenchStatus { phase: Phase::Ready, pod_ref, conditions: with(&prev, c), ..prev }, &ctx).await?;
            Ok(Action::await_change())
        }
        PodVerdict::Locked(holder) => {
            let c = cond("Ready", false, crd::FOLDER_LOCKED, &format!("folder held by {holder}"));
            write(&b, crd::BenchStatus { phase: Phase::Starting, pod_ref, conditions: with(&prev, c), ..prev }, &ctx).await?;
            Ok(Action::requeue(TICK))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{Container, ContainerState, ContainerStatus, PodCondition, PodSpec, PodStatus};

    const FINISHED_AT: &str = "2026-09-13T10:00:00Z";

    fn fixture_bench(desired: DesiredState) -> crd::Bench {
        let mut b = crd::Bench::new(
            "bench-1",
            serde_json::from_value(serde_json::json!({"owner": "alice", "team": "acme", "image": "i", "desiredState": "running"})).unwrap(),
        );
        b.spec.desired_state = desired;
        b.status = Some(crd::BenchStatus::default());
        b
    }

    fn pod_with(command: &[&str], last_terminated: Option<(i32, &str)>, ready: bool) -> Pod {
        let terminated = last_terminated.map(|(exit_code, message)| ContainerState {
            terminated: Some(ContainerStateTerminated {
                exit_code,
                message: Some(message.into()),
                finished_at: Some(serde_json::from_value(serde_json::json!(FINISHED_AT)).unwrap()),
                ..Default::default()
            }),
            ..Default::default()
        });
        Pod {
            spec: Some(PodSpec {
                containers: vec![Container { name: k8s::BENCH_CONTAINER.into(), command: Some(command.iter().map(|s| s.to_string()).collect()), ..Default::default() }],
                ..Default::default()
            }),
            status: Some(PodStatus {
                phase: Some("Running".into()),
                conditions: Some(vec![PodCondition { type_: "Ready".into(), status: if ready { "True" } else { "False" }.into(), ..Default::default() }]),
                container_statuses: Some(vec![ContainerStatus { name: k8s::BENCH_CONTAINER.into(), last_state: terminated, ..Default::default() }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn the_pod_decides_create_replace_ready_locked_idle_and_absent() {
        let running = fixture_bench(DesiredState::Running);
        assert!(matches!(bench_state(&running, None), PodVerdict::Create));
        assert!(matches!(bench_state(&running, Some(&pod_with(&["harness-bench", "--read-only"], None, true))), PodVerdict::Replace), "a member's bench running the reader");
        assert!(matches!(bench_state(&running, Some(&pod_with(&["harness-bench"], None, true))), PodVerdict::Ready));
        assert!(matches!(bench_state(&running, Some(&pod_with(&["harness-bench"], None, false))), PodVerdict::Starting));
        match bench_state(&running, Some(&pod_with(&["harness-bench"], Some((75, "node-b")), false))) {
            PodVerdict::Locked(h) => assert_eq!(h, "node-b"),
            v => panic!("exit 75 is a held lock: {v:?}"),
        }
        let mut exited = pod_with(&["harness-bench"], Some((0, "idle")), false);
        exited.status.as_mut().unwrap().phase = Some("Succeeded".into());
        match bench_state(&running, Some(&exited)) {
            PodVerdict::Idle(at) => assert_eq!(at, FINISHED_AT),
            v => panic!("exit 0 is asleep: {v:?}"),
        }
        let mut asleep = fixture_bench(DesiredState::Running);
        asleep.status.as_mut().unwrap().idle_since = Some(FINISHED_AT.into());
        assert!(matches!(bench_state(&asleep, None), PodVerdict::Absent), "nobody asked");
        asleep.spec.wake_at = Some("2099-01-01T00:00:00Z".into());
        assert!(matches!(bench_state(&asleep, None), PodVerdict::Create), "a client asked after it slept");
        let stopped = fixture_bench(DesiredState::Stopped);
        assert!(matches!(bench_state(&stopped, Some(&pod_with(&["harness-bench"], None, true))), PodVerdict::Remove));
        assert!(matches!(bench_state(&stopped, None), PodVerdict::Absent));
        let mut departed = fixture_bench(DesiredState::Running);
        departed.spec.access = crd::BenchAccess::ReadOnly;
        assert!(matches!(bench_state(&departed, Some(&pod_with(&["harness-bench"], None, true))), PodVerdict::Replace), "tools stop when the owner leaves");
    }

    #[test]
    fn an_exit_without_finished_at_sleeps_from_the_ready_transition_or_keeps_starting() {
        let running = fixture_bench(DesiredState::Running);
        let mut exited = pod_with(&["harness-bench"], None, false);
        let st = exited.status.as_mut().unwrap();
        st.phase = Some("Succeeded".into());
        st.container_statuses.as_mut().unwrap()[0].state =
            Some(ContainerState { terminated: Some(ContainerStateTerminated { exit_code: 0, ..Default::default() }), ..Default::default() });
        assert!(matches!(bench_state(&running, Some(&exited)), PodVerdict::Starting), "no clock at all is not an empty idleSince");
        exited.status.as_mut().unwrap().conditions.as_mut().unwrap()[0].last_transition_time =
            Some(serde_json::from_value(serde_json::json!(FINISHED_AT)).unwrap());
        match bench_state(&running, Some(&exited)) {
            PodVerdict::Idle(at) => assert_eq!(at, FINISHED_AT),
            v => panic!("the Ready transition is the clock: {v:?}"),
        }
    }
}
