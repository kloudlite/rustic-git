//! A stand-in for `bins/api` plus a gateway: enough of the wire contract for the CLI to run
//! against, so the tests exercise the real binary rather than its inner functions.

use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct Stub {
    /// Every `/v1` request served — what `kl ws ssh` must keep to one.
    pub api_calls: Arc<AtomicUsize>,
}

/// Serves the api and the tunnel on one port; returns `http://127.0.0.1:port`.
pub async fn spawn(s: Stub) -> String {
    let app = Router::new()
        .route(
            "/v1/workspaces",
            get(|State(s): State<Stub>| async move {
                s.api_calls.fetch_add(1, Ordering::SeqCst);
                Json(serde_json::json!([
                    {"id": "ws-1", "name": "gh", "state": "ready", "packages": ["git", "go"]},
                    {"id": "ws-2", "name": "api", "state": "stopped", "packages": []},
                ]))
            }),
        )
        .route(
            "/v1/workspaces/{id}/ssh-session",
            post(|State(s): State<Stub>, Path(target): Path<String>| async move {
                s.api_calls.fetch_add(1, Ordering::SeqCst);
                // The real api resolves a name to an id; `gh` is ws-1's name above.
                let id = if target == "gh" { "ws-1".to_string() } else { target };
                (
                    axum::http::StatusCode::CREATED,
                    Json(serde_json::json!({
                        "id": id,
                        "token": "sst_test",
                        "gateway": format!("wss://ws-test.khost.dev/tunnel/{id}"),
                        "expires_at": "2030-01-01T00:00:00Z",
                        "host_key": "ssh-ed25519 AAAAKEY",
                    })),
                )
            }),
        )
        .route(
            "/tunnel/{id}",
            get(|ws: WebSocketUpgrade| async move {
                ws.on_upgrade(|mut sock| async move {
                    while let Some(Ok(m)) = sock.recv().await {
                        if let Message::Binary(b) = m {
                            if sock.send(Message::Binary(b)).await.is_err() {
                                break;
                            }
                        }
                    }
                }) as Response
            }),
        )
        .with_state(s);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

/// A logged-in config pointing at the stub.
pub fn write_config(dir: &std::path::Path, api: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("config.json"),
        serde_json::json!({"api": api, "token": "cli-token", "expires_at": "2030-01-01T00:00:00Z", "username": "k"})
            .to_string(),
    )
    .unwrap();
}
