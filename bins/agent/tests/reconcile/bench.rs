//! A bench reconciled as a WORKSPACE: the second container, the idle clock, the held folder and the
//! pause — against the mocked API server.
//!
//! There is no bench reconciler any more. `crd::is_bench` is the only predicate and
//! `crd::wants_pod` the only pod decision, so everything here goes through `apply_workspace`.

use super::*;

const POD: &str = "/api/v1/namespaces/ws-alice/pods/ws-1";
const PODS: &str = "/api/v1/namespaces/ws-alice/pods";
const AT: &str = "2026-09-13T10:00:00Z";

/// `ws_json`'s workspace, flagged as a bench, with whatever status the pass should start from.
fn bench_ws(spec: serde_json::Value, status: serde_json::Value) -> crd::Workspace {
    let mut o = ws_json(status);
    o["spec"]["bench"] = serde_json::json!({"model": "m"});
    o["spec"]["name"] = serde_json::json!("bench");
    for (k, v) in spec.as_object().unwrap() {
        o["spec"][k] = v.clone();
    }
    serde_json::from_value(o).unwrap()
}

fn creating() -> serde_json::Value {
    serde_json::json!({"phase": "creating", "nodeName": "node-a"})
}

/// A bench pod as the kubelet reports it: `ready` is the `bench` container's readiness (the idle
/// channel), `running` false stands for a restarting container, and `exit` is its last exit.
fn bench_pod(ready: bool, running: bool, exit: Option<i32>, since: &str) -> serde_json::Value {
    let state = if running {
        serde_json::json!({"running": {"startedAt": since}})
    } else {
        serde_json::json!({"waiting": {"reason": "CrashLoopBackOff"}})
    };
    serde_json::json!({
        "apiVersion": "v1", "kind": "Pod", "metadata": {"name": "ws-1", "namespace": "ws-alice"},
        "spec": {"nodeName": "node-a", "containers": []},
        "status": {
            "phase": "Running", "podIP": "10.1.2.3",
            "conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" }, "lastTransitionTime": since}],
            "containerStatuses": [{
                // `started` is the startup probe's flip; `bench_pod` is the already-serving shape,
                // and the never-started one is built inline by its own test below.
                "name": "sessions", "ready": ready, "started": true, "restartCount": 0, "image": "b", "imageID": "",
                "state": state,
                "lastState": exit.map(|c| serde_json::json!({"terminated": {"exitCode": c, "message": "node-b", "finishedAt": since}})).unwrap_or(serde_json::Value::Null),
            }],
        },
    })
}

fn delete_pod() -> Route {
    Route { method: "DELETE", path: POD.into(), status: 200, body: serde_json::json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "ws-1"}}) }
}

fn last_status(rec: &Recorder) -> serde_json::Value {
    rec.sent("PATCH", WS_STATUS).last().expect("a status write")["status"].clone()
}

fn has_cond(st: &serde_json::Value, t: &str, status: &str, reason: &str) -> bool {
    st["conditions"].as_array().is_some_and(|cs| cs.iter().any(|c| c["type"] == t && c["status"] == status && c["reason"] == reason))
}

/// A bench is a workspace pod plus the `bench` container, stamped with the agent's configured
/// image and the region's `benchIdleSecs` — neither of which is a spec field.
#[tokio::test]
async fn a_running_bench_gets_one_pod_with_both_containers() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = ssh_routes();
    // Twice: the start's capacity gate asks whether a pod exists before `create_if_absent` does.
    routes.push(kloudlite_workspaces::kube_test::not_found(POD));
    routes.push(kloudlite_workspaces::kube_test::not_found(POD));
    routes.push(kloudlite_workspaces::kube_test::get(POD, bench_pod(true, true, None, AT)));
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);
    let idle_secs = ctx.settings.load().bench_idle_secs.to_string();

    apply_until_settled(&bench_ws(serde_json::json!({}), creating()), &ctx).await;

    let sent = rec.sent("POST", PODS);
    assert_eq!(sent.len(), 1, "{:?}", rec.calls());
    let names: Vec<_> = sent[0]["spec"]["containers"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    // A bench pod is the SESSIONS container and a terminal — no workspace container at all since
    // 2026-09-17 (spec §2.2), so nothing on it serves tools, sshd or code.
    assert_eq!(names, vec!["sessions", "shell"], "{:?}", sent[0]["spec"]["containers"]);
    let bench = &sent[0]["spec"]["containers"][0];
    assert_eq!(bench["image"], ctx.bench_image.as_str());
    let env = bench["env"].as_array().unwrap();
    assert!(env.iter().any(|e| e["name"] == "KL_BENCH_IDLE_SECS" && e["value"] == idle_secs.as_str()), "{env:?}");
    assert_eq!(sent[0]["metadata"]["labels"]["kloudlite.io/kind"], "bench");
    assert_eq!(last_status(&rec)["phase"], "ready");
}

/// A container that has never passed its startup probe is STARTING, never asleep — otherwise a
/// bench that runs but cannot serve is declared idle after 15 s, its pod deleted, and the fault
/// hidden behind a phase that reads as normal on every wake.
#[tokio::test]
async fn a_bench_that_never_started_is_not_idle() {
    let tmp = tempfile::tempdir().unwrap();
    let mut pod = bench_pod(false, true, None, &rfc3339_ago(600));
    pod["status"]["containerStatuses"][0]["started"] = serde_json::json!(false);
    let mut routes = ssh_routes();
    routes.push(kloudlite_workspaces::kube_test::not_found(POD));
    routes.push(kloudlite_workspaces::kube_test::not_found(POD));
    routes.push(kloudlite_workspaces::kube_test::get(POD, pod));
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);

    apply_until_settled(&bench_ws(serde_json::json!({}), creating()), &ctx).await;

    assert!(rec.sent("DELETE", POD).is_empty(), "{:?}", rec.calls());
    assert_ne!(last_status(&rec)["phase"], "idle", "{}", last_status(&rec));
}

/// The idle channel is READINESS now, not a pod exit: the container keeps serving, `--ping` says
/// asleep, and the pass deletes the pod and stamps the pod's own clock as `idleSince`.
#[tokio::test]
async fn an_idle_bench_loses_its_pod_and_only_a_later_wake_brings_it_back() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = ssh_routes();
    // Twice: the start's capacity gate asks whether a pod exists before `create_if_absent` does.
    routes.push(kloudlite_workspaces::kube_test::not_found(POD));
    routes.push(kloudlite_workspaces::kube_test::not_found(POD));
    routes.push(kloudlite_workspaces::kube_test::get(POD, bench_pod(false, true, None, &rfc3339_ago(600))));
    routes.push(delete_pod());
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);
    apply_until_settled(&bench_ws(serde_json::json!({}), creating()), &ctx).await;

    assert_eq!(rec.sent("DELETE", POD).len(), 1, "{:?}", rec.calls());
    let st = last_status(&rec);
    assert_eq!(st["phase"], "idle");
    assert!(has_cond(&st, "Ready", "False", "Idle"), "{st}");
    assert!(st["idleSince"].is_string(), "{st}");

    // Asleep and nobody asked: no pod at all, and the pass never reaches the volume.
    let tmp2 = tempfile::tempdir().unwrap();
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp2.path(), vec![delete_pod()]);
    kloudlite_agent::controller::apply_workspace(&bench_ws(serde_json::json!({}), st.clone()), &ctx).await.unwrap();
    assert!(rec.sent("POST", PODS).is_empty(), "{:?}", rec.calls());
    // Converged: the same Idle status computed again is not rewritten, and nothing but the pod
    // delete is even attempted — no volume, no namespace, no profile.
    assert!(rec.sent("PATCH", WS_STATUS).is_empty(), "{:?}", rec.calls());

    // A wake stamped after it slept starts one again.
    let tmp3 = tempfile::tempdir().unwrap();
    let mut routes = ssh_routes();
    // Twice: the start's capacity gate asks whether a pod exists before `create_if_absent` does.
    routes.push(kloudlite_workspaces::kube_test::not_found(POD));
    routes.push(kloudlite_workspaces::kube_test::not_found(POD));
    routes.push(kloudlite_workspaces::kube_test::get(POD, bench_pod(true, true, None, AT)));
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp3.path(), routes);
    let woken = bench_ws(serde_json::json!({"bench": {"model": "m", "wakeAt": "2099-01-01T00:00:00Z"}}), st);
    apply_until_settled(&woken, &ctx).await;
    assert_eq!(rec.sent("POST", PODS).len(), 1, "{:?}", rec.calls());
}

/// Exit 75 is another pod holding the folder; the kubelet restarts it with backoff and the person
/// is told who has it.
#[tokio::test]
async fn a_held_folder_reports_the_holder_and_keeps_the_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = ssh_routes();
    // Twice: the start's capacity gate asks whether a pod exists before `create_if_absent` does.
    routes.push(kloudlite_workspaces::kube_test::not_found(POD));
    routes.push(kloudlite_workspaces::kube_test::not_found(POD));
    routes.push(kloudlite_workspaces::kube_test::get(POD, bench_pod(false, false, Some(75), &rfc3339_ago(600))));
    routes.push(delete_pod());
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);
    apply_until_settled(&bench_ws(serde_json::json!({}), creating()), &ctx).await;

    assert!(rec.sent("DELETE", POD).is_empty(), "a held lock is not an idle pod: {:?}", rec.calls());
    let st = last_status(&rec);
    assert_eq!(st["phase"], "starting");
    assert!(has_cond(&st, "Ready", "False", "FolderLocked"), "{st}");
    assert!(st["conditions"].as_array().unwrap().iter().any(|c| c["message"].as_str().is_some_and(|m| m.contains("node-b"))), "{st}");
}

/// A paused member's bench loses its pod and says so — and `Paused` is not `Idle`, because a
/// connection cannot wake it.
#[tokio::test]
async fn a_paused_bench_has_no_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), vec![delete_pod()]);
    let paused = bench_ws(serde_json::json!({"access": "paused"}), creating());
    kloudlite_agent::controller::apply_workspace(&paused, &ctx).await.unwrap();

    assert_eq!(rec.sent("DELETE", POD).len(), 1, "{:?}", rec.calls());
    assert!(rec.sent("POST", PODS).is_empty(), "{:?}", rec.calls());
    let st = last_status(&rec);
    assert_eq!(st["phase"], "stopped");
    assert!(has_cond(&st, "Ready", "False", "Paused"), "{st}");
}


/// I2: idle is a bench's NORMAL resting state, so the park pass must RECOMPUTE `Replicated` rather
/// than keep the running pass's `False/Running`. Kept, the volume decision sweep reads "waiting for
/// a replica" forever: the bench never comes back after a node death and the node can never drain.
#[tokio::test]
async fn an_idle_bench_recomputes_replicated_so_its_volume_can_be_released() {
    let idle = serde_json::json!({
        "phase": "idle", "nodeName": "node-a", "volumeRef": "vol-1",
        "idleSince": rfc3339_ago(600),
        "conditions": [{"type": "Replicated", "status": "False", "reason": "Running",
                        "message": "running here; its live edits are on this node only",
                        "lastTransitionTime": AT}],
    });
    for (held, status, reason) in
        [("stop-ws-1-3", "True", "Replicated"), ("sync-ws-1-old", "False", "AwaitingReplica")]
    {
        let tmp = tempfile::tempdir().unwrap();
        let mut routes = vec![delete_pod()];
        routes.extend(super::the_stop_before_teardown_snapshot::replicated_routes(held));
        let (ctx, rec, _nix) = ws_ctx_with_ssh(tmp.path(), routes);

        kloudlite_agent::controller::apply_workspace(&bench_ws(serde_json::json!({}), idle.clone()), &ctx)
            .await
            .unwrap();

        let st = last_status(&rec);
        assert!(has_cond(&st, "Replicated", status, reason), "{held}: {st}");
    }
}
