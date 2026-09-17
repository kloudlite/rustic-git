//! The two WebSocket streams. `/stream/process/{id}`: the ring from `?since=` (0 by default),
//! then every frame as it arrives, then the exit. `/stream/watch/{id}`: the same shape over a
//! watch's events. Text frames, lossy UTF-8, one JSON object each.
//!
//! `?since=` is what makes a RECONNECT cheap. Without it every dial replayed the whole ring, so a
//! client that reopens the stream per notification saw the same `#N DONE` lines again on each one
//! and a build's log arrived from byte 0 forever (workspace session, 2026-09-18). The offset is
//! the `next` a previous read or frame answered with; anything the ring has already dropped is
//! reported as `dropped`, never silently skipped.
use crate::procs::Frame;
use crate::server::App;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use std::sync::Arc;

/// `?since=N&since_err=N`, each tolerated as a number or a decimal string: a client that hands its
/// cursor back as text is asking the same question, and answering it from 0 is the bug this exists
/// to prevent. `since_err` is stderr's own cursor — buildkit writes its whole progress there, so
/// one shared offset replayed a build's log on every reconnect.
#[derive(serde::Deserialize, Default)]
pub struct Since {
    #[serde(default, deserialize_with = "de_since")]
    since: u64,
    #[serde(default, deserialize_with = "de_since")]
    since_err: u64,
}

fn de_since<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    match serde::Deserialize::deserialize(d)? {
        serde_json::Value::Number(n) => Ok(n.as_u64().unwrap_or(0)),
        serde_json::Value::String(s) => Ok(s.trim().parse().unwrap_or(0)),
        _ => Ok(0),
    }
}

pub async fn watch(State(app): State<Arc<App>>, Path(id): Path<String>, Query(q): Query<Since>, ws: WebSocketUpgrade) -> axum::response::Response {
    match app.watches.get(&id) {
        Some(w) => ws.on_upgrade(move |sock| pump_watch(sock, w, q.since)).into_response(),
        None => (axum::http::StatusCode::NOT_FOUND, format!("no watch {id}")).into_response(),
    }
}

async fn pump_watch(mut sock: WebSocket, w: Arc<std::sync::Mutex<crate::watches::Watch>>, since: u64) {
    let (mut rx, replay, dropped, stopped) = {
        let g = w.lock().unwrap_or_else(|q| q.into_inner());
        let (events, _, dropped) = g.read_since(since);
        (g.tx.subscribe(), events, dropped, g.state == crate::watches::State::Stopped)
    };
    if dropped > 0 && !send(&mut sock, serde_json::json!({ "dropped_events": dropped })).await {
        return;
    }
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

pub async fn process(State(app): State<Arc<App>>, Path(id): Path<String>, Query(q): Query<Since>, ws: WebSocketUpgrade) -> axum::response::Response {
    match app.procs.get(&id) {
        Some(p) => ws.on_upgrade(move |sock| pump(sock, p, q.since, q.since_err)).into_response(),
        None => (axum::http::StatusCode::NOT_FOUND, format!("no process {id}")).into_response(),
    }
}

async fn send(sock: &mut WebSocket, v: serde_json::Value) -> bool {
    sock.send(Message::Text(v.to_string().into())).await.is_ok()
}

async fn pump(mut sock: WebSocket, p: Arc<std::sync::Mutex<crate::procs::Proc>>, since: u64, since_err: u64) {
    // Subscribe BEFORE replaying so nothing printed between the two is lost; a frame replayed
    // and then received again is the one duplicate this ordering allows, at the seam only.
    //
    // Two cursors, because the two streams advance independently: buildkit writes its progress to
    // STDERR, so replaying stderr from 0 while honouring `since` on stdout re-sent a whole build
    // log on every reconnect and the workspace model looped rebuilding the image (2026-09-18).
    let (mut rx, replay, dropped, exit) = {
        let g = p.lock().unwrap_or_else(|q| q.into_inner());
        let (o, _, dropped) = g.out.read_since(since);
        let (e, _, dropped_err) = g.err.read_since(since_err);
        (g.tx.subscribe(), (o, e), (dropped, dropped_err), g.exit_code)
    };
    if (dropped.0 > 0 || dropped.1 > 0) && !send(&mut sock, serde_json::json!({ "dropped": dropped.0, "dropped_err": dropped.1 })).await {
        return;
    }
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
