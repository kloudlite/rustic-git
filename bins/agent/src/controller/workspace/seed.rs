//! The git seed init container's outcome, and the one-pod-per-workspace check.

use super::*;


/// Whether the pod exists AND its `Ready` condition is true. A missing pod is "not ready", never an
/// error: that is the normal state between applying it and the kubelet creating it.
/// `Some((reason, message))` when the `git-seed` init container has failed at least once — the
/// clone is retried in place for two minutes and then the pod restarts it, so a waiting
/// `CrashLoopBackOff`/`Error` or a non-zero termination both mean "the repository did not come".
pub(crate) fn seed_failure(pod: &Pod) -> Option<(String, String)> {
    let st = pod.status.as_ref()?.init_container_statuses.as_ref()?.iter().find(|c| c.name == "git-seed")?;
    let state = st.state.as_ref()?;
    let why = if let Some(t) = state.terminated.as_ref().filter(|t| t.exit_code != 0) {
        format!("exited {}", t.exit_code)
    } else if let Some(w) = state.waiting.as_ref().filter(|w| w.reason.as_deref() != Some("PodInitializing")) {
        w.reason.clone().unwrap_or_else(|| "waiting".to_string())
    } else if st.restart_count > 0 {
        format!("restarted {} times", st.restart_count)
    } else {
        return None;
    };
    Some((
        "SeedFailed".to_string(),
        format!("the repository clone has not succeeded ({why}); check that your platform key may read it — the pod's git-seed log has the server's answer"),
    ))
}


/// Ready, and THIS node's. The pod is named after the workspace on every node, so right after a
/// handover the new owner reads the previous node's pod by that name — still Ready, already
/// terminating — and reported `Ready` on itself two seconds before its own pod existed. `/v1`
/// said "ready on session-1", the gateway dialled a dying pod, and an exec into it exited 1.
/// A pod on another node or on its way out is nobody's answer.
pub(crate) fn own_ready_pod(pod: &Pod, me: &str) -> bool {
    pod.spec.as_ref().and_then(|s| s.node_name.as_deref()) == Some(me)
        && pod.metadata.deletion_timestamp.is_none()
        && pod
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
}


#[cfg(test)]
pub(crate) mod own_pod_tests {
    use super::*;

    fn pod(node: Option<&str>, ready: bool, deleting: bool) -> Pod {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "ws-1", "deletionTimestamp": if deleting { serde_json::json!("2026-09-06T18:26:25Z") } else { serde_json::Value::Null }},
            "spec": {"nodeName": node, "containers": []},
            "status": {"conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" }}]},
        }))
        .unwrap()
    }

    #[test]
    fn a_ready_pod_counts_only_on_this_node_and_only_while_it_stays() {
        assert!(own_ready_pod(&pod(Some("node-a"), true, false), "node-a"));
        assert!(!own_ready_pod(&pod(Some("node-a"), false, false), "node-a"), "not ready");
        assert!(!own_ready_pod(&pod(Some("node-b"), true, false), "node-a"), "the previous owner's pod");
        assert!(!own_ready_pod(&pod(Some("node-a"), true, true), "node-a"), "terminating");
        assert!(!own_ready_pod(&pod(None, true, false), "node-a"), "unscheduled");
    }
}


#[cfg(test)]
pub(crate) mod seed_tests {
    use super::*;
    use k8s_openapi::api::core::v1::{ContainerState, ContainerStateTerminated, ContainerStateWaiting, ContainerStatus, PodStatus};

    fn pod_with(name: &str, state: ContainerState, restarts: i32) -> Pod {
        Pod {
            status: Some(PodStatus {
                init_container_statuses: Some(vec![ContainerStatus {
                    name: name.into(),
                    state: Some(state),
                    restart_count: restarts,
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_seed_still_initialising_is_not_a_failure() {
        let p = pod_with("git-seed", ContainerState { waiting: Some(ContainerStateWaiting { reason: Some("PodInitializing".into()), ..Default::default() }), ..Default::default() }, 0);
        assert_eq!(seed_failure(&p), None);
    }

    #[test]
    fn a_crash_looping_seed_is_named() {
        let p = pod_with("git-seed", ContainerState { waiting: Some(ContainerStateWaiting { reason: Some("CrashLoopBackOff".into()), ..Default::default() }), ..Default::default() }, 2);
        let (r, m) = seed_failure(&p).unwrap();
        assert_eq!(r, "SeedFailed");
        assert!(m.contains("CrashLoopBackOff") && m.contains("git-seed"), "{m}");
    }

    #[test]
    fn a_seed_that_exited_non_zero_is_named_and_another_init_container_is_not() {
        let p = pod_with("git-seed", ContainerState { terminated: Some(ContainerStateTerminated { exit_code: 1, ..Default::default() }), ..Default::default() }, 0);
        assert!(seed_failure(&p).unwrap().1.contains("exited 1"));
        let other = pod_with("other", ContainerState { terminated: Some(ContainerStateTerminated { exit_code: 1, ..Default::default() }), ..Default::default() }, 0);
        assert_eq!(seed_failure(&other), None);
    }
}
