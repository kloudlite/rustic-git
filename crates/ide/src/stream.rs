//! The two WebSocket streams. `/stream/process/{id}`: the ring from offset 0, then every frame
//! as it arrives, then the exit. `/stream/watch/{id}`: the same shape over a watch's events.
//! Text frames, lossy UTF-8, one JSON object each.
use crate::procs::Frame;
use crate::server::App;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use std::sync::Arc;

pub async fn watch(State(app): State<Arc<App>>, Path(id): Path<String>, ws: WebSocketUpgrade) -> axum::response::Response {
    match app.watches.get(&id) {
        Some(w) => ws.on_upgrade(move |sock| pump_watch(sock, w)).into_response(),
        None => (axum::http::StatusCode::NOT_FOUND, format!("no watch {id}")).into_response(),
    }
}

async fn pump_watch(mut sock: WebSocket, w: Arc<std::sync::Mutex<crate::watches::Watch>>) {
    let (mut rx, replay, stopped) = {
        let g = w.lock().unwrap_or_else(|q| q.into_inner());
        let (events, _, _) = g.read_since(0);
        (g.tx.subscribe(), events, g.state == crate::watches::State::Stopped)
    };
    for ev in replay {
        if !send(&mut sock, ev).await {
            return;
        }
    }
    if stopped {
        let _ = send(&mut sock, serde_json::json!({ "state": "stopped" })).await;
        return;
    }
    loop {
        match rx.recv().await {
            Ok(ev) => {
                if !send(&mut sock, ev).await {
                    return;
                }
                if w.lock().unwrap_or_else(|q| q.into_inner()).state == crate::watches::State::Stopped {
                    let _ = send(&mut sock, serde_json::json!({ "state": "stopped" })).await;
                    return;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                if !send(&mut sock, serde_json::json!({ "dropped_events": n })).await {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

pub async fn process(State(app): State<Arc<App>>, Path(id): Path<String>, ws: WebSocketUpgrade) -> axum::response::Response {
    match app.procs.get(&id) {
        Some(p) => ws.on_upgrade(move |sock| pump(sock, p)).into_response(),
        None => (axum::http::StatusCode::NOT_FOUND, format!("no process {id}")).into_response(),
    }
}

async fn send(sock: &mut WebSocket, v: serde_json::Value) -> bool {
    sock.send(Message::Text(v.to_string().into())).await.is_ok()
}

async fn pump(mut sock: WebSocket, p: Arc<std::sync::Mutex<crate::procs::Proc>>) {
    // Subscribe BEFORE replaying so nothing printed between the two is lost; a frame replayed
    // and then received again is the one duplicate this ordering allows, at the seam only.
    let (mut rx, replay, exit) = {
        let g = p.lock().unwrap_or_else(|q| q.into_inner());
        let (o, _, _) = g.out.read_since(0);
        let (e, _, _) = g.err.read_since(0);
        (g.tx.subscribe(), (o, e), g.exit_code)
    };
    if !replay.0.is_empty() && !send(&mut sock, serde_json::json!({ "stream": "stdout", "data": String::from_utf8_lossy(&replay.0) })).await {
        return;
    }
    if !replay.1.is_empty() && !send(&mut sock, serde_json::json!({ "stream": "stderr", "data": String::from_utf8_lossy(&replay.1) })).await {
        return;
    }
    if let Some(code) = exit {
        let _ = send(&mut sock, serde_json::json!({ "exit": code })).await;
        return;
    }
    loop {
        match rx.recv().await {
            Ok(Frame::Stdout(b)) => {
                if !send(&mut sock, serde_json::json!({ "stream": "stdout", "data": String::from_utf8_lossy(&b) })).await {
                    return;
                }
            }
            Ok(Frame::Stderr(b)) => {
                if !send(&mut sock, serde_json::json!({ "stream": "stderr", "data": String::from_utf8_lossy(&b) })).await {
                    return;
                }
            }
            Ok(Frame::Exit(code)) => {
                let _ = send(&mut sock, serde_json::json!({ "exit": code })).await;
                return;
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                if !send(&mut sock, serde_json::json!({ "dropped_frames": n })).await {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}
