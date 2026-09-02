//! `/v1` push/history/refs served from the commit model — the only model there is (Task 8).

use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{router, ApiState};
use rustic_git_workspaces::kube_test::{get, mock_client, not_found, Recorder, Route};
use rustic_git_workspaces::store::{MemStore, MetaStore};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

const API: &str = "/apis/rustic-git.io/v1alpha1";
const NODE: &str = "node-a";

struct Server {
    base: String,
    jwt: Arc<Jwt>,
    rec: Recorder,
}

fn placed_ws(name: &str, owner: &str) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": name, "uid": format!("uid-{name}"), "labels": {"rustic-git.io/owner": owner}},
        "spec": {
            "owner": owner, "team": "", "name": name, "region": "centralindia", "image": "nginx:alpine",
            "storage": {"quotaGb": 20}, "desiredState": "running"
        },
        "status": {"phase": "ready", "nodeName": NODE, "compatibleNodes": [NODE], "volumeRef": name}
    })
}

fn placed_ws_with_head(name: &str, owner: &str, head: &str) -> Value {
    let mut w = placed_ws(name, owner);
    w["status"]["head"] = json!(head);
    w
}

fn no_workspaces() -> Route {
    get(format!("{API}/workspaces"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "WorkspaceList", "metadata": {}, "items": []}))
}

fn placed_env(name: &str, owner: &str) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Environment",
        "metadata": {"name": name, "uid": format!("uid-{name}"), "labels": {"rustic-git.io/owner": owner}},
        "spec": {
            "owner": owner, "name": name, "region": "centralindia", "services": [],
            "storage": {"quotaGb": 20}, "desiredState": "running"
        },
        "status": {"phase": "running", "nodeName": NODE, "compatibleNodes": [NODE], "volumeRef": name}
    })
}

fn snapshot(name: &str, volume: &str, owner: &str, worktree: &str, parent: &str, phase: &str) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
        "metadata": {"name": name},
        "spec": {"volume": volume, "owner": owner, "worktree": worktree, "parent": parent, "pinned": false},
        "status": {"phase": phase},
    })
}

fn token(jwt: &Jwt, username: &str) -> String {
    jwt.mint(&format!("{username}@example.com"), "Test User", Some(username)).unwrap()
}

async fn server(routes: Vec<Route>) -> Server {
    let store = Arc::new(MemStore::new());
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, rec) = mock_client(routes);
    let state = ApiState::new(store as Arc<dyn MetaStore>, jwt.clone(), HashSet::new()).with_kube(client);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt, rec }
}

/// A push under the flag creates a `Working` `Snapshot` naming the workspace as `worktree` and
/// the workspace's current `head` as `parent` — never a `SnapshotRequest`.
#[tokio::test]
async fn push_creates_a_working_snapshot_with_worktree_and_parent() {
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws_with_head("ws-1", "karthik", "ws-1-aaaaaaaa")),
        get(format!("{API}/snapshots"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": []})),
        get(format!("{API}/volumes/ws-1"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume", "metadata": {"name": "ws-1", "uid": "vol-uid-1"}, "spec": {"owner": "karthik", "nodeName": "node-a", "region": "r1", "quotaGb": 5}})),
        Route { method: "POST", path: format!("{API}/snapshots"), status: 201, body: snapshot("ws-1-cccccccc", "ws-1", "karthik", "ws-1", "", "working") },
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/push", s.base))
        .bearer_auth(&tok)
        .json(&json!({"message": "checkpoint"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());

    let req = s.rec.sent("POST", &format!("{API}/snapshots")).remove(0);
    assert_eq!(req["spec"]["volume"], "ws-1");
    assert_eq!(req["spec"]["worktree"], "ws-1");
    assert_eq!(req["spec"]["parent"], "ws-1-aaaaaaaa");
    assert_eq!(req["spec"]["message"], "checkpoint");
    assert_eq!(req["spec"]["owner"], "karthik");
    assert_eq!(req["status"]["phase"], "working");
    // H2: the label is a VIEW of `spec.volume`/`spec.owner` — the e2e's `-l rustic-git.io/volume=`
    // selects on exactly this, and nothing else stamps it.
    assert_eq!(req["metadata"]["labels"]["rustic-git.io/volume"], "ws-1");
    assert_eq!(req["metadata"]["labels"]["rustic-git.io/owner"], "karthik");
    // Owned by the Volume: the record is garbage-collected with it instead of outliving a deleted workspace.
    assert_eq!(req["metadata"]["ownerReferences"][0]["kind"], "Volume");
    assert_eq!(req["metadata"]["ownerReferences"][0]["uid"], "vol-uid-1");
    assert!(!s.rec.calls().iter().any(|c| c.contains("snapshotrequests")), "no SnapshotRequest under the flag");
}

/// A workspace with no recorded head yet (its first push) writes an EMPTY parent — the root of a
/// new chain.
#[tokio::test]
async fn first_push_of_a_workspace_has_no_parent() {
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "karthik")),
        get(format!("{API}/snapshots"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": []})),
        get(format!("{API}/volumes/ws-1"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume", "metadata": {"name": "ws-1", "uid": "vol-uid-1"}, "spec": {"owner": "karthik", "nodeName": "node-a", "region": "r1", "quotaGb": 5}})),
        Route { method: "POST", path: format!("{API}/snapshots"), status: 201, body: snapshot("ws-1-cccccccc", "ws-1", "karthik", "ws-1", "", "working") },
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/push", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let req = s.rec.sent("POST", &format!("{API}/snapshots")).remove(0);
    assert_eq!(req["spec"]["parent"], "");
}

/// `/history` walks `Snapshot` CRs, NEWEST first (F2: matches the registry path's order),
/// parent-linked — no registry round trip.
#[tokio::test]
async fn history_lists_snapshot_crs_in_parent_order() {
    let mut root = snapshot("ws-1-aaaaaaaa", "ws-1", "karthik", "ws-1", "", "ready");
    root["metadata"]["creationTimestamp"] = json!("2026-01-01T00:00:00Z");
    let mut tip = snapshot("ws-1-bbbbbbbb", "ws-1", "karthik", "ws-1", "ws-1-aaaaaaaa", "working");
    tip["metadata"]["creationTimestamp"] = json!("2026-01-02T00:00:00Z");
    let routes = vec![get(
        format!("{API}/snapshots"),
        json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": [tip, root]}),
    )];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/volumes/ws-1/history", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    let rows: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], "ws-1-bbbbbbbb");
    assert_eq!(rows[0]["parent"], "ws-1-aaaaaaaa");
    assert_eq!(rows[1]["id"], "ws-1-aaaaaaaa");
    assert_eq!(rows[1]["parent"], Value::Null);
}

/// `/refs` names the newest commit as `main` — same "first = tip" convention the registry path
/// keeps, computed here from creation order instead.
#[tokio::test]
async fn refs_names_the_newest_commit_as_main() {
    let mut root = snapshot("ws-1-aaaaaaaa", "ws-1", "karthik", "ws-1", "", "ready");
    root["metadata"]["creationTimestamp"] = json!("2026-01-01T00:00:00Z");
    let mut tip = snapshot("ws-1-bbbbbbbb", "ws-1", "karthik", "ws-1", "ws-1-aaaaaaaa", "ready");
    tip["metadata"]["creationTimestamp"] = json!("2026-01-02T00:00:00Z");
    let routes = vec![get(
        format!("{API}/snapshots"),
        json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": [root, tip]}),
    )];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/volumes/ws-1/refs", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["main"], "ws-1-bbbbbbbb");
}

/// A caller reading a volume that has no `Snapshot` under any owner label they may read gets a
/// 404 — same "not found" the registry path returns for a volume that is not theirs.
#[tokio::test]
async fn history_of_an_unknown_volume_is_not_found() {
    let routes = vec![get(
        format!("{API}/snapshots"),
        json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": []}),
    )];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/volumes/ws-1/history", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

/// Restore-in-place under the flag names a `Snapshot` CR of THIS environment's own volume — an
/// unknown id is a 404, same as every other "no such snapshot" case.
#[tokio::test]
async fn restore_in_place_of_an_unknown_commit_is_not_found() {
    let env = placed_env("env-1", "karthik");
    let routes = vec![
        get(format!("{API}/environments/env-1"), env),
        not_found(format!("{API}/snapshots/no-such")),
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/environments/env-1/restore-in-place", s.base))
        .bearer_auth(&tok)
        .json(&json!({"snapshot_id": "no-such"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert!(!s.rec.calls().iter().any(|c| c.contains(&format!("PATCH {API}/environments/env-1"))), "no wish written on a bad id");
}

/// A commit still `Working` (not yet `Ready`) cannot be restored onto — the swap would have
/// nothing finished to check out.
#[tokio::test]
async fn restore_in_place_of_a_not_ready_commit_is_not_found() {
    let env = placed_env("env-1", "karthik");
    let snap = snapshot("env-1-aaaaaaaa", "env-1", "karthik", "env-1", "", "working");
    let routes = vec![get(format!("{API}/environments/env-1"), env), get(format!("{API}/snapshots/env-1-aaaaaaaa"), snap)];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/environments/env-1/restore-in-place", s.base))
        .bearer_auth(&tok)
        .json(&json!({"snapshot_id": "env-1-aaaaaaaa"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

/// A `Ready` commit of a DIFFERENT volume is a foreign commit — restoring onto it here would put
/// another environment's bytes under these services, which is what `clone`/`restore`-into-a-new-
/// object are for, not an in-place restore.
#[tokio::test]
async fn restore_in_place_of_a_foreign_commit_is_not_found() {
    let env = placed_env("env-1", "karthik");
    let snap = snapshot("env-2-aaaaaaaa", "env-2", "karthik", "env-2", "", "ready");
    let routes = vec![get(format!("{API}/environments/env-1"), env), get(format!("{API}/snapshots/env-2-aaaaaaaa"), snap)];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/environments/env-1/restore-in-place", s.base))
        .bearer_auth(&tok)
        .json(&json!({"snapshot_id": "env-2-aaaaaaaa"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

/// A `Ready` commit of the environment's own volume writes the wish exactly as the old path did —
/// the controllers do the checkout-swap, not this handler.
#[tokio::test]
async fn restore_in_place_of_a_valid_commit_writes_the_wish() {
    let env = placed_env("env-1", "karthik");
    let snap = snapshot("env-1-aaaaaaaa", "env-1", "karthik", "env-1", "", "ready");
    let routes = vec![
        get(format!("{API}/environments/env-1"), env),
        get(format!("{API}/snapshots/env-1-aaaaaaaa"), snap),
        Route {
            method: "PATCH",
            path: format!("{API}/environments/env-1"),
            status: 200,
            body: placed_env("env-1", "karthik"),
        },
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/environments/env-1/restore-in-place", s.base))
        .bearer_auth(&tok)
        .json(&json!({"snapshot_id": "env-1-aaaaaaaa"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let patch = &s.rec.sent("PATCH", &format!("{API}/environments/env-1"))[0];
    assert_eq!(patch["spec"]["restore"]["snapshotId"], "env-1-aaaaaaaa");
    assert_eq!(patch["spec"]["restore"]["volume"], "env-1");
}

/// F1: a second push while the first is still `Working` is refused before it can create a second
/// racing cut — the loser would become a Ready commit no worktree's `head` ever points at, so
/// retention (which walks only the winner's chain) would never revisit and never delete it.
#[tokio::test]
async fn a_racing_push_while_one_is_still_working_is_refused() {
    let racing = snapshot("ws-1-aaaaaaaa", "ws-1", "karthik", "ws-1", "", "working");
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "karthik")),
        get(
            format!("{API}/snapshots"),
            json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": [racing]}),
        ),
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/push", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409, "{}", resp.text().await.unwrap());
    assert!(!s.rec.calls().iter().any(|c| c.contains(&format!("POST {API}/snapshots"))), "no second CR");
}

/// F2: history stays NEWEST first — the registry path's `records.first()` is always its tip, and
/// a consumer switched over at cutover must see the same order, not a reversed one.
#[tokio::test]
async fn history_stays_newest_first_across_a_three_commit_chain() {
    let mut root = snapshot("ws-1-aaaaaaaa", "ws-1", "karthik", "ws-1", "", "ready");
    root["metadata"]["creationTimestamp"] = json!("2026-01-01T00:00:00Z");
    let mut mid = snapshot("ws-1-bbbbbbbb", "ws-1", "karthik", "ws-1", "ws-1-aaaaaaaa", "ready");
    mid["metadata"]["creationTimestamp"] = json!("2026-01-02T00:00:00Z");
    let mut tip = snapshot("ws-1-cccccccc", "ws-1", "karthik", "ws-1", "ws-1-bbbbbbbb", "ready");
    tip["metadata"]["creationTimestamp"] = json!("2026-01-03T00:00:00Z");
    let routes = vec![get(
        format!("{API}/snapshots"),
        json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": [mid, root, tip]}),
    )];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/volumes/ws-1/history", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let rows: Vec<Value> = resp.json().await.unwrap();
    let ids: Vec<&str> = rows.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["ws-1-cccccccc", "ws-1-bbbbbbbb", "ws-1-aaaaaaaa"], "newest first, same as the registry path");
}

/// F3: `createdAt` is RFC3339 — the registry path's `chrono::DateTime<Utc>` serializes that way,
/// and every consumer already parses it.
#[tokio::test]
async fn history_created_at_is_rfc3339() {
    let mut root = snapshot("ws-1-aaaaaaaa", "ws-1", "karthik", "ws-1", "", "ready");
    root["metadata"]["creationTimestamp"] = json!("2026-01-01T12:34:56Z");
    let routes = vec![get(
        format!("{API}/snapshots"),
        json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": [root]}),
    )];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/volumes/ws-1/history", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    let rows: Vec<Value> = resp.json().await.unwrap();
    let created_at = rows[0]["createdAt"].as_str().unwrap();
    chrono::DateTime::parse_from_rfc3339(created_at).unwrap_or_else(|e| panic!("{created_at:?} is not RFC3339: {e}"));
}

/// F6: `/refs` on a volume with zero commits (never pushed) is `{"main": null}`, the same shape
/// the registry path answers with — never 404, which would read as "no such volume" instead of
/// "no commits yet".
#[tokio::test]
async fn refs_of_a_zero_commit_volume_is_null_not_not_found() {
    let routes = vec![get(
        format!("{API}/snapshots"),
        json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": []}),
    )];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/volumes/ws-1/refs", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["main"], Value::Null);
}

fn stopped_ws_replicated(name: &str, owner: &str, status: &str, reason: &str, message: &str) -> Value {
    let mut w = placed_ws(name, owner);
    w["status"]["phase"] = json!("stopped");
    w["status"]["conditions"] = json!([{
        "type": "Replicated", "status": status, "reason": reason, "message": message,
        "lastTransitionTime": "2026-09-03T10:00:00Z", "observedGeneration": 3
    }]);
    w
}

/// The condition the owner wrote is what `/v1` answers with, verbatim: the UI's "safe to start
/// anywhere" vs "still copying" is that one field, and re-deriving it here would be a second
/// truth that can disagree with the node's.
#[tokio::test]
async fn get_and_stop_expose_the_replicated_condition() {
    let ws = stopped_ws_replicated("ws-1", "karthik", "False", "AwaitingReplica", "no other node holds the final sync point yet");
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), ws.clone()),
        get(format!("{API}/snapshots"), json!({"apiVersion": "v1", "kind": "SnapshotList", "metadata": {}, "items": []})),
        Route { method: "PATCH", path: format!("{API}/workspaces/ws-1"), status: 200, body: ws },
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let http = reqwest::Client::new();

    let got: Value = http.get(format!("{}/v1/workspaces/ws-1", s.base)).bearer_auth(&tok).send().await.unwrap().json().await.unwrap();
    assert_eq!(got["replicated"]["ready"], false);
    assert_eq!(got["replicated"]["message"], "no other node holds the final sync point yet");

    let stopped: Value = http.post(format!("{}/v1/workspaces/ws-1/stop", s.base)).bearer_auth(&tok).send().await.unwrap().json().await.unwrap();
    assert_eq!(stopped["replicated"]["reason"], "AwaitingReplica");
}

fn interrupted_ws(name: &str, owner: &str) -> Value {
    let mut w = placed_ws(name, owner);
    w["status"]["conditions"] = json!([{
        "type": "Degraded", "status": "True", "reason": "NodeDead",
        "message": "node node-a is down", "lastTransitionTime": "2026-09-03T10:00:00Z"
    }]);
    w
}

/// There is no way to start an interrupted parent elsewhere and no way to abandon its edits:
/// reaching that state is a system failure, never a workflow. The 409 says what will happen
/// instead of leaving a start silently pending forever.
#[tokio::test]
async fn starting_an_interrupted_workspace_is_a_409_that_explains_itself() {
    let routes = vec![get(format!("{API}/workspaces/ws-1"), interrupted_ws("ws-1", "karthik"))];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");

    let r = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/start", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    assert_eq!(
        r.text().await.unwrap(),
        "workspace is interrupted: its node is down; it resumes when the node returns"
    );
    assert!(
        !s.rec.calls().iter().any(|c| c.starts_with("PATCH")),
        "and desiredState is never flipped: {:?}",
        s.rec.calls()
    );
}

#[tokio::test]
async fn starting_an_interrupted_environment_is_a_409_too() {
    let mut e = placed_env("env-1", "karthik");
    e["status"]["conditions"] = json!([{
        "type": "Degraded", "status": "True", "reason": "NodeDead",
        "message": "node node-a is down", "lastTransitionTime": "2026-09-03T10:00:00Z"
    }]);
    let routes = vec![get(format!("{API}/environments/env-1"), e)];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let r = reqwest::Client::new().post(format!("{}/v1/environments/env-1/start", s.base)).bearer_auth(&tok).send().await.unwrap();
    assert_eq!(r.status(), 409);
    assert_eq!(r.text().await.unwrap(), "environment is interrupted: its node is down; it resumes when the node returns");
}

/// A stopped parent on a dead node is NOT interrupted — it was flushed on the way down, so it
/// starts as soon as an up-to-date node claims it. Only a parent carrying `NodeDead` is refused.
#[tokio::test]
async fn a_plain_stopped_workspace_still_starts() {
    let mut w = placed_ws("ws-1", "karthik");
    w["status"]["phase"] = json!("stopped");
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), w.clone()),
        Route { method: "PATCH", path: format!("{API}/workspaces/ws-1"), status: 200, body: w },
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let r = reqwest::Client::new().post(format!("{}/v1/workspaces/ws-1/start", s.base)).bearer_auth(&tok).send().await.unwrap();
    assert_eq!(r.status(), 202);
}

/// The sync-beat shape a clone grafts onto: a Ready TRANSIENT of the source worktree. `snapshot()`
/// builds commits, and the ordering rule ignores anything that is not transient.
fn transient(name: &str, volume: &str, owner: &str, worktree: &str) -> Value {
    let mut s = snapshot(name, volume, owner, worktree, "", "ready");
    s["spec"]["transient"] = json!(true);
    s["status"]["readyAt"] = json!("2026-09-01T14:32:07Z");
    s
}

/// A clone cuts a snapshot NOW rather than leaning on whatever the last beat happened to leave:
/// `clone-{ws}-{hex}`, a transient, parented on the source's newest sync point so the puller
/// sends a delta. The clone's spec then names that cut, not the source's `head`.
#[tokio::test]
async fn clone_cuts_a_transient_and_bases_the_clone_on_it() {
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws_with_head("ws-1", "karthik", "ws-1-aaaaaaaa")),
        no_workspaces(),
        get(format!("{API}/snapshots"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": [
            transient("sync-ws-1-bbbb", "ws-1", "karthik", "ws-1")
        ]})),
        get(format!("{API}/volumes/ws-1"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
            "spec": {"owner": "karthik", "nodeName": "node-a", "region": "r1", "quotaGb": 5}})),
        Route { method: "POST", path: format!("{API}/snapshots"), status: 201,
                body: snapshot("clone-ws-1-cafe", "ws-1", "karthik", "ws-1", "sync-ws-1-bbbb", "working") },
        Route { method: "POST", path: format!("{API}/workspaces"), status: 201, body: placed_ws("ws-2", "karthik") },
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");

    let r = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/clone", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "copy"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);

    let cut = s.rec.sent("POST", &format!("{API}/snapshots")).remove(0);
    assert!(cut["metadata"]["name"].as_str().unwrap().starts_with("clone-ws-1-"), "{cut}");
    assert_eq!(cut["spec"]["transient"], true, "a clone cut is a sync point, not a commit in anyone's history");
    assert_eq!(cut["spec"]["parent"], "sync-ws-1-bbbb", "parented on the newest transient so the send is a delta");
    assert_eq!(cut["spec"]["worktree"], "ws-1");
    assert_eq!(cut["metadata"]["ownerReferences"][0]["name"], "ws-1", "owned by the source: deleting it is the whole delete");

    let made = s.rec.sent("POST", &format!("{API}/workspaces")).remove(0);
    assert_eq!(made["spec"]["storage"]["source"]["cloneOf"]["commit"], cut["metadata"]["name"]);

    // basedOn is on EVERY clone response, not only an interrupted one: a clone is always based
    // on a cut, and the person is always entitled to know which.
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["based_on"]["snapshot"], cut["metadata"]["name"]);
    assert_eq!(body["based_on"]["interrupted"], false);
    assert_eq!(body["based_on"]["age_seconds"], 0, "a cut made by this very request is zero seconds old");
    assert!(!s.rec.calls().iter().any(|c| c.starts_with(&format!("POST {API}/volumes"))), "no child Volume for a clone");
}

/// A source that has never pushed and never synced is still cloneable — the cut this request makes
/// IS its first snapshot, with an empty parent exactly as a root commit has.
#[tokio::test]
async fn a_never_snapshotted_source_is_cloneable_and_the_cut_is_its_root() {
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "karthik")),
        no_workspaces(),
        get(format!("{API}/snapshots"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": []})),
        get(format!("{API}/volumes/ws-1"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
            "spec": {"owner": "karthik", "nodeName": "node-a", "region": "r1", "quotaGb": 5}})),
        Route { method: "POST", path: format!("{API}/snapshots"), status: 201,
                body: snapshot("clone-ws-1-cafe", "ws-1", "karthik", "ws-1", "", "working") },
        Route { method: "POST", path: format!("{API}/workspaces"), status: 201, body: placed_ws("ws-2", "karthik") },
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let r = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/clone", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "copy"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202, "no head is no longer a 400: the cut is the commit");
    let cut = s.rec.sent("POST", &format!("{API}/snapshots")).remove(0);
    assert_eq!(cut["spec"]["parent"], "");
}

/// One node's replica row, saying which transient of each worktree it HOLDS.
fn replica_holding(node: &str, volume: &str, worktree: &str, held: &str) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplica",
        "metadata": {"name": format!("{volume}.{node}")},
        "spec": {"volume": volume, "node": node},
        "status": {"phase": "Synced", "branches": {worktree: held}},
    })
}

fn replicas(items: Vec<Value>) -> Route {
    get(format!("{API}/volumereplicas"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "VolumeReplicaList", "metadata": {}, "items": items}))
}

/// Cloning an INTERRUPTED source is allowed — it is the one way forward — and the response says
/// exactly what it is based on, so choosing it is choosing the gap knowingly.
#[tokio::test]
async fn cloning_an_interrupted_source_is_allowed_and_states_the_cut_it_used() {
    let mut src = interrupted_ws("ws-1", "karthik");
    src["status"]["head"] = json!("ws-1-aaaaaaaa");
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), src),
        no_workspaces(),
        get(format!("{API}/snapshots"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": [
            transient("sync-ws-1-bbbb", "ws-1", "karthik", "ws-1")
        ]})),
        replicas(vec![replica_holding("node-b", "ws-1", "ws-1", "sync-ws-1-bbbb")]),
        get(format!("{API}/volumes/ws-1"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
            "spec": {"owner": "karthik", "nodeName": "node-a", "region": "r1", "quotaGb": 5}})),
        Route { method: "POST", path: format!("{API}/workspaces"), status: 201, body: placed_ws("ws-2", "karthik") },
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let r = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/clone", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "copy"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    // No new cut: the owner is down, so the newest transient a peer HOLDS is the only thing to
    // graft onto — and that is exactly what the response names.
    assert!(s.rec.sent("POST", &format!("{API}/snapshots")).is_empty(), "an interrupted source cannot be cut");
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["based_on"]["snapshot"], "sync-ws-1-bbbb");
    assert_eq!(body["based_on"]["interrupted"], true);
    assert_eq!(body["based_on"]["at"], "2026-09-01T14:32:07Z", "the age the person weighs is the cut's own readyAt");
    assert!(body["based_on"]["age_seconds"].as_i64().unwrap() > 0);
}

/// An interrupted source with nothing synced anywhere has no way forward at all — a 409 that says
/// so beats a clone of an empty worktree indistinguishable from a real one.
#[tokio::test]
async fn cloning_an_interrupted_source_with_no_sync_point_is_a_409() {
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), interrupted_ws("ws-1", "karthik")),
        no_workspaces(),
        get(format!("{API}/snapshots"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": []})),
        replicas(vec![]),
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let r = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/clone", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "copy"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
}

/// The cut the DEAD owner turned Ready seconds before it died exists as a CR and is the newest
/// transient cluster-wide — but its bytes are only on the dead node. Grafting onto it would leave
/// the clone unplaceable forever, so `/v1` chooses among what a `VolumeReplica` reports HOLDING,
/// which is the same field placement reads.
#[tokio::test]
async fn an_interrupted_clone_skips_a_transient_no_live_node_holds() {
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), interrupted_ws("ws-1", "karthik")),
        no_workspaces(),
        get(format!("{API}/snapshots"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": [
            transient("sync-ws-1-old", "ws-1", "karthik", "ws-1"),
            // Newest cluster-wide, and held by nobody but the corpse.
            transient("sync-ws-1-last", "ws-1", "karthik", "ws-1")
        ]})),
        replicas(vec![replica_holding("node-b", "ws-1", "ws-1", "sync-ws-1-old")]),
        get(format!("{API}/volumes/ws-1"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
            "spec": {"owner": "karthik", "nodeName": "node-a", "region": "r1", "quotaGb": 5}})),
        Route { method: "POST", path: format!("{API}/workspaces"), status: 201, body: placed_ws("ws-2", "karthik") },
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let r = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/clone", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "copy"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["based_on"]["snapshot"], "sync-ws-1-old", "the newest a LIVE node can serve, not the newest that exists");
}

/// A replica row that names another worktree's branch, or none at all, holds nothing of this one.
#[tokio::test]
async fn an_interrupted_clone_ignores_replicas_of_a_different_worktree() {
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), interrupted_ws("ws-1", "karthik")),
        no_workspaces(),
        get(format!("{API}/snapshots"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": [
            transient("sync-ws-1-bbbb", "ws-1", "karthik", "ws-1")
        ]})),
        replicas(vec![replica_holding("node-b", "ws-1", "ws-other", "sync-ws-other-zzzz")]),
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let r = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/clone", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "copy"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
}

/// Environments clone by the same rule, off their own volume and worktree.
#[tokio::test]
async fn cloning_an_environment_cuts_a_transient_too() {
    let routes = vec![
        get(format!("{API}/environments/env-1"), placed_env("env-1", "karthik")),
        get(format!("{API}/snapshots"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": [
            transient("sync-env-1-bbbb", "env-1", "karthik", "env-1")
        ]})),
        get(format!("{API}/volumes/env-1"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "env-1", "uid": "vol-uid-2"},
            "spec": {"owner": "karthik", "nodeName": "node-a", "region": "r1", "quotaGb": 5}})),
        Route { method: "POST", path: format!("{API}/snapshots"), status: 201,
                body: snapshot("clone-env-1-cafe", "env-1", "karthik", "env-1", "sync-env-1-bbbb", "working") },
        Route { method: "POST", path: format!("{API}/environments"), status: 201, body: placed_env("env-2", "karthik") },
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let r = reqwest::Client::new()
        .post(format!("{}/v1/environments/env-1/clone", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "copy"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let cut = s.rec.sent("POST", &format!("{API}/snapshots")).remove(0);
    assert!(cut["metadata"]["name"].as_str().unwrap().starts_with("clone-env-1-"), "{cut}");
    let made = s.rec.sent("POST", &format!("{API}/environments")).remove(0);
    // `commit` stays null on purpose: an environment clone still COPIES bytes into a fresh child
    // Volume, and a resolved commit would silently flip it onto the workspace's shared-worktree
    // path. The local copy is at least as fresh as the cut, so `based_on` is still honest.
    assert!(made["spec"]["storage"]["source"]["cloneOf"]["commit"].is_null(), "{made}");
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["based_on"]["snapshot"], cut["metadata"]["name"]);
}
