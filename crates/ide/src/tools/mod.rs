//! One `ToolSet` per family; `Registry` is what `/tools/{name}` dispatches on. Every call is logged once
//! (`ide.call`: tool, ms, ok, bytes) — the pod log is the record, there is no metrics endpoint.
pub mod exec;
pub mod files;
pub mod graft;
pub mod patch;
pub mod watch;

use futures::future::BoxFuture;
use serde_json::Value;
use std::time::Instant;

#[derive(Debug)]
pub enum ToolError {
    Unknown(String),
    Invalid(String),
    Denied(String),
    Failed(String),
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::Unknown(t) => write!(f, "unknown tool {t}"),
            ToolError::Invalid(m) | ToolError::Denied(m) | ToolError::Failed(m) => f.write_str(m),
        }
    }
}

/// The HTTP status a tool error is: 404 unknown, 400 bad arguments, 403 outside the home, 500 failed.
pub fn status_of(e: &ToolError) -> axum::http::StatusCode {
    use axum::http::StatusCode;
    match e {
        ToolError::Unknown(_) => StatusCode::NOT_FOUND,
        ToolError::Invalid(_) => StatusCode::BAD_REQUEST,
        ToolError::Denied(_) => StatusCode::FORBIDDEN,
        ToolError::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
}

pub trait ToolSet: Send + Sync {
    fn tools(&self) -> Vec<Tool>;
    fn call<'a>(&'a self, name: &'a str, args: Value) -> BoxFuture<'a, Result<Value, ToolError>>;
}

pub struct Registry {
    sets: Vec<Box<dyn ToolSet>>,
    /// Built once: `call` used to ask every set for its whole `tools()` vec on every request,
    /// which rebuilt every schema to find one name (2026-09-12).
    index: std::collections::HashMap<String, usize>,
}

impl Registry {
    pub fn new(sets: Vec<Box<dyn ToolSet>>) -> Self {
        let mut index = std::collections::HashMap::new();
        for (i, s) in sets.iter().enumerate() {
            for t in s.tools() {
                index.insert(t.name.to_string(), i);
            }
        }
        Registry { sets, index }
    }

    pub fn tools(&self) -> Vec<Tool> {
        self.sets.iter().flat_map(|s| s.tools()).collect()
    }

    pub async fn call(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        let start = Instant::now();
        let set = self.index.get(name).map(|i| &self.sets[*i]);
        let r = match set {
            Some(s) => s.call(name, args).await,
            None => Err(ToolError::Unknown(name.to_string())),
        };
        let bytes = r.as_ref().map(|v| v.to_string().len()).unwrap_or(0);
        tracing::info!(tool = name, ms = start.elapsed().as_millis() as u64, ok = r.is_ok(), bytes, "ide.call");
        r
    }
}

/// Argument readers with the tool's name in every error.
pub(crate) fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| ToolError::Invalid(format!("`{key}` (string) is required")))
}
pub(crate) fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}
pub(crate) fn opt_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}
pub(crate) fn opt_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// An argv array's entries. A non-string used to be dropped silently, which shifted every
/// argument after it and ran a different command than the caller wrote (2026-09-12).
pub(crate) fn argv(a: &[Value]) -> Result<Vec<&str>, ToolError> {
    a.iter().map(|v| v.as_str().ok_or_else(|| ToolError::Invalid(format!("cmd[]: `{v}` is not a string")))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_argv_entry_that_is_not_a_string_is_refused_by_value() {
        assert_eq!(argv(&[serde_json::json!("echo"), serde_json::json!("hi")]).unwrap(), vec!["echo", "hi"]);
        let e = argv(&[serde_json::json!("echo"), serde_json::json!(1)]).unwrap_err();
        assert!(matches!(e, ToolError::Invalid(ref m) if m.contains('1')), "{e:?}");
    }
}
