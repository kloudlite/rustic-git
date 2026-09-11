//! `exec` and the `process_*` tools. A job is `exec` without `detach`: it runs to completion under
//! a timeout and answers what it printed. `detach: true` hands the same command to `Procs`.
use super::{opt_bool, opt_str, opt_u64, str_arg, Tool, ToolError, ToolSet};
use crate::paths::confine;
use crate::procs::{Procs, State};
use futures::future::BoxFuture;
use futures::FutureExt;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;

pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const MAX_TIMEOUT_MS: u64 = 600_000;
/// A job's captured output is capped the same as a process ring; `truncated` says so.
pub const JOB_CAP: usize = 4 << 20;

pub struct Exec {
    pub root: PathBuf,
    pub home: PathBuf,
    pub procs: Arc<Procs>,
    /// Called after every finished job: a command may have moved the tree.
    pub after_change: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// Build the command: argv or a shell string, cwd confined, env merged, its own process group so
/// a kill reaches the children, inheriting the pod's login environment.
fn command(root: &std::path::Path, home: &std::path::Path, args: &Value) -> Result<(Command, String), ToolError> {
    let (mut cmd, line) = match args.get("cmd") {
        Some(Value::String(s)) => {
            // Not a login shell: the server was started from the pod's prelude and already carries the
            // login environment (`login_env`, the nix profile on PATH). `/etc/profile` would reset PATH
            // to the system directories — `git: command not found` on build fb3673f1.
            let mut c = Command::new("sh");
            c.arg("-c").arg(s);
            (c, s.clone())
        }
        Some(Value::Array(a)) if !a.is_empty() => {
            let argv: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
            let mut c = Command::new(argv[0]);
            c.args(&argv[1..]);
            (c, argv.join(" "))
        }
        _ => return Err(ToolError::Invalid("`cmd` (string, or argv array) is required".into())),
    };
    let cwd = confine(root, home, opt_str(args, "cwd").unwrap_or("."))?;
    cmd.current_dir(&cwd);
    if let Some(env) = args.get("env").and_then(Value::as_object) {
        for (k, v) in env {
            if let Some(v) = v.as_str() {
                cmd.env(k, v);
            }
        }
    }
    cmd.process_group(0);
    cmd.kill_on_drop(true);
    Ok((cmd, line))
}

fn cap(mut bytes: Vec<u8>) -> (String, bool) {
    let truncated = bytes.len() > JOB_CAP;
    if truncated {
        bytes.drain(..bytes.len() - JOB_CAP);
    }
    (String::from_utf8_lossy(&bytes).into_owned(), truncated)
}

async fn job(mut cmd: Command, timeout_ms: u64) -> Result<Value, ToolError> {
    let started = std::time::Instant::now();
    let mut child = cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).stdin(std::process::Stdio::null()).spawn().map_err(|e| ToolError::Failed(format!("spawn: {e}")))?;
    let pid = child.id();
    let out = child.stdout.take();
    let err = child.stderr.take();
    let read = async {
        let (o, e) = tokio::join!(
            async { match out { Some(mut r) => { let mut b = Vec::new(); let _ = tokio::io::AsyncReadExt::read_to_end(&mut r, &mut b).await; b } None => Vec::new() } },
            async { match err { Some(mut r) => { let mut b = Vec::new(); let _ = tokio::io::AsyncReadExt::read_to_end(&mut r, &mut b).await; b } None => Vec::new() } }
        );
        let status = child.wait().await;
        (o, e, status)
    };
    match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), read).await {
        Ok((o, e, status)) => {
            let code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
            let (stdout, t1) = cap(o);
            let (stderr, t2) = cap(e);
            Ok(json!({ "exit_code": code, "stdout": stdout, "stderr": stderr, "truncated": t1 || t2, "timed_out": false, "ms": started.elapsed().as_millis() as u64 }))
        }
        Err(_) => {
            if let Some(pid) = pid {
                // SAFETY: the group this call created.
                unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            }
            Ok(json!({ "exit_code": -1, "stdout": "", "stderr": format!("timed out after {timeout_ms} ms"), "truncated": false, "timed_out": true, "ms": started.elapsed().as_millis() as u64 }))
        }
    }
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

impl ToolSet for Exec {
    fn tools(&self) -> Vec<Tool> {
        vec![
            Tool { name: "exec", description: "Run a command in the workspace as the workspace user. cmd is a shell string or an argv array; cwd defaults to the workspace dir. Without detach it is a job: waits (timeout_ms, default 120000, max 600000) and answers exit_code, stdout, stderr. With detach:true it answers {id} and becomes a process for process_output / process_kill / GET /stream/process/{id}. pty is not supported in this version.", schema: obj(json!({ "cmd": {}, "cwd": {"type":"string"}, "env": {"type":"object"}, "timeout_ms": {"type":"integer"}, "detach": {"type":"boolean"}, "pty": {"type":"boolean"} }), &["cmd"]) },
            Tool { name: "process_list", description: "Every detached process: id, cmd, started_at, state (running|exited), exit_code.", schema: obj(json!({}), &[]) },
            Tool { name: "process_output", description: "A process's output since a byte offset (0 = from the start; the ring keeps the last 4 MiB). Answers stdout, stderr, next (offset), dropped, state, exit_code.", schema: obj(json!({ "id": {"type":"string"}, "since": {"type":"integer"} }), &["id"]) },
            Tool { name: "process_write", description: "Write to a process's stdin.", schema: obj(json!({ "id": {"type":"string"}, "data": {"type":"string"} }), &["id","data"]) },
            Tool { name: "process_kill", description: "Stop a process: TERM (default), then KILL after 5 s; or KILL.", schema: obj(json!({ "id": {"type":"string"}, "signal": {"type":"string","enum":["TERM","KILL"]} }), &["id"]) },
        ]
    }

    fn call<'a>(&'a self, name: &'a str, args: Value) -> BoxFuture<'a, Result<Value, ToolError>> {
        async move {
            match name {
                "exec" => {
                    if opt_bool(&args, "pty") {
                        return Err(ToolError::Invalid("pty: unsupported in this version".into()));
                    }
                    let (cmd, line) = command(&self.root, &self.home, &args)?;
                    if opt_bool(&args, "detach") {
                        let id = self.procs.spawn(cmd, line).map_err(ToolError::Failed)?;
                        return Ok(json!({ "id": id }));
                    }
                    let timeout = opt_u64(&args, "timeout_ms").unwrap_or(DEFAULT_TIMEOUT_MS).min(MAX_TIMEOUT_MS);
                    let r = job(cmd, timeout).await;
                    if let Some(f) = &self.after_change {
                        f();
                    }
                    r
                }
                "process_list" => Ok(json!({ "processes": self.procs.list().iter().map(|p| {
                    let g = p.lock().unwrap_or_else(|q| q.into_inner());
                    json!({ "id": g.id, "cmd": g.cmd, "started_at": g.started_at, "state": g.state, "exit_code": g.exit_code })
                }).collect::<Vec<_>>() })),
                "process_output" => {
                    let id = str_arg(&args, "id")?;
                    let p = self.procs.get(id).ok_or_else(|| ToolError::Failed(format!("no process {id}")))?;
                    let since = opt_u64(&args, "since").unwrap_or(0);
                    let g = p.lock().unwrap_or_else(|q| q.into_inner());
                    let (o, next, dropped) = g.out.read_since(since);
                    let (e, _, _) = g.err.read_since(0);
                    Ok(json!({ "stdout": String::from_utf8_lossy(&o), "stderr": String::from_utf8_lossy(&e), "next": next, "dropped": dropped, "state": g.state, "exit_code": g.exit_code }))
                }
                "process_write" => {
                    let id = str_arg(&args, "id")?;
                    let data = str_arg(&args, "data")?;
                    let n = self.procs.write_stdin(id, data.as_bytes()).await.map_err(ToolError::Failed)?;
                    Ok(json!({ "bytes": n }))
                }
                "process_kill" => {
                    let id = str_arg(&args, "id")?;
                    let st: State = self.procs.kill(id, opt_str(&args, "signal").unwrap_or("TERM")).await.map_err(ToolError::Failed)?;
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

    fn exec_set() -> (tempfile::TempDir, Exec) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let root = home.join("ws");
        std::fs::create_dir_all(&root).unwrap();
        (tmp, Exec { root, home, procs: Arc::new(Procs::default()), after_change: None })
    }

    #[tokio::test]
    async fn a_job_answers_its_exit_code_and_output_in_the_workspace_dir() {
        let (_t, x) = exec_set();
        let v = x.call("exec", json!({ "cmd": "pwd; echo err >&2; exit 3" })).await.unwrap();
        assert_eq!(v["exit_code"], 3);
        assert_eq!(v["stdout"].as_str().unwrap().trim(), x.root.to_string_lossy());
        assert_eq!(v["stderr"].as_str().unwrap().trim(), "err");
        let v = x.call("exec", json!({ "cmd": ["sh", "-c", "echo $FOO"], "env": { "FOO": "bar" } })).await.unwrap();
        assert_eq!(v["stdout"].as_str().unwrap().trim(), "bar");
    }

    #[tokio::test]
    async fn a_job_that_outlives_its_timeout_is_killed_and_says_so() {
        let (_t, x) = exec_set();
        let v = x.call("exec", json!({ "cmd": "sleep 5", "timeout_ms": 200 })).await.unwrap();
        assert_eq!(v["timed_out"], true);
    }

    #[tokio::test]
    async fn a_detached_process_is_listed_polled_and_killed() {
        let (_t, x) = exec_set();
        let v = x.call("exec", json!({ "cmd": "for i in 1 2 3; do echo $i; sleep 0.05; done; sleep 30", "detach": true })).await.unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let v = x.call("process_output", json!({ "id": id })).await.unwrap();
        assert_eq!(v["stdout"], "1\n2\n3\n");
        assert_eq!(v["state"], "running");
        let next = v["next"].as_u64().unwrap();
        let v = x.call("process_output", json!({ "id": id, "since": next })).await.unwrap();
        assert_eq!(v["stdout"], "");
        let v = x.call("process_kill", json!({ "id": id })).await.unwrap();
        assert_eq!(v["state"], "exited");
        let v = x.call("process_list", json!({})).await.unwrap();
        assert_eq!(v["processes"][0]["state"], "exited");
        assert!(x.call("exec", json!({ "cmd": "true", "pty": true })).await.is_err());
        assert!(x.call("exec", json!({ "cmd": "true", "cwd": "/etc" })).await.is_err(), "cwd is confined");
    }
}
