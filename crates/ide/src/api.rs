//! The tool API: `GET /tools` lists every tool with its JSON schema, `POST /tools/{name}` runs one
//! with the request body as its arguments and answers the tool's JSON. Plain HTTP on purpose — MCP
//! belongs to the session layer above, which speaks it once for every workspace a person holds and
//! forwards `workspace + tool + args` here; a protocol inside the pod would be spoken twice.
//!
//! Errors are HTTP: 404 an unknown tool, 400 bad arguments, 403 a path outside the home, 500 the
//! tool itself failed — always `{"error": "..."}`, so the caller never parses a tool's own answer
//! to learn it did not run.
use crate::server::App;
use crate::tools::status_of;
use axum::{extract::{Path, State}, http::StatusCode, Json};
use serde_json::{json, Value};
use std::sync::Arc;

pub async fn list(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!({ "tools": app.registry.tools().into_iter().map(|t| json!({
        "name": t.name, "description": t.description, "schema": t.schema })).collect::<Vec<_>>() }))
}

pub async fn call(State(app): State<Arc<App>>, Path(name): Path<String>, body: Option<Json<Value>>) -> (StatusCode, Json<Value>) {
    let args = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    match app.registry.call(&name, args).await {
        Ok(v) => (StatusCode::OK, Json(v)),
        Err(e) => (status_of(&e), Json(json!({ "error": e.to_string() }))),
    }
}
