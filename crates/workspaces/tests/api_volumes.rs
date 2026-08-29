//! `/v1/volumes` browse routes, against a mocked cluster and a mocked server tier.
//!
//! The snapshots themselves come from the SERVER tier now: the index and the records both live in
//! the `vol/{owner}/{name}` registry, and this tier only asks the cluster whether a snapshot's
//! parent workspace still exists. No `SnapshotRequest` appears in this file at all — the request is
//! the push work item, and a listing that depended on one would go blind the moment it was
//! collected.

use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{router, ApiState};
use rustic_git_workspaces::kube_test::{get as kget, mock_client, stub_registry as upstream, Recorder, Route};
use rustic_git_workspaces::store::{MemStore, MetaStore};
use rustic_git_workspaces::upstream::Upstream;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

const API: &str = "/apis/rustic-git.io/v1alpha1";
const NODE: &str = "node-a";

struct Server {
    base: String,
    jwt: Arc<Jwt>,
    #[allow(dead_code)]
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

/// A commit record as the server tier answers it. `state` is where a push writes its provenance.
fn record(id: &str, at: &str, message: Option<&str>, state: Value) -> Value {
    let mut v = json!({
        "id": id, "state": state, "lineage": [], "region": "centralindia", "created_at": at
    });
    if let Some(m) = message {
        v["message"] = json!(m);
    }
    v
}

async fn server(routes: Vec<Route>, upstream_base: String) -> Server {
    let store = Arc::new(MemStore::new());
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, rec) = mock_client(routes);
    let state = ApiState::new(store as Arc<dyn MetaStore>, jwt.clone(), HashSet::new())
        .with_kube(client)
        .with_upstream(Arc::new(Upstream::new(upstream_base, "peer-secret")));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router(Arc::new(state))).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt, rec }
}

fn token(jwt: &Jwt, username: &str) -> String {
    jwt.mint(&format!("{username}@example.com"), "Test User", Some(username)).unwrap()
}

async fn get_json(s: &Server, tok: &str, path: &str) -> (reqwest::StatusCode, Value) {
    let resp = reqwest::Client::new()
        .get(format!("{}{path}", s.base))
        .bearer_auth(tok)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// THE bug this exists for: a volume whose workspace was deleted is still listed, still counted,
/// and still says what it used to be — because the records outlive the workspace and the listing
/// reads the records.
#[tokio::test]
async fn a_volume_whose_parent_was_deleted_is_still_listed() {
    let up = upstream(
        vec![("karthik", json!([{"name": "ws-live", "latest_ms": 1_700_000_000_000i64},
                                {"name": "ws-gone", "latest_ms": 1_700_000_001_000i64}]))],
        vec![(
            "karthik/ws-gone",
            json!([
                record("c2", "2026-08-27T10:00:00Z", Some("second"), json!({"kind": "workspace", "name": "api-scratch"})),
                record("c1", "2026-08-27T09:00:00Z", Some("first"), json!({"kind": "workspace", "name": "api-scratch"})),
            ]),
        )],
    )
    .await;
    // Only `ws-live` still exists in the cluster.
    let s = server(
        vec![
            kget(format!("{API}/workspaces"), ws_list(vec![ws_obj("ws-live", "karthik", "web")])),
            kget(format!("{API}/environments"), env_list(vec![])),
        ],
        up,
    )
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
    assert_eq!(gone["display_name"], "api-scratch", "from the newest record's provenance");
    assert_eq!(gone["latest_ms"], 1_700_000_001_000i64);
}

/// A volume with no provenance anywhere — pushed before it was written, or backfilled — falls back
/// to the volume id rather than showing a blank name, and to the ID PREFIX for its kind. The
/// prefix is authoritative (`rid("ws")` / `rid("env")` mint every id), so an unnamed `env-` volume
/// is an environment; defaulting the whole class to "workspace" filed every deleted environment's
/// snapshots under the wrong heading.
#[tokio::test]
async fn a_record_without_provenance_falls_back_to_the_volume_id() {
    let up = upstream(
        vec![(
            "karthik",
            json!([
                {"name": "ws-old", "latest_ms": 1_700_000_000_000i64},
                {"name": "env-old", "latest_ms": 1_700_000_000_000i64}
            ]),
        )],
        vec![
            ("karthik/ws-old", json!([record("c1", "2026-08-27T09:00:00Z", None, Value::Null)])),
            ("karthik/env-old", json!([record("c2", "2026-08-27T09:00:00Z", None, Value::Null)])),
        ],
    )
    .await;
    let s = server(
        vec![
            kget(format!("{API}/workspaces"), ws_list(vec![])),
            kget(format!("{API}/environments"), env_list(vec![])),
        ],
        up,
    )
    .await;
    let tok = token(&s.jwt, "karthik");

    let (status, body) = get_json(&s, &tok, "/v1/volumes").await;
    assert_eq!(status, 200);
    let rows = body.as_array().unwrap();
    let by = |n: &str| rows.iter().find(|r| r["name"] == n).unwrap_or_else(|| panic!("{n} missing: {body}")).clone();

    let ws = by("ws-old");
    assert_eq!(ws["display_name"], "ws-old");
    assert_eq!(ws["kind"], "workspace");
    assert_eq!(ws["deleted"], true);

    let env = by("env-old");
    assert_eq!(env["display_name"], "env-old");
    assert_eq!(env["kind"], "environment", "the id prefix is what says so");
    assert_eq!(env["deleted"], true);
}

/// History is the server tier's answer verbatim, and needs no live parent to be readable.
#[tokio::test]
async fn history_reads_without_a_live_workspace() {
    let up = upstream(
        vec![("karthik", json!([{"name": "ws-gone", "latest_ms": 1i64}]))],
        vec![(
            "karthik/ws-gone",
            json!([
                record("c2", "2026-08-27T10:00:00Z", None, Value::Null),
                record("c1", "2026-08-27T09:00:00Z", Some("first"), Value::Null),
            ]),
        )],
    )
    .await;
    let s = server(vec![], up).await;
    let tok = token(&s.jwt, "karthik");

    let (status, body) = get_json(&s, &tok, "/v1/volumes/ws-gone/history").await;
    assert_eq!(status, 200, "{body}");
    let records = body.as_array().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["id"], "c2", "newest first");
    assert_eq!(records[1]["message"], "first");

    // And "main" is the newest, the same convention `engine::ops` relies on.
    let (status, body) = get_json(&s, &tok, "/v1/volumes/ws-gone/refs").await;
    assert_eq!(status, 200);
    assert_eq!(body["main"], "c2");
}

/// The server tier refuses a volume that is not the named owner's, and this tier only ever asks as
/// owners it has verified the caller for — so someone else's volume is a 404 either way.
#[tokio::test]
async fn another_owners_volume_is_not_found() {
    let up = upstream(
        vec![("alice", json!([{"name": "ws-1", "latest_ms": 1i64}]))],
        vec![("alice/ws-1", json!([record("c1", "2026-08-27T09:00:00Z", None, Value::Null)]))],
    )
    .await;
    let s = server(vec![], up).await;
    let tok = token(&s.jwt, "karthik");

    assert_eq!(get_json(&s, &tok, "/v1/volumes/ws-1/history").await.0, 404);
}

#[tokio::test]
async fn unauthorized_without_a_token() {
    let up = upstream(vec![], vec![]).await;
    let s = server(vec![], up).await;
    let resp = reqwest::Client::new().get(format!("{}/v1/volumes/ws-1/history", s.base)).send().await.unwrap();
    assert_eq!(resp.status(), 401);
}

/// Environments push volumes exactly as workspaces do, so the Snapshots page shows both — a live
/// environment named by its own spec, and a deleted one named by the provenance its last push
/// wrote. The kind is what the page picks an icon by, so getting it from the record (not from a
/// guess) is the point.
#[tokio::test]
async fn environment_and_workspace_snapshots_both_list() {
    let up = upstream(
        vec![(
            "karthik",
            json!([
                {"name": "ws-1", "latest_ms": 1_700_000_000_000i64},
                {"name": "env-live", "latest_ms": 1_700_000_002_000i64},
                {"name": "env-gone", "latest_ms": 1_700_000_003_000i64},
            ]),
        )],
        vec![(
            "karthik/env-gone",
            json!([record("c9", "2026-08-27T11:00:00Z", None, json!({"kind": "environment", "name": "staging"}))]),
        )],
    )
    .await;
    let s = server(
        vec![
            kget(format!("{API}/workspaces"), ws_list(vec![ws_obj("ws-1", "karthik", "web")])),
            kget(
                format!("{API}/environments"),
                env_list(vec![json!({
                    "apiVersion": "rustic-git.io/v1alpha1", "kind": "Environment",
                    "metadata": {"name": "env-live", "labels": {"rustic-git.io/owner": "karthik"}},
                    "spec": {"owner": "karthik", "name": "preview", "region": "centralindia",
                             "services": [], "storage": {"quotaGb": 20}, "desiredState": "running"}
                })]),
            ),
        ],
        up,
    )
    .await;
    let tok = token(&s.jwt, "karthik");

    let (status, body) = get_json(&s, &tok, "/v1/volumes").await;
    assert_eq!(status, 200, "{body}");
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), 3, "both kinds, live and deleted: {body}");

    let ws = rows.iter().find(|v| v["name"] == "ws-1").unwrap();
    assert_eq!(ws["kind"], "workspace");
    assert_eq!(ws["display_name"], "web");

    let live_env = rows.iter().find(|v| v["name"] == "env-live").unwrap();
    assert_eq!(live_env["kind"], "environment", "a live environment names its own kind: {live_env}");
    assert_eq!(live_env["display_name"], "preview");
    assert_eq!(live_env["deleted"], false);

    let gone_env = rows.iter().find(|v| v["name"] == "env-gone").unwrap();
    assert_eq!(gone_env["kind"], "environment", "from the record, the environment being gone: {gone_env}");
    assert_eq!(gone_env["display_name"], "staging");
    assert_eq!(gone_env["deleted"], true);

    // What the Snapshots tab actually asks for: environment snapshots are the shared artifact, a
    // workspace's are that one person's undo history and are reached from their own workspace row.
    let (status, body) = get_json(&s, &tok, "/v1/volumes?kind=environment").await;
    assert_eq!(status, 200, "{body}");
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), 2, "only the environments: {body}");
    assert!(rows.iter().all(|v| v["kind"] == "environment"), "{body}");
}

/// `DELETE /v1/volumes/{id}` — what the environment Delete dialog calls when "Also delete its
/// snapshots" is checked, and what an archived row's own "Delete snapshots" calls. Scoped exactly
/// as the history read is: another owner's volume is a 404, never a 403.
#[tokio::test]
async fn deleting_a_volumes_snapshots_is_owner_scoped() {
    let up = upstream(
        vec![("karthik", json!([{"name": "env-1", "latest_ms": 1i64}]))],
        vec![("karthik/env-1", json!([record("c1", "2026-08-27T09:00:00Z", None, Value::Null)]))],
    )
    .await;
    let s = server(vec![], up).await;

    let del = |tok: String, name: &str| {
        let url = format!("{}/v1/volumes/{name}", s.base);
        async move { reqwest::Client::new().delete(url).bearer_auth(tok).send().await.unwrap().status() }
    };

    // Not karthik's: `volume_owner` never finds it under any label they may read.
    assert_eq!(del(token(&s.jwt, "bob"), "env-1").await, 404);
    assert_eq!(del(token(&s.jwt, "karthik"), "no-such-vol").await, 404);
    assert_eq!(del(token(&s.jwt, "karthik"), "env-1").await, 204);
}

/// `DELETE /v1/volumes/{id}/snapshots/{snapshot}` — one record out of the lineage, scoped exactly
/// as the history read is. An id that is not in that volume's history is a 404 like a volume that
/// is not the caller's: the client learns nothing either way.
#[tokio::test]
async fn deleting_one_snapshot_is_owner_scoped() {
    let up = upstream(
        vec![("karthik", json!([{"name": "env-1", "latest_ms": 1i64}]))],
        vec![(
            "karthik/env-1",
            json!([
                record("c2", "2026-08-27T10:00:00Z", None, Value::Null),
                record("c1", "2026-08-27T09:00:00Z", None, Value::Null),
            ]),
        )],
    )
    .await;
    let s = server(vec![], up).await;

    let del = |tok: String, name: &str, id: &str| {
        let url = format!("{}/v1/volumes/{name}/snapshots/{id}", s.base);
        async move { reqwest::Client::new().delete(url).bearer_auth(tok).send().await.unwrap().status() }
    };

    assert_eq!(del(token(&s.jwt, "bob"), "env-1", "c1").await, 404, "not bob's volume");
    assert_eq!(del(token(&s.jwt, "karthik"), "no-such-vol", "c1").await, 404);
    assert_eq!(del(token(&s.jwt, "karthik"), "env-1", "nope").await, 404, "unknown snapshot id");
    assert_eq!(del(token(&s.jwt, "karthik"), "env-1", "c1").await, 204);
}

/// A volume name or snapshot id is spliced into a PEER url one tier down, so a `..` or an encoded
/// slash would re-route the request to any browse route under the caller's own owner. Refused
/// here with a 400 — and never sent: the stub answers 404 for anything it does not know, so a
/// 400 proves the request stopped before the client.
#[tokio::test]
async fn a_traversing_volume_name_or_snapshot_id_is_refused_before_the_peer() {
    let up = upstream(
        vec![("karthik", json!([{"name": "env-1", "latest_ms": 1i64}]))],
        vec![("karthik/env-1", json!([record("c1", "2026-08-27T09:00:00Z", None, Value::Null)]))],
    )
    .await;
    let s = server(vec![], up).await;
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
    assert_eq!(send(del, "/v1/volumes/env-1/snapshots/c1".into()).await, 204, "a plain id still works");
}
