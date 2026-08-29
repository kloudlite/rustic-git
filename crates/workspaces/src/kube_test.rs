//! A `kube::Client` backed by canned responses, for testing everything that talks to the API
//! server without one.
//!
//! `kube::Client::new` takes any `tower::Service<http::Request<Body>>`, which is the whole trick: a
//! `service_fn` that matches on method and path and answers JSON. Chosen over an envtest-style real
//! API server because Rust has no envtest, and downloading a `kube-apiserver` binary inside
//! `cargo test` is a non-starter. A real API server IS exercised — by `tests/ws_e2e.sh` against
//! real k3s — so this covers the branching logic and that covers the wire.

use std::sync::{Arc, Mutex};

/// One canned answer: method, exact path, HTTP status, body.
#[derive(Clone)]
pub struct Route {
    pub method: &'static str,
    pub path: String,
    pub status: u16,
    pub body: serde_json::Value,
}

pub fn get(path: impl Into<String>, body: serde_json::Value) -> Route {
    Route { method: "GET", path: path.into(), status: 200, body }
}

pub fn post(path: impl Into<String>, body: serde_json::Value) -> Route {
    Route { method: "POST", path: path.into(), status: 201, body }
}

pub fn not_found(path: impl Into<String>) -> Route {
    Route {
        method: "GET",
        path: path.into(),
        status: 404,
        body: serde_json::to_value(kube::core::Status::failure("not found", "NotFound").with_code(404))
            .expect("Status serializes"),
    }
}

/// What the client actually asked for, so a test can assert the absence of a call as well as its
/// result — "it did not try to re-home" is a real assertion.
#[derive(Default)]
pub struct Recorder(
    pub Arc<Mutex<Vec<String>>>,
    Arc<Mutex<Vec<(String, String, serde_json::Value)>>>,
    Arc<Mutex<Vec<String>>>,
);

impl Recorder {
    pub fn calls(&self) -> Vec<String> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// `calls`, with the query string kept: the selector a watch was opened with lives there, and
    /// "this watch is scoped to my node" is only assertable from it.
    pub fn requests(&self) -> Vec<String> {
        self.2.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The JSON bodies sent to `method path`, in order. Asserting on what was WRITTEN is the only
    /// way to test a handler whose whole output is an object in the API server.
    pub fn sent(&self, method: &str, path: &str) -> Vec<serde_json::Value> {
        self.1
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(m, p, _)| m == method && p == path)
            .map(|(_, _, b)| b.clone())
            .collect()
    }
}

/// Build a client answering `routes`. An unmatched request answers 404 rather than panicking, so a
/// test asserting "this path is never called" fails on its own assertion instead of a mock panic
/// buried in a task.
pub fn mock_client(routes: Vec<Route>) -> (kube::Client, Recorder) {
    let rec = Recorder::default();
    let seen = rec.0.clone();
    let bodies = rec.1.clone();
    let full = rec.2.clone();
    let svc = tower::service_fn(move |req: http::Request<kube::client::Body>| {
        let routes = routes.clone();
        let seen = seen.clone();
        let bodies = bodies.clone();
        let full = full.clone();
        async move {
            use http_body_util::BodyExt;
            let m = req.method().as_str().to_string();
            let p = req.uri().path().to_string();
            let pq = req.uri().path_and_query().map(|x| x.to_string()).unwrap_or_else(|| p.clone());
            full.lock().unwrap_or_else(|x| x.into_inner()).push(format!("{m} {pq}"));
            let (_, body) = req.into_parts();
            let raw = body.collect().await.map(|b| b.to_bytes()).unwrap_or_default();
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw) {
                bodies.lock().unwrap_or_else(|x| x.into_inner()).push((m.clone(), p.clone(), v));
            }
            seen.lock().unwrap_or_else(|x| x.into_inner()).push(format!("{m} {p}"));
            // Successive calls to the SAME method+path walk that path's routes in order, so a
            // test can say "404 first, then the winner's object" — which is exactly the shape of
            // the conflict-adopt flow. The last route repeats once its list is exhausted.
            let matching: Vec<&Route> = routes.iter().filter(|r| r.method == m && r.path == p).collect();
            let nth = {
                let log = seen.lock().unwrap_or_else(|x| x.into_inner());
                log.iter().filter(|c| *c == &format!("{m} {p}")).count().saturating_sub(1)
            };
            let hit = matching.get(nth.min(matching.len().saturating_sub(1))).copied();
            let (status, body) = match hit {
                Some(r) => (r.status, r.body.clone()),
                None => (
                    404,
                    serde_json::json!({
                        "kind": "Status", "apiVersion": "v1", "status": "Failure",
                        "reason": "NotFound", "code": 404,
                        "message": format!("mock has no route for {m} {p}")
                    }),
                ),
            };
            http::Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(kube::client::Body::from(serde_json::to_vec(&body).unwrap()))
        }
    });
    (kube::Client::new(svc, "default"), rec)
}

/// A stand-in for the server tier's volume browse routes (`bins/server/src/browse_api/volumes.rs`),
/// so the `/v1` volume handlers can be tested without a git node.
///
/// `volumes` is keyed by owner, `histories` by `"{owner}/{name}"`; anything absent answers 404,
/// which is exactly what the real one does for "not yours" as well as "not there". Returns the base
/// URL to hand `Upstream::new`. The peer secret is not checked — it is the real server tier's
/// business, and asserting it here would only test this stub.
pub async fn stub_registry(
    volumes: Vec<(&str, serde_json::Value)>,
    histories: Vec<(&str, serde_json::Value)>,
) -> String {
    use axum::response::IntoResponse;
    use axum::{extract::Path, routing::get, Router};
    use std::collections::HashMap;

    let vols: Arc<HashMap<String, serde_json::Value>> =
        Arc::new(volumes.into_iter().map(|(k, v)| (k.to_string(), v)).collect());
    let hist: Arc<HashMap<String, serde_json::Value>> =
        Arc::new(histories.into_iter().map(|(k, v)| (k.to_string(), v)).collect());
    let hist_del = hist.clone();
    let snap_del = hist.clone();
    let app = Router::new()
        // One snapshot: 404 for an unknown volume AND for an id that is not in its history, which
        // is the pair the api tier collapses into a single 404 of its own.
        .route(
            "/api/{owner}/{name}/snapshotdelete/{snapshot}",
            axum::routing::delete(
                move |Path((owner, name, snapshot)): Path<(String, String, String)>| {
                    let h = snap_del.clone();
                    async move {
                        let found = h
                            .get(&format!("{owner}/{name}"))
                            .and_then(|v| v.as_array())
                            .is_some_and(|recs| recs.iter().any(|r| r["id"] == snapshot));
                        match found {
                            true => axum::http::StatusCode::NO_CONTENT,
                            false => axum::http::StatusCode::NOT_FOUND,
                        }
                    }
                },
            ),
        )
        // The delete side of the same map: 404 when nothing was pushed under that name, so the
        // api tier's own scoping (which owner label it may ask as) is what the test exercises.
        .route(
            "/api/{owner}/{name}/volumedelete",
            axum::routing::delete(move |Path((owner, name)): Path<(String, String)>| {
                let h = hist_del.clone();
                async move {
                    match h.contains_key(&format!("{owner}/{name}")) {
                        true => axum::http::StatusCode::NO_CONTENT,
                        false => axum::http::StatusCode::NOT_FOUND,
                    }
                }
            }),
        )
        .route(
            "/api/{owner}/volumes",
            get(move |Path(owner): Path<String>| {
                let v = vols.clone();
                async move {
                    match v.get(&owner) {
                        Some(list) => axum::Json(list.clone()).into_response(),
                        None => axum::http::StatusCode::NOT_FOUND.into_response(),
                    }
                }
            }),
        )
        .route(
            "/api/{owner}/{name}/volumehistory",
            get(move |Path((owner, name)): Path<(String, String)>| {
                let h = hist.clone();
                async move {
                    match h.get(&format!("{owner}/{name}")) {
                        Some(list) => axum::Json(list.clone()).into_response(),
                        None => axum::http::StatusCode::NOT_FOUND.into_response(),
                    }
                }
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}
