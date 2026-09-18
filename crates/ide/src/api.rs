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
use axum::{extract::{Path, Query, State}, http::StatusCode, Json};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

pub async fn list(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!({ "tools": app.registry.tools().into_iter().map(|t| json!({
        "name": t.name, "description": t.description, "schema": t.schema })).collect::<Vec<_>>() }))
}

pub async fn call(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    body: Option<Json<Value>>,
) -> (StatusCode, Json<Value>) {
    let args = with_tree(body.map(|Json(v)| v).unwrap_or_else(|| json!({})), q.get("tree"));
    let (status, body) = match app.registry.call(&name, args).await {
        Ok(v) => (StatusCode::OK, v),
        Err(e) => (status_of(&e), json!({ "error": e.to_string() })),
    };
    // Only a name the registry knew: a 404's name is whatever the caller typed.
    if status != StatusCode::NOT_FOUND {
        kloudlite_trace::set_attribute(&tracing::Span::current(), "kl.tool.name", name);
    }
    (status, Json(body))
}

/// `?tree=` merged into the arguments, body first.
///
/// ONE place, for every tool. The query string used to be discarded outright: `exec?tree=T` ran in
/// main and `write?tree=T` WROTE into main, so a caller that believed the URL form did anything
/// got silent cross-tree writes, and only calls that pass `tree` in the body — the agents' own —
/// were confined at all (R-D22, 2026-09-18). Merging here rather than in each tool is the point:
/// a tool added later cannot be the next one to ignore it.
///
/// The BODY wins when both are given. A session pins its tree per call in the body, and a URL
/// somebody typed must never quietly move that session somewhere else; the query is the
/// convenience form, so it is the one that yields.
///
/// A body that is not an object is left alone: the tool refuses it by name, which is a better
/// error than one about a field it never asked for.
fn with_tree(mut args: Value, tree: Option<&String>) -> Value {
    let Some(tree) = tree else { return args };
    let Some(o) = args.as_object_mut() else { return args };
    o.entry("tree").or_insert_with(|| json!(tree));
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_wins_and_the_query_fills_in() {
        let t = |v: &Value| v.get("tree").and_then(Value::as_str).map(str::to_string);
        // Filled in from the query when the body says nothing.
        assert_eq!(t(&with_tree(json!({"path": "a"}), Some(&"x".to_string()))), Some("x".into()));
        // The body wins, so a pinned session is never moved by a URL.
        assert_eq!(t(&with_tree(json!({"tree": "b"}), Some(&"x".to_string()))), Some("b".into()));
        // Even when the body pins MAIN explicitly: that is still the session speaking.
        assert_eq!(t(&with_tree(json!({"tree": "main"}), Some(&"x".to_string()))), Some("main".into()));
        // No query, no change.
        assert_eq!(t(&with_tree(json!({"path": "a"}), None)), None);
        // A body that is not an object is the tool's to refuse, by its own name.
        assert_eq!(with_tree(json!("nonsense"), Some(&"x".to_string())), json!("nonsense"));
    }
}
