//! MCP over streamable HTTP, the JSON half only: `POST /mcp` takes one JSON-RPC 2.0 request and
//! answers one JSON body (no SSE in this cut — every tool here answers in one piece, and the two
//! things that stream have their own WebSocket routes). Hand-rolled: four methods and an error
//! envelope are less to own than a protocol crate's surface.
use crate::server::App;
use crate::tools::ToolError;
use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub const PROTOCOL: &str = "2025-03-26";

#[derive(Deserialize)]
pub struct Request {
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

pub async fn handle(State(app): State<Arc<App>>, Json(req): Json<Request>) -> (StatusCode, Json<Value>) {
    let id = req.id.clone().unwrap_or(Value::Null);
    let result = match req.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "kl-ide", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Tools run inside this workspace pod as the workspace user. Paths are relative to the workspace directory. `exec` with detach:true starts a process you read with process_output or /stream/process/{id}.",
        })),
        "notifications/initialized" => return (StatusCode::NO_CONTENT, Json(Value::Null)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": app.registry.tools().into_iter().map(|t| json!({
            "name": t.name, "description": t.description, "inputSchema": t.schema })).collect::<Vec<_>>() })),
        "tools/call" => {
            let name = req.params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = req.params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match app.registry.call(name, args).await {
                Ok(v) => Ok(json!({ "content": [{ "type": "text", "text": v.to_string() }], "isError": false })),
                Err(ToolError::Unknown(t)) => Err((-32602, format!("unknown tool {t}"))),
                Err(e) => Ok(json!({ "content": [{ "type": "text", "text": e.to_string() }], "isError": true })),
            }
        }
        other => Err((-32601, format!("method not found: {other}"))),
    };
    let body = match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, msg)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": msg } }),
    };
    (StatusCode::OK, Json(body))
}
