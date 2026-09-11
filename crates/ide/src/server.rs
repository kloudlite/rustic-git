//! The HTTP surface: `/healthz`, `/mcp`, and the two streams. Loopback only — see the crate doc.
use crate::Config;
use axum::{extract::State, routing::get, Json, Router};
use std::sync::Arc;

pub struct App {
    pub cfg: Config,
}

pub fn router(app: Arc<App>) -> Router {
    Router::new().route("/healthz", get(healthz)).with_state(app)
}

async fn healthz(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true, "root": app.cfg.root, "graph": "unknown" }))
}

pub async fn serve(cfg: Config) -> anyhow::Result<()> {
    let bind = cfg.bind;
    let app = Arc::new(App { cfg });
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
        let r = router(Arc::new(App { cfg })).oneshot(axum::http::Request::get("/healthz").body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), 200);
        let b = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["root"], "/home/kl/workspaces/api");
    }
}
