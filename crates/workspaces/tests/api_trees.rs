//! `POST /v1/workspaces/{id}/trees` asks for a writable nested snapshot of the workspace's own
//! working directory, and `DELETE …/trees/{name}` gives it back. Spec only — the node agent cuts
//! and deletes the subvolume — so both answer 202, the same shape as an intercept.
//!
//! The rule the ceiling test pins: a tree is bytes on a volume the owner already pays for, so
//! quota is never charged; what bounds it is `trees_per_workspace`, and the refusal names the
//! number rather than making the caller guess.

use kloudlite_core::jwt::Jwt;
use kloudlite_workspaces::api::{router, ApiState};
use kloudlite_workspaces::kube_test::{get, mock_client, Recorder, Route};
use serde_json::{json, Value};
use std::sync::Arc;

const API: &str = "/apis/kloudlite.io/v1alpha1";

struct Server {
    base: String,
    jwt: Arc<Jwt>,
    rec: Recorder,
}

fn ws_obj(trees: Value, state: &str) -> Value {
    json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": "ws-1", "resourceVersion": "42", "labels": {"kloudlite.io/owner": "karthik"}},
        "spec": {
            "owner": "karthik", "team": "", "name": "w", "region": "centralindia", "image": "nginx:alpine",
            "storage": {"quotaGb": 20}, "desiredState": state,
            "trees": trees,
        },
        "status": {"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"},
    })
}

fn routes(trees: Value, state: &str) -> Vec<Route> {
    vec![
        get(format!("{API}/workspaces/ws-1"), ws_obj(trees.clone(), state)),
        get(
            format!("{API}/snapshots"),
            json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": []}),
        ),
        // No `ClusterSettings/default` route: `get_opt` on a path the mock does not serve answers
        // 404, which is exactly "no admin ever set a ceiling" — the compiled-in 8 then applies,
        // and that is the number the ceiling test asserts on.
        Route { method: "PATCH", path: format!("{API}/workspaces/ws-1"), status: 200, body: ws_obj(trees, state) },
    ]
}

async fn server(routes: Vec<Route>) -> Server {
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, rec) = mock_client(routes);
    let state = ApiState::new(jwt.clone()).with_kube(client);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt, rec }
}

fn token(jwt: &Jwt) -> String {
    jwt.mint("karthik@example.com", "Test User", Some("karthik")).unwrap()
}

async fn cut(s: &Server, name: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/trees", s.base))
        .bearer_auth(token(&s.jwt))
        .json(&json!({"name": name}))
        .send()
        .await
        .unwrap()
}

async fn drop_tree(s: &Server, name: &str) -> reqwest::Response {
    reqwest::Client::new()
        .delete(format!("{}/v1/workspaces/ws-1/trees/{name}", s.base))
        .bearer_auth(token(&s.jwt))
        .send()
        .await
        .unwrap()
}

/// The `trees` value a CAS patch carried, and the assertion that the guard is on it at all —
/// what makes two agents asking for a tree at once lose one rather than clobber the other.
fn written(s: &Server) -> Vec<Value> {
    s.rec
        .sent("PATCH", &format!("{API}/workspaces/ws-1"))
        .iter()
        .map(|p| {
            let ops = p.as_array().expect("a JSON Patch is an array of ops");
            assert_eq!(
                ops[0],
                json!({"op": "test", "path": "/metadata/resourceVersion", "value": "42"}),
                "every write is guarded: {p}"
            );
            assert_eq!(ops[1]["op"], "add");
            assert_eq!(ops[1]["path"], "/spec/trees");
            ops[1]["value"].clone()
        })
        .collect()
}

#[tokio::test]
async fn a_tree_is_cut_from_a_running_workspace() {
    let s = server(routes(json!([]), "running")).await;
    let r = cut(&s, "fix-auth").await;
    assert_eq!(r.status(), 202);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["name"], "fix-auth");
    // Under the home, which IS the worktree mount now (one home per pod), never a path named by
    // the CR id: the id named a path that does not exist (R-D21).
    assert_eq!(body["path"], "/home/kl/.agents/fix-auth/workspace");
    let w = written(&s);
    assert_eq!(w.len(), 1, "one write");
    assert_eq!(w[0].as_array().unwrap().len(), 1);
    assert_eq!(w[0][0]["name"], "fix-auth");
    assert!(w[0][0]["created"].is_string(), "the cut is dated: {}", w[0]);
}

#[tokio::test]
async fn the_same_name_twice_is_409() {
    let held = json!([{"name": "fix-auth", "created": "2026-09-17T00:00:00Z"}]);
    let s = server(routes(held, "running")).await;
    let r = cut(&s, "fix-auth").await;
    assert_eq!(r.status(), 409);
    assert!(r.text().await.unwrap().contains("tree fix-auth exists"));
    assert!(written(&s).is_empty(), "nothing written");
}

#[tokio::test]
async fn the_ninth_tree_is_409_naming_the_ceiling() {
    let held: Value = (0..8).map(|i| json!({"name": format!("t{i}"), "created": "2026-09-17T00:00:00Z"})).collect();
    let s = server(routes(held, "running")).await;
    let r = cut(&s, "t8").await;
    assert_eq!(r.status(), 409);
    let body = r.text().await.unwrap();
    assert!(body.contains("8 of 8"), "the ceiling is named: {body}");
    assert!(written(&s).is_empty(), "nothing written");
}

/// A tree is a snapshot of a LIVE working directory. On a stopped workspace there is no node
/// holding it and nothing to snapshot, so this is a refusal rather than a wish left pending.
#[tokio::test]
async fn a_stopped_workspace_is_409() {
    let s = server(routes(json!([]), "stopped")).await;
    let r = cut(&s, "fix-auth").await;
    assert_eq!(r.status(), 409);
    assert!(r.text().await.unwrap().contains("not running"));
    assert!(written(&s).is_empty(), "nothing written");
}

#[tokio::test]
async fn a_name_outside_the_charset_is_422() {
    let s = server(routes(json!([]), "running")).await;
    for bad in ["Fix-Auth", "fix auth", "fix/auth", "", "../x", &"a".repeat(33)] {
        let r = cut(&s, bad).await;
        assert_eq!(r.status(), 422, "{bad:?} is refused");
    }
    assert!(written(&s).is_empty(), "nothing written");
}

#[tokio::test]
async fn delete_removes_the_spec_entry() {
    let held = json!([
        {"name": "a", "created": "2026-09-17T00:00:00Z"},
        {"name": "b", "created": "2026-09-17T00:00:00Z"},
    ]);
    let s = server(routes(held, "running")).await;
    assert_eq!(drop_tree(&s, "a").await.status(), 202);
    let w = written(&s);
    assert_eq!(w.len(), 1);
    assert_eq!(w[0], json!([{"name": "b", "created": "2026-09-17T00:00:00Z"}]));
}

/// A tree the spec does not name is already the state being asked for — but a 404 says so, because
/// the harness waits on a name it believes it created and a silent 202 would hide a typo forever.
#[tokio::test]
async fn deleting_an_unknown_tree_is_404() {
    let s = server(routes(json!([]), "running")).await;
    assert_eq!(drop_tree(&s, "nope").await.status(), 404);
    assert!(written(&s).is_empty(), "nothing written");
}

/// A tree is cut on a workspace the caller may act on, and on no other. `my_ws` is the gate, the
/// same one every other workspace verb resolves through.
#[tokio::test]
async fn someone_elses_workspace_is_404() {
    let mut ws = ws_obj(json!([]), "running");
    ws["spec"]["owner"] = json!("other");
    ws["metadata"]["labels"]["kloudlite.io/owner"] = json!("other");
    let s = server(vec![
        get(format!("{API}/workspaces/ws-1"), ws),
        Route { method: "PATCH", path: format!("{API}/workspaces/ws-1"), status: 200, body: json!({}) },
    ])
    .await;
    assert_eq!(cut(&s, "fix-auth").await.status(), 404);
    assert!(written(&s).is_empty(), "nothing written");
}

/// `GET /v1/workspaces/{id}` lists what the NODE cut, not what was asked for (spec §4.2). The two
/// differ for as long as a cut takes, and the harness waits on exactly this difference: it polls
/// until the tree it asked for reads `ready`.
///
/// `reason` rides along because a cut can fail — a full pool, a lock held — and the agent retries
/// it on the next pass. Without it a person watching a tree that never turns ready has nothing to
/// read but the absence of a row.
#[tokio::test]
async fn the_workspace_doc_lists_the_trees_the_node_cut() {
    let mut ws = ws_obj(json!([{"name": "fix-auth", "created": "2026-09-17T00:00:00Z"}]), "running");
    ws["status"]["trees"] = json!([
        {"name": "fix-auth", "path": "/home/kl/workspaces/ws-1/.agents/fix-auth", "ready": true},
        {"name": "sad", "path": "/home/kl/workspaces/ws-1/.agents/sad", "ready": false, "reason": "no space left on device"},
    ]);
    let s = server(vec![
        get(format!("{API}/workspaces/ws-1"), ws),
        get(
            format!("{API}/snapshots"),
            json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": []}),
        ),
    ])
    .await;
    let doc: Value = reqwest::Client::new()
        .get(format!("{}/v1/workspaces/ws-1", s.base))
        .bearer_auth(token(&s.jwt))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        doc["trees"],
        json!([
            {"name": "fix-auth", "path": "/home/kl/workspaces/ws-1/.agents/fix-auth", "ready": true},
            {"name": "sad", "path": "/home/kl/workspaces/ws-1/.agents/sad", "ready": false, "reason": "no space left on device"},
        ]),
        "{doc}"
    );
}

/// A workspace nobody has asked a tree of carries no `trees` key at all, rather than an empty
/// array: every other optional list on this doc is omitted the same way, and a reader that sees
/// the key can trust it means something.
#[tokio::test]
async fn a_workspace_with_no_trees_omits_the_field() {
    let s = server(routes(json!([]), "running")).await;
    let doc: Value = reqwest::Client::new()
        .get(format!("{}/v1/workspaces/ws-1", s.base))
        .bearer_auth(token(&s.jwt))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(doc.get("trees").is_none(), "{doc}");
}
