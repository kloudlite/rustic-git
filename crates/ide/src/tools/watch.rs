//! `watch`, `watch_poll`, `watch_stop`.
use super::{argv, opt_bool, opt_str, opt_u64, str_arg, Tool, ToolError, ToolSet};
use crate::paths::confine;
use crate::procs::Procs;
use crate::watches::Watches;
use futures::future::BoxFuture;
use futures::FutureExt;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

pub struct WatchTools {
    pub root: PathBuf,
    pub home: PathBuf,
    pub procs: Arc<Procs>,
    pub watches: Arc<Watches>,
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

impl ToolSet for WatchTools {
    fn tools(&self) -> Vec<Tool> {
        vec![
            Tool { name: "watch", description: "Start a watch and answer {id}. Either paths[] (file-system changes under them, recursive) or cmd (a command whose output lines are events, filtered by the regex pattern when given). once:true ends the watch at the first event. Read with watch_poll or GET /stream/watch/{id}.", schema: obj(json!({ "paths": {"type":"array","items":{"type":"string"}}, "cmd": {}, "pattern": {"type":"string"}, "once": {"type":"boolean"} }), &[]) },
            Tool { name: "watch_poll", description: "Events since an index (0 = from the start; the ring keeps the last 1000). Answers events, next, dropped, state.", schema: obj(json!({ "id": {"type":"string"}, "since": {"type":"integer"} }), &["id"]) },
            Tool { name: "watch_stop", description: "Stop a watch (and its command, if any).", schema: obj(json!({ "id": {"type":"string"} }), &["id"]) },
        ]
    }

    fn call<'a>(&'a self, name: &'a str, args: Value) -> BoxFuture<'a, Result<Value, ToolError>> {
        async move {
            match name {
                "watch" => {
                    let once = opt_bool(&args, "once");
                    if let Some(paths) = args.get("paths").and_then(Value::as_array) {
                        let mut confined = Vec::new();
                        for p in paths {
                            let p = p.as_str().ok_or_else(|| ToolError::Invalid("paths[] must be strings".into()))?;
                            confined.push(confine(&self.root, &self.home, p)?);
                        }
                        if confined.is_empty() {
                            return Err(ToolError::Invalid("paths[] is empty".into()));
                        }
                        let id = self.watches.watch_paths(confined, once).map_err(ToolError::Failed)?;
                        return Ok(json!({ "id": id }));
                    }
                    let Some(cmd) = args.get("cmd") else { return Err(ToolError::Invalid("either paths[] or cmd is required".into())) };
                    let pattern = opt_str(&args, "pattern").map(regex::Regex::new).transpose().map_err(|e| ToolError::Invalid(format!("pattern: {e}")))?;
                    let (mut command, line) = match cmd {
                        Value::String(s) => {
                            let mut c = tokio::process::Command::new("sh");
                            c.arg("-c").arg(s); // plain shell, as `exec`: the server holds the login env
                            (c, s.clone())
                        }
                        Value::Array(a) if !a.is_empty() => {
                            let words = argv(a)?;
                            let mut c = tokio::process::Command::new(words[0]);
                            c.args(&words[1..]);
                            (c, words.join(" "))
                        }
                        _ => return Err(ToolError::Invalid("cmd must be a string or an argv array".into())),
                    };
                    command.current_dir(&self.root);
                    command.process_group(0);
                    command.kill_on_drop(true);
                    let id = self.watches.watch_cmd(self.procs.clone(), command, line, pattern, once).map_err(ToolError::Failed)?;
                    Ok(json!({ "id": id }))
                }
                "watch_poll" => {
                    let id = str_arg(&args, "id")?;
                    let w = self.watches.get(id).ok_or_else(|| ToolError::Failed(format!("no watch {id}")))?;
                    let g = w.lock().unwrap_or_else(|q| q.into_inner());
                    let (events, next, dropped) = g.read_since(opt_u64(&args, "since").unwrap_or(0));
                    Ok(json!({ "events": events, "next": next, "dropped": dropped, "state": g.state }))
                }
                "watch_stop" => {
                    let id = str_arg(&args, "id")?;
                    let st = self.watches.stop(id).await.map_err(ToolError::Failed)?;
                    Ok(json!({ "state": st }))
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
    use crate::watches::Watches;

    /// A poll with the `next` the previous poll answered must return only what arrived AFTER it.
    /// Every fire re-sent the whole ring, so a watch on a build's output repeated the same
    /// `#N DONE` lines on every notification (workspace session, 2026-09-18). A cursor sent as a
    /// string is the same question and must not read as "from the start".
    #[tokio::test]
    async fn polling_a_watch_with_its_cursor_returns_only_what_is_new() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let root = home.join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let w = WatchTools { root, home, procs: Arc::new(Procs::default()), watches: Arc::new(Watches::default()) };
        let v = w.call("watch", json!({ "cmd": "echo one; echo two; sleep 30" })).await.unwrap();
        let id = v["id"].as_str().unwrap().to_string();

        let mut first = json!({});
        for _ in 0..100 {
            first = w.call("watch_poll", json!({ "id": id })).await.unwrap();
            if first["events"].as_array().is_some_and(|e| e.len() >= 2) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let events = first["events"].as_array().unwrap();
        assert!(events.len() >= 2, "{first}");
        let next = first["next"].as_u64().unwrap();

        let again = w.call("watch_poll", json!({ "id": id, "since": next })).await.unwrap();
        assert_eq!(again["events"].as_array().unwrap().len(), 0, "a cursor poll must not replay: {again}");
        let quoted = w.call("watch_poll", json!({ "id": id, "since": next.to_string() })).await.unwrap();
        assert_eq!(quoted["events"].as_array().unwrap().len(), 0, "a quoted cursor is the same cursor: {quoted}");

        w.call("watch_stop", json!({ "id": id })).await.unwrap();
    }

    #[tokio::test]
    async fn a_watch_refuses_a_non_string_argv_entry_and_a_non_string_path() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let root = home.join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let w = WatchTools { root, home, procs: Arc::new(Procs::default()), watches: Arc::new(Watches::default()) };
        let e = w.call("watch", json!({ "cmd": ["echo", 7] })).await.unwrap_err();
        assert!(matches!(e, ToolError::Invalid(_)), "{e:?}");
        let e = w.call("watch", json!({ "paths": [7] })).await.unwrap_err();
        assert!(matches!(e, ToolError::Invalid(_)), "{e:?}");
    }
}
