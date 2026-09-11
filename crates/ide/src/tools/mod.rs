//! One `ToolSet` per family; `Registry` is what `/mcp` dispatches on. Every call is logged once
//! (`ide.call`: tool, ms, ok, bytes) — the pod log is the record, there is no metrics endpoint.
pub mod exec;
pub mod files;

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
}

impl Registry {
    pub fn new(sets: Vec<Box<dyn ToolSet>>) -> Self {
        Registry { sets }
    }

    pub fn tools(&self) -> Vec<Tool> {
        self.sets.iter().flat_map(|s| s.tools()).collect()
    }

    pub async fn call(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        let start = Instant::now();
        let set = self.sets.iter().find(|s| s.tools().iter().any(|t| t.name == name));
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
