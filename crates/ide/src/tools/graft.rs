//! The graft tools: six proxied to `graft mcp`, `graft_build` as a detached process, `graft_blast`
//! as a job. Schemas are a fixed table (graft 0.18's own, copied) so `tools/list` answers while
//! the child is still starting.
use super::{opt_bool, opt_str, opt_u64, Tool, ToolError, ToolSet};
use crate::graft::{Graft, GraphState};
use crate::procs::Procs;
use futures::future::BoxFuture;
use futures::FutureExt;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct GraftTools {
    pub graft: Arc<Graft>,
    pub procs: Arc<Procs>,
}

pub const PROXIED: [&str; 6] = ["graft_find_code", "graft_find_all", "graft_trace_calls", "graft_file_api", "graft_repo_map", "graft_check_freshness"];

fn obj(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

pub fn table() -> Vec<Tool> {
    vec![
        Tool { name: "graft_find_code", description: "Query the repo context graph in plain words: ranked nodes with exact file:line spans and the relevant source inlined. Usually the whole answer, no file read needed.", schema: obj(json!({ "query": {"type":"string"}, "limit": {"type":"integer"}, "full": {"type":"boolean"}, "in": {"type":"string"} }), &["query"]) },
        Tool { name: "graft_find_all", description: "Regex search over the graph's indexed files, hits grouped by innermost enclosing symbol and ranked by coupling — every occurrence, not top-N.", schema: obj(json!({ "pattern": {"type":"string"}, "in": {"type":"string"}, "ignore_case": {"type":"boolean"}, "fixed": {"type":"boolean"} }), &["pattern"]) },
        Tool { name: "graft_trace_calls", description: "Structural edges for a symbol: direct callers by default; direction \"out\" for callees; depth N or \"all\" for the transitive closure (blast radius before a change).", schema: obj(json!({ "symbol": {"type":"string"}, "direction": {"type":"string","enum":["in","out"]}, "depth": {}, "in": {"type":"string"} }), &["symbol"]) },
        Tool { name: "graft_file_api", description: "Signatures-only view of one file: every definition's signature and line span, about a tenth of the tokens of reading it.", schema: obj(json!({ "file": {"type":"string"} }), &["file"]) },
        Tool { name: "graft_repo_map", description: "Token-budgeted repo orientation: directory clusters, per-directory hubs, global hotspots.", schema: obj(json!({ "max_dirs": {"type":"integer"} }), &[]) },
        Tool { name: "graft_check_freshness", description: "Is the graph in sync with the code? The drift report.", schema: obj(json!({}), &[]) },
        Tool { name: "graft_build", description: "Rebuild the graph as a detached process (answers {id}; read it with process_output). deep:true adds the LLM concept map and per-symbol summaries and needs GRAFT_PROVIDER/GRAFT_API_KEY in the workspace; no_reuse re-parses every file.", schema: obj(json!({ "deep": {"type":"boolean"}, "no_reuse": {"type":"boolean"} }), &[]) },
        Tool { name: "graft_blast", description: "Blast radius of a diff: what depends on the lines the change touched. base is a git ref (default HEAD); depth N or \"all\".", schema: obj(json!({ "base": {"type":"string"}, "depth": {} }), &[]) },
    ]
}

impl ToolSet for GraftTools {
    fn tools(&self) -> Vec<Tool> {
        table()
    }

    fn call<'a>(&'a self, name: &'a str, args: Value) -> BoxFuture<'a, Result<Value, ToolError>> {
        async move {
            if self.graft.state() == GraphState::Unavailable {
                return Err(ToolError::Failed("graft is not installed in this workspace image".into()));
            }
            match name {
                n if PROXIED.contains(&n) => {
                    if self.graft.state() == GraphState::Building && self.graft.root.join("graft").exists() {
                        // A refresh in flight answers from the graph on disk; a first build has nothing yet.
                    } else if self.graft.state() == GraphState::Building {
                        return Err(ToolError::Failed("the graph is being built for the first time; retry in a moment".into()));
                    }
                    self.graft.call(n, args).await.map_err(ToolError::Failed)
                }
                "graft_build" => {
                    if opt_bool(&args, "deep") && (std::env::var_os("GRAFT_PROVIDER").is_none() || std::env::var_os("GRAFT_API_KEY").is_none()) {
                        return Err(ToolError::Failed("no_provider: a deep build needs GRAFT_PROVIDER and GRAFT_API_KEY in the workspace".into()));
                    }
                    let mut c = tokio::process::Command::new("graft");
                    if let Some(d) = &self.graft.graft_dir {
                        c.arg("--dir").arg(d);
                    }
                    c.arg("build");
                    if opt_bool(&args, "deep") {
                        c.arg("--deep");
                    }
                    if opt_bool(&args, "no_reuse") {
                        c.arg("--no-reuse");
                    }
                    c.arg(&self.graft.root).current_dir(&self.graft.root).env("DO_NOT_TRACK", "1").process_group(0);
                    let id = self.procs.spawn(c, "graft build".into()).map_err(ToolError::Failed)?;
                    Ok(json!({ "id": id }))
                }
                "graft_blast" => {
                    let depth = args.get("depth").map(|d| match d { Value::Number(n) => n.to_string(), Value::String(s) => s.clone(), _ => "1".into() });
                    let _ = opt_u64(&args, "depth");
                    self.graft.blast(opt_str(&args, "base"), depth.as_deref()).await.map_err(ToolError::Failed)
                }
                other => Err(ToolError::Unknown(other.to_string())),
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_carries_the_eight_graft_tools_with_object_schemas() {
        let t = table();
        assert_eq!(t.len(), 8);
        for tool in &t {
            assert!(tool.name.starts_with("graft_"));
            assert_eq!(tool.schema["type"], "object");
        }
        assert!(PROXIED.iter().all(|p| t.iter().any(|t| t.name == *p)));
    }
}
