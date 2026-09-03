//! `/v1/volumes` — the listing and the two deletes, against a mocked cluster.
//!
//! Everything here reads and writes CRDs: a push is a `Snapshot` CR and nothing else, so a listing
//! that asked the registry's volume index would have gone blind on everything pushed since the
//! commit model landed. The cluster is also what says whether a parent still exists — a display
//! detail for the listing, and a refusal for the deletes.

use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{router, ApiState};
use rustic_git_workspaces::kube_test::{get as kget, mock_client, Recorder, Route};
use rustic_git_workspaces::store::{MemStore, MetaStore};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

const API: &str = "/apis/rustic-git.io/v1alpha1";
const SNAPS: &str = "/apis/rustic-git.io/v1alpha1/snapshots";
const NODE: &str = "node-a";

struct Server {
    base: String,
    jwt: Arc<Jwt>,
    rec: Recorder,
}

fn ws_obj(name: &str, owner: &str, display: &str) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": name, "labels": {"rustic-git.io/owner": owner}},
        "spec": {
            "owner": owner, "name": display, "region": "centralindia", "image": "nginx:alpine",
            "storage": {"quotaGb": 20}, "desiredState": "running"
        },
        "status": {"phase": "ready", "nodeName": NODE, "volumeRef": name}
    })
}

fn ws_list(items: Vec<Value>) -> Value {
    json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "WorkspaceList", "metadata": {}, "items": items})
}

fn env_list(items: Vec<Value>) -> Value {
    json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "EnvironmentList", "metadata": {}, "items": items})
}

fn snap_list(items: Vec<Value>) -> Value {
    json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": items})
}

/// A push: `transient` unset is what makes it a snapshot rather than a sync point.
fn push(id: &str, volume: &str, owner: &str, at: &str) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
        "metadata": {"name": id, "creationTimestamp": at, "labels": {"rustic-git.io/owner": owner}},
        "spec": {"volume": volume, "owner": owner, "worktree": volume, "parent": ""},
        "status": {"phase": "ready", "readyAt": at},
    })
}

fn sync_point(id: &str, volume: &str, owner: &str, at: &str) -> Value {
    let mut v = push(id, volume, owner, at);
    v["spec"]["transient"] = json!(true);
    v
}

fn with_state(mut snap: Value, state: Value) -> Value {
    snap["spec"]["state"] = state;
    snap
}

fn ok(method: &'static str, path: String) -> Route {
    Route { method, path, status: 200, body: json!({"kind": "Status", "apiVersion": "v1", "status": "Success"}) }
}

async fn server(routes: Vec<Route>) -> Server {
    let store = Arc::new(MemStore::new());
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, rec) = mock_client(routes);
    let state =
        ApiState::new(store as Arc<dyn MetaStore>, jwt.clone(), HashSet::new()).with_kube(client);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router(Arc::new(state))).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt, rec }
}

fn token(jwt: &Jwt, username: &str) -> String {
    jwt.mint(&format!("{username}@example.com"), "Test User", Some(username)).unwrap()
}

async fn get_json(s: &Server, tok: &str, path: &str) -> (reqwest::StatusCode, Value) {
    let resp = reqwest::Client::new().get(format!("{}{path}", s.base)).bearer_auth(tok).send().await.unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn delete(s: &Server, tok: &str, path: &str) -> reqwest::StatusCode {
    reqwest::Client::new().delete(format!("{}{path}", s.base)).bearer_auth(tok).send().await.unwrap().status()
}

/// THE bug this listing exists for: a volume whose workspace was deleted is still listed, still
/// counted, and still says what it was — the snapshots outlive the parent.
#[tokio::test]
async fn a_volume_whose_parent_was_deleted_is_still_listed() {
    let s = server(vec![
        kget(
            SNAPS,
            snap_list(vec![
                push("ws-live-a", "ws-live", "karthik", "2026-08-27T09:00:00Z"),
                push("ws-gone-a", "ws-gone", "karthik", "2026-08-27T10:00:00Z"),
                push("ws-gone-b", "ws-gone", "karthik", "2026-08-27T11:00:00Z"),
            ]),
        ),
        kget(format!("{API}/workspaces"), ws_list(vec![ws_obj("ws-live", "karthik", "web")])),
        kget(format!("{API}/environments"), env_list(vec![])),
    ])
    .await;
    let tok = token(&s.jwt, "karthik");

    let (status, body) = get_json(&s, &tok, "/v1/volumes").await;
    assert_eq!(status, 200, "{body}");
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), 2, "the deleted parent's volume is still a row: {body}");

    let live = rows.iter().find(|v| v["name"] == "ws-live").unwrap();
    assert_eq!(live["deleted"], false);
    assert_eq!(live["kind"], "workspace");
    assert_eq!(live["display_name"], "web", "a live parent names itself");
    assert_eq!(live["volume"], "vol/karthik/ws-live", "the field the web already reads");

    let gone = rows.iter().find(|v| v["name"] == "ws-gone").unwrap();
    assert_eq!(gone["deleted"], true, "no live workspace of that name: {gone}");
    assert_eq!(gone["kind"], "workspace");
    assert_eq!(gone["display_name"], "ws-gone", "nothing alive to name it: the volume id");
}

/// The two fields the Snapshots page counts on: how many pushes a volume holds, and when the last
/// one landed. Sync points are neither — they are a live worktree's replication state — and a
/// volume that holds ONLY sync points is not a row at all.
#[tokio::test]
async fn volume_rows_carry_the_snapshot_count_and_last_push() {
    let s = server(vec![
        kget(
            SNAPS,
            snap_list(vec![
                push("ws-1-a", "ws-1", "karthik", "2026-08-27T09:00:00Z"),
                push("ws-1-b", "ws-1", "karthik", "2026-08-27T10:00:00Z"),
                sync_point("sync-ws-1", "ws-1", "karthik", "2026-08-27T12:00:00Z"),
                sync_point("sync-ws-2", "ws-2", "karthik", "2026-08-27T12:00:00Z"),
            ]),
        ),
        kget(format!("{API}/workspaces"), ws_list(vec![ws_obj("ws-1", "karthik", "web")])),
        kget(format!("{API}/environments"), env_list(vec![])),
    ])
    .await;
    let tok = token(&s.jwt, "karthik");

    let (status, body) = get_json(&s, &tok, "/v1/volumes").await;
    assert_eq!(status, 200, "{body}");
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), 1, "a volume with only sync points is not a row: {body}");
    assert_eq!(rows[0]["snapshots"], 2, "the pushes, not the sync points: {body}");
    assert_eq!(rows[0]["last_push_at"], "2026-08-27T10:00:00Z");
    assert_eq!(rows[0]["latest_ms"], 1_787_832_000_000i64, "the newest cut of any kind");
}

/// A deleted parent still says what it WAS: the frozen `spec.state` is tagged by kind. With no
/// state either (a record cut before it existed) the ID PREFIX is authoritative — `rid("ws")` /
/// `rid("env")` mint every id there is, and defaulting the class to "workspace" filed every
/// deleted environment's snapshots under the wrong heading.
#[tokio::test]
async fn a_deleted_parents_kind_comes_from_the_frozen_state_then_the_id_prefix() {
    let s = server(vec![
        kget(
            SNAPS,
            snap_list(vec![
                with_state(
                    push("ws-old-a", "ws-old", "karthik", "2026-08-27T09:00:00Z"),
                    json!({"kind": "environment", "services": [], "quotaGb": 20}),
                ),
                push("env-old-a", "env-old", "karthik", "2026-08-27T09:00:00Z"),
            ]),
        ),
        kget(format!("{API}/workspaces"), ws_list(vec![])),
        kget(format!("{API}/environments"), env_list(vec![])),
    ])
    .await;
    let tok = token(&s.jwt, "karthik");

    let (status, body) = get_json(&s, &tok, "/v1/volumes").await;
    assert_eq!(status, 200, "{body}");
    let rows = body.as_array().unwrap();
    let by = |n: &str| rows.iter().find(|r| r["name"] == n).unwrap_or_else(|| panic!("{n} missing: {body}")).clone();

    assert_eq!(by("ws-old")["kind"], "environment", "the frozen state, not the id: {body}");
    assert_eq!(by("env-old")["kind"], "environment", "the id prefix, with no state to read");
    assert_eq!(by("env-old")["display_name"], "env-old");
}

/// The Snapshots tab asks for one kind: an environment's snapshots are the shared artifact, a
/// workspace's are that one person's undo history.
#[tokio::test]
async fn the_listing_filters_by_kind() {
    let s = server(vec![
        kget(
            SNAPS,
            snap_list(vec![
                push("ws-1-a", "ws-1", "karthik", "2026-08-27T09:00:00Z"),
                push("env-1-a", "env-1", "karthik", "2026-08-27T09:00:00Z"),
            ]),
        ),
        kget(format!("{API}/workspaces"), ws_list(vec![ws_obj("ws-1", "karthik", "web")])),
        kget(format!("{API}/environments"), env_list(vec![])),
    ])
    .await;
    let tok = token(&s.jwt, "karthik");

    let (status, body) = get_json(&s, &tok, "/v1/volumes?kind=environment").await;
    assert_eq!(status, 200, "{body}");
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), 1, "only the environments: {body}");
    assert_eq!(rows[0]["name"], "env-1");
}

/// A snapshot a running worktree is standing on is its BASE — deleting it would take the bytes
/// out from under a live pod. 409, not 404: the caller may read it, they just may not do this.
#[tokio::test]
async fn deleting_a_snapshot_that_is_a_running_worktrees_head_is_a_409() {
    let mut running = ws_obj("ws-1", "karthik", "web");
    running["status"]["head"] = json!("ws-1-a");
    let s = server(vec![
        kget(SNAPS, snap_list(vec![push("ws-1-a", "ws-1", "karthik", "2026-08-27T09:00:00Z")])),
        kget(format!("{API}/workspaces"), ws_list(vec![running])),
        kget(format!("{API}/environments"), env_list(vec![])),
    ])
    .await;
    let tok = token(&s.jwt, "karthik");

    assert_eq!(delete(&s, &tok, "/v1/volumes/ws-1/snapshots/ws-1-a").await, 409);
    assert!(s.rec.calls().iter().all(|c| !c.starts_with("DELETE")), "nothing was deleted: {:?}", s.rec.calls());
}

/// A sync point belongs to the agent's sync beat: deleting one by hand takes a replica's btrfs
/// send parent away. 409 with its own message, so the person is told what it is.
#[tokio::test]
async fn a_sync_point_cannot_be_deleted_by_hand() {
    let s = server(vec![
        kget(
            SNAPS,
            snap_list(vec![
                push("ws-1-a", "ws-1", "karthik", "2026-08-27T09:00:00Z"),
                sync_point("sync-ws-1", "ws-1", "karthik", "2026-08-27T12:00:00Z"),
            ]),
        ),
        kget(format!("{API}/workspaces"), ws_list(vec![ws_obj("ws-1", "karthik", "web")])),
        kget(format!("{API}/environments"), env_list(vec![])),
    ])
    .await;
    let tok = token(&s.jwt, "karthik");

    assert_eq!(delete(&s, &tok, "/v1/volumes/ws-1/snapshots/sync-ws-1").await, 409);
    assert!(s.rec.calls().iter().all(|c| !c.starts_with("DELETE")), "nothing was deleted: {:?}", s.rec.calls());
}

/// The ordinary case: the record goes and the Volume stays, because another snapshot is still on
/// it. Owner-scoped exactly like the history read — someone else's volume is a 404, never a 403.
#[tokio::test]
async fn deleting_a_snapshot_removes_its_record_and_keeps_the_volume() {
    let s = server(vec![
        kget(
            SNAPS,
            snap_list(vec![
                push("ws-1-a", "ws-1", "karthik", "2026-08-27T09:00:00Z"),
                push("ws-1-b", "ws-1", "karthik", "2026-08-27T10:00:00Z"),
            ]),
        ),
        kget(format!("{API}/workspaces"), ws_list(vec![])),
        kget(format!("{API}/environments"), env_list(vec![])),
        ok("DELETE", format!("{SNAPS}/ws-1-a")),
    ])
    .await;

    assert_eq!(delete(&s, &token(&s.jwt, "bob"), "/v1/volumes/ws-1/snapshots/ws-1-a").await, 404, "not bob's");
    assert_eq!(delete(&s, &token(&s.jwt, "karthik"), "/v1/volumes/ws-1/snapshots/nope").await, 404, "unknown id");
    assert_eq!(delete(&s, &token(&s.jwt, "karthik"), "/v1/volumes/ws-1/snapshots/ws-1-a").await, 204);

    let deletes: Vec<String> = s.rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
    assert_eq!(deletes, vec![format!("DELETE {SNAPS}/ws-1-a")], "the record only: {deletes:?}");
}

/// A shared clone or a restore-to-new puts a worktree owned by ANOTHER person on the same volume,
/// so the running-parent checks are cluster-wide, not owner-scoped: an owner-scoped listing could
/// not see it, and both deletes would have taken bytes out from under their running pod.
#[tokio::test]
async fn a_foreign_worktree_on_the_volume_refuses_both_deletes() {
    let mut foreign = ws_obj("ws-clone", "alice", "clone");
    foreign["status"]["volumeRef"] = json!("ws-1");
    foreign["status"]["head"] = json!("ws-1-a");
    let s = server(vec![
        kget(SNAPS, snap_list(vec![push("ws-1-a", "ws-1", "karthik", "2026-08-27T09:00:00Z")])),
        kget(format!("{API}/workspaces"), ws_list(vec![foreign])),
        kget(format!("{API}/environments"), env_list(vec![])),
    ])
    .await;
    let tok = token(&s.jwt, "karthik");

    assert_eq!(delete(&s, &tok, "/v1/volumes/ws-1/snapshots/ws-1-a").await, 409, "another owner's running base");
    assert_eq!(delete(&s, &tok, "/v1/volumes/ws-1").await, 409, "another owner's live worktree");
    assert!(s.rec.calls().iter().all(|c| !c.starts_with("DELETE")), "nothing was deleted: {:?}", s.rec.calls());
}

/// `/history` and `/refs` show SNAPSHOTS only. A sync point is the agent's replication state — the
/// next beat deletes it — and a migration baseline is nobody's push; offering either as history
/// offers a restore onto a record that can vanish.
#[tokio::test]
async fn history_and_refs_skip_sync_points_and_baselines() {
    let mut baseline = push("ws-1-base", "ws-1", "karthik", "2026-08-27T08:00:00Z");
    baseline["spec"]["message"] = json!("migration baseline");
    let s = server(vec![
        kget(
            SNAPS,
            snap_list(vec![
                sync_point("sync-ws-1", "ws-1", "karthik", "2026-08-27T12:00:00Z"),
                push("ws-1-a", "ws-1", "karthik", "2026-08-27T09:00:00Z"),
                baseline,
            ]),
        ),
        kget(format!("{API}/workspaces"), ws_list(vec![])),
        kget(format!("{API}/environments"), env_list(vec![])),
    ])
    .await;
    let tok = token(&s.jwt, "karthik");

    let (status, body) = get_json(&s, &tok, "/v1/volumes/ws-1/history").await;
    assert_eq!(status, 200, "{body}");
    let ids: Vec<&str> = body.as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["ws-1-a"], "the push only: {body}");

    // The sync point is the NEWEST record, so a tip that did not filter would name it.
    let (status, body) = get_json(&s, &tok, "/v1/volumes/ws-1/refs").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["main"], "ws-1-a", "refs never name a sync point: {body}");
}

/// The last snapshot of a volume nothing owns any more takes the volume with it — that is what
/// detaching it (`cleanup_parent`) kept it alive FOR, and leaving it would leak a subvolume
/// nothing can ever reach again.
#[tokio::test]
async fn deleting_the_last_snapshot_of_a_detached_volume_deletes_the_volume() {
    let s = server(vec![
        kget(
            SNAPS,
            snap_list(vec![
                push("ws-1-a", "ws-1", "karthik", "2026-08-27T09:00:00Z"),
                sync_point("sync-ws-1", "ws-1", "karthik", "2026-08-27T12:00:00Z"),
            ]),
        ),
        kget(format!("{API}/workspaces"), ws_list(vec![])),
        kget(format!("{API}/environments"), env_list(vec![])),
        ok("DELETE", format!("{SNAPS}/ws-1-a")),
        ok("DELETE", format!("{API}/volumes/ws-1")),
    ])
    .await;

    assert_eq!(delete(&s, &token(&s.jwt, "karthik"), "/v1/volumes/ws-1/snapshots/ws-1-a").await, 204);

    let deletes: Vec<String> = s.rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
    assert_eq!(
        deletes,
        vec![format!("DELETE {SNAPS}/ws-1-a"), format!("DELETE {API}/volumes/ws-1")],
        "a leftover sync point is not a reason to keep the volume: {deletes:?}"
    );
}

/// The same delete on a volume that still has a live parent keeps the volume: those bytes are
/// somebody's working copy.
#[tokio::test]
async fn deleting_the_last_snapshot_of_an_attached_volume_keeps_the_volume() {
    let s = server(vec![
        kget(SNAPS, snap_list(vec![push("ws-1-a", "ws-1", "karthik", "2026-08-27T09:00:00Z")])),
        kget(format!("{API}/workspaces"), ws_list(vec![ws_obj("ws-1", "karthik", "web")])),
        kget(format!("{API}/environments"), env_list(vec![])),
        ok("DELETE", format!("{SNAPS}/ws-1-a")),
    ])
    .await;

    assert_eq!(delete(&s, &token(&s.jwt, "karthik"), "/v1/volumes/ws-1/snapshots/ws-1-a").await, 204);

    let deletes: Vec<String> = s.rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
    assert_eq!(deletes, vec![format!("DELETE {SNAPS}/ws-1-a")], "the volume is still in use: {deletes:?}");
}

/// `DELETE /v1/volumes/{name}` on a volume that still belongs to a workspace or environment is a
/// 409 — deleting the Volume takes every snapshot on it AND the live worktree.
#[tokio::test]
async fn deleting_a_volume_with_a_parent_is_a_409() {
    let s = server(vec![
        kget(SNAPS, snap_list(vec![push("ws-1-a", "ws-1", "karthik", "2026-08-27T09:00:00Z")])),
        kget(format!("{API}/workspaces"), ws_list(vec![ws_obj("ws-1", "karthik", "web")])),
        kget(format!("{API}/environments"), env_list(vec![])),
    ])
    .await;

    assert_eq!(delete(&s, &token(&s.jwt, "karthik"), "/v1/volumes/ws-1").await, 409);
    assert!(s.rec.calls().iter().all(|c| !c.starts_with("DELETE")), "nothing was deleted: {:?}", s.rec.calls());
}

/// What the environment Delete dialog calls with "Also delete its snapshots" checked, and what an
/// archived row's own "Delete snapshots" calls: the Volume goes and every `Snapshot` on it is its
/// child, so they go with it.
#[tokio::test]
async fn deleting_a_detached_volume_deletes_it() {
    let s = server(vec![
        kget(
            SNAPS,
            snap_list(vec![
                push("env-1-a", "env-1", "karthik", "2026-08-27T09:00:00Z"),
                push("env-1-b", "env-1", "karthik", "2026-08-27T10:00:00Z"),
            ]),
        ),
        kget(format!("{API}/workspaces"), ws_list(vec![])),
        kget(format!("{API}/environments"), env_list(vec![])),
        ok("DELETE", format!("{API}/volumes/env-1")),
    ])
    .await;

    assert_eq!(delete(&s, &token(&s.jwt, "karthik"), "/v1/volumes/env-1").await, 204);

    let deletes: Vec<String> = s.rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
    assert_eq!(deletes, vec![format!("DELETE {API}/volumes/env-1")], "one delete takes the lot: {deletes:?}");
}

/// A volume under a label the caller may not read is a 404, never a 403 — they learn nothing about
/// volumes that are not theirs, and a volume with no records at all reads the same way.
#[tokio::test]
async fn a_foreign_volume_is_not_found_on_delete() {
    let s = server(vec![
        kget(SNAPS, snap_list(vec![push("ws-1-a", "ws-1", "alice", "2026-08-27T09:00:00Z")])),
        kget(format!("{API}/workspaces"), ws_list(vec![])),
        kget(format!("{API}/environments"), env_list(vec![])),
    ])
    .await;
    let tok = token(&s.jwt, "karthik");

    assert_eq!(delete(&s, &tok, "/v1/volumes/ws-1").await, 404, "alice's volume");
    assert_eq!(delete(&s, &tok, "/v1/volumes/ws-1/snapshots/ws-1-a").await, 404);
    assert_eq!(get_json(&s, &tok, "/v1/volumes/ws-1/history").await.0, 404);
    assert!(s.rec.calls().iter().all(|c| !c.starts_with("DELETE")), "nothing was deleted: {:?}", s.rec.calls());
}

#[tokio::test]
async fn unauthorized_without_a_token() {
    let s = server(vec![]).await;
    let resp = reqwest::Client::new().get(format!("{}/v1/volumes/ws-1/history", s.base)).send().await.unwrap();
    assert_eq!(resp.status(), 401);
}

/// A volume name or snapshot id becomes a CR name, so a `..` or an encoded slash would address
/// another object entirely. Refused with a 400 before anything is read or written.
#[tokio::test]
async fn a_traversing_volume_name_or_snapshot_id_is_refused() {
    let s = server(vec![
        kget(SNAPS, snap_list(vec![push("env-1-a", "env-1", "karthik", "2026-08-27T09:00:00Z")])),
        kget(format!("{API}/workspaces"), ws_list(vec![])),
        kget(format!("{API}/environments"), env_list(vec![])),
        ok("DELETE", format!("{SNAPS}/env-1-a")),
        ok("DELETE", format!("{API}/volumes/env-1")),
    ])
    .await;
    let tok = token(&s.jwt, "karthik");
    let send = |method: reqwest::Method, path: String| {
        let url = format!("{}{path}", s.base);
        let tok = tok.clone();
        async move { reqwest::Client::new().request(method, url).bearer_auth(tok).send().await.unwrap().status() }
    };
    let (del, get) = (reqwest::Method::DELETE, reqwest::Method::GET);
    assert_eq!(send(del.clone(), "/v1/volumes/x/snapshots/..%2F..%2Fy".into()).await, 400);
    assert_eq!(send(del.clone(), "/v1/volumes/..%2F..%2Fy".into()).await, 400);
    assert_eq!(send(get, "/v1/volumes/a%2Fb/history".into()).await, 400);
    assert_eq!(send(del, "/v1/volumes/env-1/snapshots/env-1-a".into()).await, 204, "a plain id still works");
}

/// A legacy migration baseline is a sync point by SHAPE, not by `spec.transient` — it predates the
/// flag. Deleting one by hand takes a replica's btrfs send parent away just as the flagged kind
/// does, so it is the same 409.
#[tokio::test]
async fn a_legacy_migration_baseline_cannot_be_deleted_by_hand() {
    let mut baseline = push("ws-1", "ws-1", "karthik", "2026-08-01T09:00:00Z");
    baseline["spec"]["message"] = json!("migration baseline");
    let s = server(vec![
        kget(
            SNAPS,
            snap_list(vec![baseline, push("ws-1-a", "ws-1", "karthik", "2026-08-27T09:00:00Z")]),
        ),
        kget(format!("{API}/workspaces"), ws_list(vec![ws_obj("ws-1", "karthik", "web")])),
        kget(format!("{API}/environments"), env_list(vec![])),
    ])
    .await;

    assert_eq!(delete(&s, &token(&s.jwt, "karthik"), "/v1/volumes/ws-1/snapshots/ws-1").await, 409);
    assert!(s.rec.calls().iter().all(|c| !c.starts_with("DELETE")), "nothing was deleted: {:?}", s.rec.calls());
}

/// The last-snapshot volume delete decides on a SECOND read: a restore can attach a working copy
/// in the window between the first read and the record delete, and the volume it just started
/// using must not be collected under it.
#[tokio::test]
async fn a_working_copy_appearing_mid_delete_keeps_the_volume() {
    let s = server(vec![
        kget(SNAPS, snap_list(vec![push("ws-1-a", "ws-1", "karthik", "2026-08-27T09:00:00Z")])),
        kget(format!("{API}/workspaces"), ws_list(vec![])),
        // The restore lands here — the second read sees it, the first did not.
        kget(format!("{API}/workspaces"), ws_list(vec![ws_obj("ws-1", "karthik", "web")])),
        kget(format!("{API}/environments"), env_list(vec![])),
        ok("DELETE", format!("{SNAPS}/ws-1-a")),
    ])
    .await;

    assert_eq!(delete(&s, &token(&s.jwt, "karthik"), "/v1/volumes/ws-1/snapshots/ws-1-a").await, 204);

    let deletes: Vec<String> = s.rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
    assert_eq!(deletes, vec![format!("DELETE {SNAPS}/ws-1-a")], "the volume is in use again: {deletes:?}");
}

/// A volume genuinely holds snapshots from more than one owner: a restore grafts the caller's new
/// workspace onto a team's volume, and `create_snapshot` stamps the PUSHING worktree's owner. The
/// owner-filtered list is what the caller may SEE; it must never be what decides whether the
/// volume is empty, or one team member's delete takes the team's whole history.
#[tokio::test]
async fn a_foreign_snapshot_on_the_volume_refuses_the_volume_delete() {
    let s = server(vec![
        kget(
            SNAPS,
            snap_list(vec![
                push("ws-1-a", "ws-1", "karthik", "2026-08-27T09:00:00Z"),
                push("ws-1-b", "ws-1", "alice", "2026-08-27T10:00:00Z"),
            ]),
        ),
        kget(format!("{API}/workspaces"), ws_list(vec![])),
        kget(format!("{API}/environments"), env_list(vec![])),
    ])
    .await;

    assert_eq!(delete(&s, &token(&s.jwt, "karthik"), "/v1/volumes/ws-1").await, 409);
    assert!(s.rec.calls().iter().all(|c| !c.starts_with("DELETE")), "nothing was deleted: {:?}", s.rec.calls());
}

/// The same split, from the other end: deleting my own last snapshot must not collect a volume
/// that still holds somebody else's.
#[tokio::test]
async fn a_foreign_snapshot_keeps_the_volume_after_my_last_snapshot_goes() {
    let s = server(vec![
        kget(
            SNAPS,
            snap_list(vec![
                push("ws-1-a", "ws-1", "karthik", "2026-08-27T09:00:00Z"),
                push("ws-1-b", "ws-1", "alice", "2026-08-27T10:00:00Z"),
            ]),
        ),
        kget(format!("{API}/workspaces"), ws_list(vec![])),
        kget(format!("{API}/environments"), env_list(vec![])),
        ok("DELETE", format!("{SNAPS}/ws-1-a")),
    ])
    .await;

    assert_eq!(delete(&s, &token(&s.jwt, "karthik"), "/v1/volumes/ws-1/snapshots/ws-1-a").await, 204);
    let deletes: Vec<String> = s.rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
    assert_eq!(deletes, vec![format!("DELETE {SNAPS}/ws-1-a")], "the volume is alice's too: {deletes:?}");
}
