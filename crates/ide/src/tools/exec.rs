//! `exec` and the `process_*` tools. A job is `exec` without `detach`: it runs to completion under
//! a timeout and answers what it printed. `detach: true` hands the same command to `Procs`.
use super::{argv, opt_bool, opt_str, opt_u64, str_arg, Tool, ToolError, ToolSet};
use crate::paths::confine;
use crate::procs::{Procs, Ring, State};
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
            let words = argv(a)?;
            let mut c = Command::new(words[0]);
            c.args(&words[1..]);
            (c, words.join(" "))
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

/// A stream read into the bounded ring: a command that prints gigabytes costs `JOB_CAP` of memory,
/// not all of it. `read_to_end` here was the whole pod's memory in one `yes` (2026-09-12).
async fn drain<R: tokio::io::AsyncRead + Unpin>(r: Option<R>) -> (String, bool) {
    let mut ring = Ring::new(JOB_CAP);
    if let Some(mut r) = r {
        let mut buf = [0u8; 64 << 10];
        loop {
            match tokio::io::AsyncReadExt::read(&mut r, &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => ring.push(&buf[..n]),
            }
        }
    }
    // The ring keeps the tail; what it dropped is exactly what `truncated` reports.
    let (bytes, _, dropped) = ring.read_since(0);
    (String::from_utf8_lossy(&bytes).into_owned(), dropped > 0)
}

async fn job(mut cmd: Command, timeout_ms: u64) -> Result<Value, ToolError> {
    let started = std::time::Instant::now();
    let mut child = cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).stdin(std::process::Stdio::null()).spawn().map_err(|e| ToolError::Failed(format!("spawn: {e}")))?;
    let pid = child.id();
    let out = child.stdout.take();
    let err = child.stderr.take();
    let read = async {
        let child = &mut child;
        let (o, e) = tokio::join!(drain(out), drain(err));
        let status = child.wait().await;
        (o, e, status)
    };
    match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), read).await {
        Ok((o, e, status)) => {
            let code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
            let ((stdout, t1), (stderr, t2)) = (o, e);
            Ok(json!({ "exit_code": code, "stdout": stdout, "stderr": stderr, "truncated": t1 || t2, "timed_out": false, "ms": started.elapsed().as_millis() as u64 }))
        }
        Err(_) => {
            if let Some(pid) = pid {
                // The caller asked for a deadline, so the answer comes AT the deadline: the TERM, the
                // five-second grace and the KILL run behind it (measured: a 1 s timeout answered in 6 s).
                tokio::spawn(async move {
                    // SAFETY: the group this call created.
                    unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                    let _ = child.wait().await;
                });
            }
            Ok(json!({ "exit_code": -1, "stdout": "", "stderr": format!("timed out after {timeout_ms} ms"), "truncated": false, "timed_out": true, "ms": started.elapsed().as_millis() as u64 }))
        }
    }
}

/// The token savers on a job's answer: `head`/`tail` keep only N lines of each stream (a
/// `cargo test` answers its last twenty lines, not four megabytes); `quiet` drops both streams
/// on success and keeps only stderr's last twenty lines on failure.
pub fn trim_output(mut v: Value, args: &Value) -> Value {
    fn keep(s: &str, head: Option<usize>, tail: Option<usize>) -> (String, bool) {
        let lines: Vec<&str> = s.lines().collect();
        let n = lines.len();
        let kept: Vec<&str> = match (head, tail) {
            (Some(h), _) if h < n => lines[..h].to_vec(),
            (_, Some(t)) if t < n => lines[n - t..].to_vec(),
            _ => return (s.to_string(), false),
        };
        (kept.join("\n"), true)
    }
    let head = opt_u64(args, "head").map(|n| n as usize);
    let tail = opt_u64(args, "tail").map(|n| n as usize);
    let quiet = opt_bool(args, "quiet");
    let ok = v.get("exit_code").and_then(Value::as_i64) == Some(0);
    let mut trimmed = false;
    for key in ["stdout", "stderr"] {
        let Some(s) = v.get(key).and_then(Value::as_str).map(str::to_string) else { continue };
        let (out, t) = if quiet {
            if key == "stderr" && !ok { keep(&s, None, Some(20)) } else { (String::new(), !s.is_empty()) }
        } else {
            keep(&s, head, tail)
        };
        trimmed |= t;
        v[key] = json!(out);
    }
    if trimmed {
        v["trimmed"] = json!(true);
    }
    v
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

impl ToolSet for Exec {
    fn tools(&self) -> Vec<Tool> {
        vec![
            Tool { name: "exec", description: "Run a command in the workspace as the workspace user. cmd is a shell string or an argv array; cwd defaults to the workspace dir. Without detach it is a job: waits (timeout_ms, default 120000, max 600000) and answers exit_code, stdout, stderr; at the timeout it answers timed_out:true at once and the process group is killed behind the answer. head or tail keep only N lines of each stream; quiet answers the exit code alone (stderr's last 20 lines on failure). With detach:true it answers {id} and becomes a process for process_output / process_kill / GET /stream/process/{id}. pty is not supported in this version.", schema: obj(json!({ "cmd": {}, "cwd": {"type":"string"}, "env": {"type":"object"}, "timeout_ms": {"type":"integer"}, "head": {"type":"integer"}, "tail": {"type":"integer"}, "quiet": {"type":"boolean"}, "detach": {"type":"boolean"}, "pty": {"type":"boolean"} }), &["cmd"]) },
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
                    let r = job(cmd, timeout).await.map(|v| trim_output(v, &args));
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

    #[test]
    fn trim_keeps_head_or_tail_and_quiet_keeps_only_a_failing_stderr() {
        let out = json!({ "exit_code": 0, "stdout": "a\nb\nc\nd", "stderr": "" });
        let v = trim_output(out.clone(), &json!({ "tail": 2 }));
        assert_eq!((v["stdout"].as_str().unwrap(), v["trimmed"].as_bool()), ("c\nd", Some(true)));
        let v = trim_output(out.clone(), &json!({ "head": 1 }));
        assert_eq!(v["stdout"], "a");
        let v = trim_output(out.clone(), &json!({ "tail": 10 }));
        assert_eq!((v["stdout"].as_str().unwrap(), v.get("trimmed")), ("a\nb\nc\nd", None));
        let v = trim_output(out, &json!({ "quiet": true }));
        assert_eq!(v["stdout"], "");
        let v = trim_output(json!({ "exit_code": 1, "stdout": "noise", "stderr": "e1\ne2" }), &json!({ "quiet": true }));
        assert_eq!((v["stdout"].as_str().unwrap(), v["stderr"].as_str().unwrap()), ("", "e1\ne2"));
    }

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
    async fn a_job_that_prints_more_than_the_ring_holds_keeps_the_tail_and_says_truncated() {
        let (_t, x) = exec_set();
        let v = x.call("exec", json!({ "cmd": "yes 0123456789abcdef | head -c 5000000; echo END" })).await.unwrap();
        assert_eq!(v["truncated"], true);
        let out = v["stdout"].as_str().unwrap();
        assert!(out.len() <= JOB_CAP, "{}", out.len());
        assert!(out.ends_with("END\n"), "the tail is what is kept");
    }

    #[tokio::test]
    async fn an_argv_entry_that_is_not_a_string_is_refused_rather_than_dropped() {
        let (_t, x) = exec_set();
        let e = x.call("exec", json!({ "cmd": ["echo", 1, "b"] })).await.unwrap_err();
        assert!(matches!(e, ToolError::Invalid(_)), "{e:?}");
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
