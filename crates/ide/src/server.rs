//! The HTTP surface: `/healthz`, `/tools`, and the two streams. Loopback only — see the crate doc.
use crate::graft::Graft;
use crate::procs::Procs;
use crate::tools::{exec::Exec, files::Files, graft::GraftTools, watch::WatchTools, Registry};
use crate::watches::Watches;
use crate::Config;
use axum::{extract::State, routing::{get, post}, Json, Router};
use std::sync::Arc;

pub struct App {
    pub cfg: Config,
    pub registry: Registry,
    pub procs: Arc<Procs>,
    pub watches: Arc<Watches>,
    pub graft: Arc<Graft>,
}

impl App {
    /// Wires everything; `Graft::start` is the caller's (`serve`) so a test App spawns nothing.
    pub fn new(cfg: Config) -> Self {
        let procs = Arc::new(Procs::default());
        let watches = Arc::new(Watches::default());
        let graft = Graft::new(cfg.root.clone(), cfg.graft_dir.clone());
        let after: Arc<dyn Fn() + Send + Sync> = {
            let g = graft.clone();
            Arc::new(move || g.refresh_soon())
        };
        let registry = Registry::new(vec![
            Box::new(Files { root: cfg.root.clone(), home: cfg.home.clone(), after_change: Some(after.clone()) }),
            Box::new(Exec { root: cfg.root.clone(), home: cfg.home.clone(), procs: procs.clone(), after_change: Some(after) }),
            Box::new(WatchTools { root: cfg.root.clone(), home: cfg.home.clone(), procs: procs.clone(), watches: watches.clone() }),
            Box::new(GraftTools { graft: graft.clone(), procs: procs.clone() }),
        ]);
        App { cfg, registry, procs, watches, graft }
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/tools", get(crate::api::list))
        .route("/tools/{name}", post(crate::api::call))
        .route("/stream/process/{id}", get(crate::stream::process))
        .route("/stream/watch/{id}", get(crate::stream::watch))
        .with_state(app)
}

async fn healthz(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true, "root": app.cfg.root, "graph": app.graft.state() }))
}

pub async fn serve(cfg: Config) -> anyhow::Result<()> {
    let bind = cfg.bind;
    let app = Arc::new(App::new(cfg));
    app.graft.start();
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "ide.listening");
    axum::serve(listener, router(app)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    #[tokio::test]
    async fn healthz_names_the_root() {
        let cfg = Config { bind: "127.0.0.1:0".parse().unwrap(), root: "/home/kl/workspaces/api".into(), home: "/home/kl".into(), graft_dir: None };
        let r = router(Arc::new(App::new(cfg))).oneshot(axum::http::Request::get("/healthz").body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), 200);
        let b = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["root"], "/home/kl/workspaces/api");
    }

    async fn post(app: &Arc<App>, name: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        let r = router(app.clone()).oneshot(axum::http::Request::post(format!("/tools/{name}")).header("content-type", "application/json").body(axum::body::Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = r.status().as_u16();
        (status, serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap())
    }

    /// The whole surface a session layer uses, against a tempdir root: list, call, and every
    /// error as an HTTP status rather than a field inside a tool's own answer.
    #[tokio::test]
    async fn the_tool_api_lists_calls_and_answers_errors_as_statuses() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let root = home.join("workspaces/api");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        let app = Arc::new(App::new(Config { bind: "127.0.0.1:0".parse().unwrap(), root, home, graft_dir: None }));
        let r = router(app.clone()).oneshot(axum::http::Request::get("/tools").body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap();
        let names: Vec<&str> = v["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["read", "write", "edit", "glob", "grep", "exec", "process_list", "process_output", "process_write", "process_kill", "watch", "watch_poll", "watch_stop", "graft_find_code", "graft_find_all", "graft_trace_calls", "graft_file_api", "graft_repo_map", "graft_build", "graft_blast"]);
        assert_eq!(v["tools"][0]["schema"]["type"], "object");
        let (s, v) = post(&app, "read", serde_json::json!({"path": "a.txt"})).await;
        assert_eq!(s, 200);
        assert!(v["content"].as_str().unwrap().contains("two"));
        let (s, v) = post(&app, "read", serde_json::json!({"path": "/etc/passwd"})).await;
        assert_eq!(s, 403, "{v}");
        assert!(v["error"].as_str().unwrap().contains("outside"), "{v}");
        let (s, _) = post(&app, "read", serde_json::json!({})).await;
        assert_eq!(s, 400);
        let (s, _) = post(&app, "nope", serde_json::json!({})).await;
        assert_eq!(s, 404);
    }
}
