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

