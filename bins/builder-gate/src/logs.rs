//! Read-only service logs for a session, served from inside the cluster on port 1235.
//!
//! A workspace (or the bench, which is a workspace pod) asks `GET /logs/{env}/{service}` and the gate
//! reads that service's pod logs with its own ServiceAccount. Nothing goes through `/v1`: the owner
//! ruled that this "happens in the kubernetes cluster itself", so the api tier is not a dependency
//! of reading a log, and the tenant never holds a Kubernetes credential.
//!
//! Identity is the TCP source IP resolved through the same pod index the buildkit splice uses
//! (`who::Resolver`), never a header: a header is the tenant's to write, the source IP is the CNI's.
//! The decision (`plan`) is a pure function of the Environment CR and the caller's owner slug; the
//! kube calls below it are a thin shell. A missing environment, someone else's and a system one all
//! answer the SAME 404, so the route cannot be used to learn which environment ids exist.

use crate::who::{self, Resolver};
use k8s_openapi::api::core::v1::Pod;
use kloudlite_workspaces::crd::{self, Environment};
use kloudlite_workspaces::k8s::{KIND_LABEL, SERVICE_LABEL};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_TAIL: i64 = 200;
pub const MAX_TAIL: i64 = 2000;
pub const MAX_SINCE: i64 = 86_400;
const MAX_PODS: usize = 10;
/// Total log bytes per response, split across the pods.
const MAX_BYTES: i64 = 1 << 20;
const REQUEST_BUDGET: Duration = Duration::from_secs(10);
const RATE_WINDOW: Duration = Duration::from_secs(10);
const RATE_MAX: u32 = 10;

pub struct Logs {
    pub who: Arc<dyn Resolver>,
    pub kube: kube::Client,
    rate: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl Logs {
    pub fn new(who: Arc<dyn Resolver>, kube: kube::Client) -> Self {
        Self { who, kube, rate: Mutex::new(HashMap::new()) }
    }

    /// Fixed window per caller IP. A poisoned lock admits the request: refusing every log read
    /// forever over a counter would be the worse failure.
    // ponytail: in-memory per-replica window, a shared limiter if the gate ever runs >1 replica
    fn allow(&self, ip: IpAddr, now: Instant) -> bool {
        let Ok(mut m) = self.rate.lock() else { return true };
        if m.len() > 1024 {
            m.retain(|_, (t, _)| now.duration_since(*t) < RATE_WINDOW);
        }
        let e = m.entry(ip).or_insert((now, 0));
        if now.duration_since(e.0) >= RATE_WINDOW {
            *e = (now, 0);
        }
        e.1 += 1;
        e.1 <= RATE_MAX
    }
}

pub fn router(logs: Arc<Logs>) -> axum::Router {
    axum::Router::new()
        .fallback(move |axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
                        method: Method,
                        uri: axum::http::Uri| {
            let logs = logs.clone();
            async move {
                // Bounded as a whole: a slow API server is a 503 here, not a held connection.
                match tokio::time::timeout(REQUEST_BUDGET, handle(&logs, &method, &uri, peer.ip())).await {
                    Ok(r) => r,
                    Err(_) => err(StatusCode::SERVICE_UNAVAILABLE, "timed out"),
                }
            }
        })
        .layer(axum::middleware::from_fn(kloudlite_trace::traced))
}

fn err(code: StatusCode, msg: &str) -> Response {
    (code, [("content-type", "application/json")], json!({ "error": msg }).to_string()).into_response()
}

fn ok(v: Value) -> Response {
    (StatusCode::OK, [("content-type", "application/json")], v.to_string()).into_response()
}

pub async fn handle(logs: &Logs, method: &Method, uri: &axum::http::Uri, peer: IpAddr) -> Response {
    let segs: Vec<&str> = uri.path().trim_start_matches('/').split('/').collect();
    let (env, service) = match (method, segs.as_slice()) {
        (&Method::GET, ["logs", env, service]) => (*env, *service),
        _ => return err(StatusCode::NOT_FOUND, "not found"),
    };
    if !logs.allow(peer, Instant::now()) {
        return err(StatusCode::TOO_MANY_REQUESTS, "slow down");
    }
    // The splice path refuses before the first LIST for the same reason: an empty index would
    // read as "nobody" and turn a restart into a wave of 403s.
    if !logs.who.listed() {
        return err(StatusCode::SERVICE_UNAVAILABLE, "starting");
    }
    let Some((owner, team)) = logs.who.resolve(peer) else {
        return err(StatusCode::FORBIDDEN, "unknown caller");
    };
    let slug = who::slug_of(&owner, &team);
    // Before any API call: these two strings become a CR name and a label value.
    if !is_dns_label(env) || !is_dns_label(service) {
        return err(StatusCode::BAD_REQUEST, "env and service must be DNS labels");
    }
    let Ok(q) = parse_query(uri.query()) else {
        return err(StatusCode::BAD_REQUEST, "bad query");
    };
    let cr = match kube::Api::<Environment>::all(logs.kube.clone()).get_opt(env).await {
        Ok(cr) => cr,
        Err(e) => {
            tracing::warn!(error = %e, "logs.env.get");
            return err(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
        }
    };
    let plan = match plan(cr.as_ref(), &slug, service) {
        Ok(p) => p,
        Err(r) => return err(StatusCode::NOT_FOUND, r),
    };
    read(logs, plan, service, &q).await
}

pub struct Query {
    pub tail: i64,
    pub since: Option<i64>,
    pub previous: bool,
}

/// Over-large numbers are clamped, not refused; anything unparsable is a 400. Unknown keys are
/// ignored so a client can add one without breaking against an older gate.
pub fn parse_query(q: Option<&str>) -> Result<Query, &'static str> {
    let mut out = Query { tail: DEFAULT_TAIL, since: None, previous: false };
    for pair in q.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "tail" => out.tail = v.parse::<i64>().map_err(|_| "bad query")?.clamp(1, MAX_TAIL),
            "since" => out.since = Some(v.parse::<i64>().map_err(|_| "bad query")?.clamp(1, MAX_SINCE)),
            "previous" => out.previous = v.parse::<bool>().map_err(|_| "bad query")?,
            _ => {}
        }
    }
    Ok(out)
}

pub fn is_dns_label(s: &str) -> bool {
    let b = s.as_bytes();
    let edge = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    (1..=63).contains(&b.len()) && edge(b[0]) && edge(b[b.len() - 1]) && b.iter().all(|&c| edge(c) || c == b'-')
}

#[derive(Debug, PartialEq)]
pub struct Plan {
    pub namespace: String,
    pub intercepted_by: Option<String>,
}

/// May `slug` read `service` of this environment, and where do its pods live. `Environment.spec.owner`
/// is the owner slug (a team, or a person) and the caller's slug is the same fold `who::slug_of`
/// uses for builders, so equality is the whole rule — labels are never consulted. The `Err` text is
/// the 404 body.
pub fn plan(env: Option<&Environment>, slug: &str, service: &str) -> Result<Plan, &'static str> {
    // One body for missing, not yours and system.
    let env = env.filter(|e| e.spec.owner == slug && e.spec.system.is_none()).ok_or("not found")?;
    let namespace = crd::env_namespace(env.metadata.name.as_deref().unwrap_or_default());
    if !namespace.starts_with("env-") || namespace.len() > 63 {
        return Err("not found");
    }
    if !env.spec.services.iter().any(|s| s.name == service) {
        return Err("no such service");
    }
    let intercepted_by = env
        .spec
        .intercepts
        .iter()
        .find(|i| i.service == service)
        .map(|i| i.workspace.clone())
        .or_else(|| {
            let st = env.status.as_ref()?;
            st.service_status.iter().find(|s| s.name == service)?.intercepted_by.clone()
        });
    Ok(Plan { namespace, intercepted_by })
}

async fn read(logs: &Logs, plan: Plan, service: &str, q: &Query) -> Response {
    let pods: kube::Api<Pod> = kube::Api::namespaced(logs.kube.clone(), &plan.namespace);
    // `kind=environment` is what leaves the intercept proxy out: it carries the same service
    // label but `kind=intercept` (`k8s::intercept::proxy_selector`).
    let sel = format!("{KIND_LABEL}=environment,{SERVICE_LABEL}={service}");
    let list = match pods.list(&kube::api::ListParams::default().labels(&sel).limit(MAX_PODS as u32)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "logs.pods.list");
            return err(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
        }
    };
    let names: Vec<String> = list.items.iter().take(MAX_PODS).filter_map(|p| p.metadata.name.clone()).collect();
    let budget = MAX_BYTES / names.len().max(1) as i64;
    let lp = kube::api::LogParams {
        // The StatefulSet names its one container after the service.
        container: Some(service.to_string()),
        tail_lines: Some(q.tail),
        since_seconds: q.since,
        previous: q.previous,
        limit_bytes: Some(budget),
        ..Default::default()
    };
    let mut out = Vec::new();
    for name in names {
        out.push(match pods.logs(&name, &lp).await {
            Ok(log) => {
                let truncated = log.len() as i64 >= budget;
                let mut e = json!({ "pod": name, "log": log });
                if truncated {
                    e["truncated"] = json!(true);
                }
                e
            }
            Err(e) => {
                // The Kubernetes body can name nodes and internals; the caller gets a fixed line.
                tracing::warn!(pod = %name, error = %e, "logs.pod.read");
                json!({ "pod": name, "log": "", "error": "log unavailable" })
            }
        });
    }
    ok(body(service, plan.intercepted_by.as_deref(), out))
}

pub fn body(service: &str, intercepted_by: Option<&str>, pods: Vec<Value>) -> Value {
    let mut v = json!({ "service": service, "pods": pods });
    if let Some(ws) = intercepted_by {
        v["intercepted_by"] = json!(ws);
        v["note"] = json!(format!(
            "intercepted: its traffic runs in workspace {ws}; read that workspace's process output"
        ));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(owner: &str, system: Option<&str>, intercept: Option<&str>) -> Environment {
        let mut spec = json!({
            "owner": owner, "name": "app", "region": "r", "desiredState": "running",
            "services": [{ "name": "api", "image": "i", "command": [], "env": {}, "mounts": [] }],
            "intercepts": intercept.map(|w| vec![json!({ "service": "api", "workspace": w })]).unwrap_or_default(),
        });
        if let Some(s) = system {
            spec["system"] = json!(s);
        }
        serde_json::from_value(json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "Environment",
            "metadata": { "name": "env-abc" }, "spec": spec,
        }))
        .unwrap()
    }

    #[test]
    fn labels_are_dns_1123() {
        for ok in ["a", "env-1", "a-b-c", &"a".repeat(63)] {
            assert!(is_dns_label(ok), "{ok}");
        }
        for bad in ["", "-a", "a-", "A", "a_b", "a/b", "a.b", "..", &"a".repeat(64)] {
            assert!(!is_dns_label(bad), "{bad}");
        }
    }

    #[test]
    fn query_defaults_clamps_and_refuses() {
        let q = parse_query(None).unwrap();
        assert_eq!((q.tail, q.since, q.previous), (200, None, false));
        let q = parse_query(Some("tail=99999&since=999999&previous=true")).unwrap();
        assert_eq!((q.tail, q.since, q.previous), (2000, Some(86_400), true));
        assert!(parse_query(Some("tail=abc")).is_err());
        assert!(parse_query(Some("previous=maybe")).is_err());
    }

    #[test]
    fn only_the_owner_reads_and_every_refusal_reads_alike() {
        let mine = env("acme", None, None);
        let p = plan(Some(&mine), "acme", "api").unwrap();
        assert_eq!((p.namespace.as_str(), p.intercepted_by), ("env-abc", None));
        assert_eq!(plan(Some(&mine), "mallory", "api"), Err("not found"));
        assert_eq!(plan(None, "acme", "api"), Err("not found"));
        assert_eq!(plan(Some(&env("acme", Some("builder"), None)), "acme", "api"), Err("not found"));
        assert_eq!(plan(Some(&mine), "acme", "db"), Err("no such service"));
    }

    #[test]
    fn a_namespace_outside_env_is_refused() {
        let mut e = env("acme", None, None);
        // env_namespace always prefixes, so only an over-long id can break the rule; it must 404.
        e.metadata.name = Some("x".repeat(70));
        assert_eq!(plan(Some(&e), "acme", "api"), Err("not found"));
    }

    #[test]
    fn an_intercepted_service_says_who_has_it() {
        let p = plan(Some(&env("acme", None, Some("ws-1"))), "acme", "api").unwrap();
        assert_eq!(p.intercepted_by.as_deref(), Some("ws-1"));
        let v = body("api", p.intercepted_by.as_deref(), vec![]);
        assert_eq!(v["intercepted_by"], "ws-1");
        assert!(v["note"].as_str().unwrap().contains("workspace ws-1"));
        assert!(body("api", None, vec![]).get("note").is_none());
    }

    #[tokio::test]
    async fn the_eleventh_request_in_a_window_is_refused() {
        struct Acme;
        impl Resolver for Acme {
            fn resolve(&self, ip: IpAddr) -> Option<(String, String)> {
                (ip == "10.0.0.9".parse::<IpAddr>().unwrap()).then(|| ("bob".into(), "acme".into()))
            }
        }
        // A client for a port nothing listens on: every test here refuses before an API call.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cfg = kube::Config::new("http://127.0.0.1:1".parse().unwrap());
        let logs = Logs::new(Arc::new(Acme), kube::Client::try_from(cfg).unwrap());
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let t = Instant::now();
        assert!((0..10).all(|_| logs.allow(ip, t)));
        assert!(!logs.allow(ip, t));
        assert!(logs.allow("10.0.0.2".parse().unwrap(), t), "per caller");
        assert!(logs.allow(ip, t + RATE_WINDOW), "a new window");

        let get = |p: &str| p.parse::<axum::http::Uri>().unwrap();
        let ip: IpAddr = "10.0.0.9".parse().unwrap();
        let r = handle(&logs, &Method::GET, &get("/logs/BAD/api"), ip).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "refused before any API call (the client is dead)");
        let r = handle(&logs, &Method::GET, &get("/logs/e/api"), "10.9.9.9".parse().unwrap()).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "unknown caller");
        let r = handle(&logs, &Method::POST, &get("/logs/e/api"), ip).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let r = handle(&logs, &Method::GET, &get("/other"), ip).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }
}
