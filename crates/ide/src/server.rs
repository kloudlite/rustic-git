//! The HTTP surface: `/healthz`, `/mcp`, and the two streams. Loopback only — see the crate doc.
use crate::tools::{files::Files, Registry};
use crate::Config;
use axum::{extract::State, routing::{get, post}, Json, Router};
use std::sync::Arc;

pub struct App {
    pub cfg: Config,
    pub registry: Registry,
}

impl App {
    pub fn new(cfg: Config) -> Self {
        let registry = Registry::new(vec![Box::new(Files { root: cfg.root.clone(), home: cfg.home.clone() })]);
        App { cfg, registry }
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new().route("/healthz", get(healthz)).route("/mcp", post(crate::mcp::handle)).with_state(app)
}

async fn healthz(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true, "root": app.cfg.root, "graph": "unknown" }))
}

pub async fn serve(cfg: Config) -> anyhow::Result<()> {
    let bind = cfg.bind;
    let app = Arc::new(App::new(cfg));
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

    async fn rpc(app: &Arc<App>, body: serde_json::Value) -> serde_json::Value {
        let r = router(app.clone()).oneshot(axum::http::Request::post("/mcp").header("content-type", "application/json").body(axum::body::Body::from(body.to_string())).unwrap()).await.unwrap();
        assert_eq!(r.status(), 200);
        serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap()
    }

    /// The whole MCP handshake a client makes, against a tempdir root.
    #[tokio::test]
    async fn mcp_initialize_lists_and_calls_the_file_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let root = home.join("workspaces/api");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        let app = Arc::new(App::new(Config { bind: "127.0.0.1:0".parse().unwrap(), root, home, graft_dir: None }));
        let v = rpc(&app, serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})).await;
        assert_eq!(v["result"]["serverInfo"]["name"], "kl-ide");
        let v = rpc(&app, serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).await;
        let names: Vec<&str> = v["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["read", "write", "edit", "glob", "grep"]);
        let v = rpc(&app, serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read","arguments":{"path":"a.txt"}}})).await;
        assert_eq!(v["result"]["isError"], false);
        assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("two"));
        let v = rpc(&app, serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"read","arguments":{"path":"/etc/passwd"}}})).await;
        assert_eq!(v["result"]["isError"], true);
        let v = rpc(&app, serde_json::json!({"jsonrpc":"2.0","id":5,"method":"nope"})).await;
        assert_eq!(v["error"]["code"], -32601);
    }
}
