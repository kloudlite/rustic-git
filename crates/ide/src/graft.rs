//! Graft, kept fresh by the server rather than by an agent's hooks.
//!
//! Two halves. The QUERY half is graft's own MCP server, `graft mcp`, run as a child on stdio
//! (newline-delimited JSON-RPC) and proxied: this crate never re-implements a graph query. The
//! FRESHNESS half is the reason this module exists: there is no Claude Code in the workspace and
//! so no hook runner, so the triggers graft's `init` would wire into an agent are wired into the
//! server's own tool calls — `write`, `edit` and a finished `exec` call `refresh_soon`, and a
//! file watcher over the tree catches every other way the tree can move (ssh, git, a detached
//! process). A refresh is `graft build`, which replays unchanged files from its cache; debounced
//! to one run per two seconds and serialised, so a burst of edits is one pass.
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Notify};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GraphState {
    /// `{root}/graft` does not exist and no build has been asked for.
    Absent,
    Building,
    Drifted,
    Ready,
    /// `graft` is not on PATH, or the child would not start.
    Unavailable,
}

pub const DEBOUNCE_MS: u64 = 2_000;
/// Directories whose changes never mean the graph moved.
pub const IGNORED_DIRS: &[&str] = &["graft", ".git", ".cache", "node_modules", "target", ".direnv"];

struct Child_ {
    stdin: ChildStdin,
    _child: Child,
}

pub struct Graft {
    pub root: PathBuf,
    pub graft_dir: Option<PathBuf>,
    state: Mutex<GraphState>,
    child: tokio::sync::Mutex<Option<Child_>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: AtomicU64,
    /// Set by `refresh_soon`; the refresh loop wakes, waits the debounce, builds once.
    dirty: Arc<Notify>,
    building: tokio::sync::Mutex<()>,
}

impl Graft {
    pub fn new(root: PathBuf, graft_dir: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Graft {
            root,
            graft_dir,
            state: Mutex::new(GraphState::Absent),
            child: tokio::sync::Mutex::new(None),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            dirty: Arc::new(Notify::new()),
            building: tokio::sync::Mutex::new(()),
        })
    }

    pub fn state(&self) -> GraphState {
        *self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn set(&self, s: GraphState) {
        *self.state.lock().unwrap_or_else(|p| p.into_inner()) = s;
    }

    fn graph_dir(&self) -> PathBuf {
        self.graft_dir.clone().unwrap_or_else(|| self.root.join("graft"))
    }

    fn base_command(&self) -> Command {
        let mut c = Command::new("graft");
        if let Some(d) = &self.graft_dir {
            c.arg("--dir").arg(d);
        }
        c.env("DO_NOT_TRACK", "1").env("GRAFT_NO_STATUSLINE", "1").current_dir(&self.root);
        c
    }

    /// On start: build a missing graph, or check an existing one and refresh it if drifted; then
    /// run the debounced refresh loop and the tree watcher for the life of the server.
    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            if which("graft").is_none() {
                me.set(GraphState::Unavailable);
                tracing::warn!("ide.graft.unavailable");
                return;
            }
            if !me.graph_dir().is_dir() {
                me.build().await;
            } else {
                me.check_then_refresh().await;
            }
            me.watch_tree();
            loop {
                me.dirty.notified().await;
                tokio::time::sleep(std::time::Duration::from_millis(DEBOUNCE_MS)).await;
                me.build().await;
            }
        });
    }

    /// Mark the graph stale; the loop refreshes it after the debounce.
    pub fn refresh_soon(&self) {
        if matches!(self.state(), GraphState::Ready | GraphState::Drifted) {
            self.dirty.notify_one();
        }
    }

    async fn check_then_refresh(&self) {
        let out = self.base_command().arg("check").arg("--json").output().await;
        let ok = out.as_ref().ok().and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok()).and_then(|v| v["graph"]["ok"].as_bool()).unwrap_or(false);
        if ok {
            self.set(GraphState::Ready);
        } else {
            self.set(GraphState::Drifted);
            self.build().await;
        }
    }

    /// One `graft build`, serialised. Ready on success; Drifted on failure so the next trigger
    /// tries again rather than the server believing a broken graph is current.
    pub async fn build(&self) {
        let _one = self.building.lock().await;
        self.set(GraphState::Building);
        let started = std::time::Instant::now();
        let out = self.base_command().arg("build").arg(&self.root).output().await;
        match out {
            Ok(o) if o.status.success() => {
                self.set(GraphState::Ready);
                tracing::info!(ms = started.elapsed().as_millis() as u64, "ide.graft.built");
            }
            Ok(o) => {
                self.set(GraphState::Drifted);
                tracing::warn!(code = o.status.code(), stderr = %String::from_utf8_lossy(&o.stderr).chars().take(400).collect::<String>(), "ide.graft.build.failed");
            }
            Err(e) => {
                self.set(GraphState::Unavailable);
                tracing::warn!(error = %e, "ide.graft.build.failed");
            }
        }
    }

    fn watch_tree(self: &Arc<Self>) {
        use notify::Watcher;
        let me = self.clone();
        let root = self.root.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            let relevant = ev.paths.iter().any(|p| {
                let rel = p.strip_prefix(&root).unwrap_or(p);
                !rel.components().next().is_some_and(|c| IGNORED_DIRS.contains(&c.as_os_str().to_string_lossy().as_ref()))
            });
            if relevant {
                me.refresh_soon();
            }
        });
        match watcher {
            Ok(mut w) => {
                if let Err(e) = w.watch(&self.root, notify::RecursiveMode::Recursive) {
                    tracing::warn!(error = %e, "ide.graft.watch.failed");
                    return;
                }
                // Leaked on purpose: the watcher lives as long as the server.
                std::mem::forget(w);
            }
            Err(e) => tracing::warn!(error = %e, "ide.graft.watch.failed"),
        }
    }

    async fn ensure_child(&self) -> Result<(), String> {
        let mut slot = self.child.lock().await;
        if slot.is_some() {
            return Ok(());
        }
        let mut child = self.base_command().arg("mcp").arg(&self.root).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn().map_err(|e| format!("graft mcp: {e}"))?;
        let stdout = child.stdout.take().ok_or("graft mcp: no stdout")?;
        let stdin = child.stdin.take().ok_or("graft mcp: no stdin")?;
        let pending = self.pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                let Some(id) = v.get("id").and_then(Value::as_u64) else { continue };
                if let Some(tx) = pending.lock().unwrap_or_else(|p| p.into_inner()).remove(&id) {
                    let _ = tx.send(v);
                }
            }
            // The child ended: every waiter gets an error and the next call respawns it.
            pending.lock().unwrap_or_else(|p| p.into_inner()).clear();
        });
        let mut c = Child_ { stdin, _child: child };
        let init = json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": { "name": "kl-ide", "version": env!("CARGO_PKG_VERSION") } } });
        c.stdin.write_all(format!("{init}\n").as_bytes()).await.map_err(|e| format!("graft mcp: {e}"))?;
        let notified = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        c.stdin.write_all(format!("{notified}\n").as_bytes()).await.map_err(|e| format!("graft mcp: {e}"))?;
        *slot = Some(c);
        Ok(())
    }

    /// `tools/call` against the child; the child's `result` (or its error message) comes back.
    pub async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        self.ensure_child().await?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).insert(id, tx);
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": args } });
        {
            let mut slot = self.child.lock().await;
            let Some(c) = slot.as_mut() else { return Err("graft mcp is not running".into()) };
            if let Err(e) = c.stdin.write_all(format!("{req}\n").as_bytes()).await {
                *slot = None;
                return Err(format!("graft mcp: {e}"));
            }
        }
        let resp = tokio::time::timeout(std::time::Duration::from_secs(120), rx).await.map_err(|_| "graft mcp: no answer in 120 s".to_string())?.map_err(|_| "graft mcp exited".to_string())?;
        if let Some(e) = resp.get("error") {
            return Err(e["message"].as_str().unwrap_or("graft error").to_string());
        }
        let result = resp.get("result").cloned().unwrap_or(Value::Null);
        // Graft answers text content; hand the caller the text itself when that is all there is.
        if let Some(text) = result.get("content").and_then(Value::as_array).filter(|c| c.len() == 1).and_then(|c| c[0].get("text")).and_then(Value::as_str) {
            let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
            return if is_error { Err(text.to_string()) } else { Ok(json!({ "text": text })) };
        }
        Ok(result)
    }

    /// `graft blast --format json`, a job.
    pub async fn blast(&self, base: Option<&str>, depth: Option<&str>) -> Result<Value, String> {
        let mut c = self.base_command();
        c.arg("blast").arg("--format").arg("json");
        if let Some(b) = base {
            c.arg("--base").arg(b);
        }
        if let Some(d) = depth {
            c.arg("--depth").arg(d);
        }
        let out = c.output().await.map_err(|e| format!("graft blast: {e}"))?;
        if !out.status.success() {
            return Err(format!("graft blast exited {:?}: {}", out.status.code(), String::from_utf8_lossy(&out.stderr).chars().take(400).collect::<String>()));
        }
        serde_json::from_slice(&out.stdout).map_err(|e| format!("graft blast: unreadable json: {e}"))
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(bin)).find(|c| c.is_file()))
}

pub fn is_ignored_dir(rel: &Path) -> bool {
    rel.components().next().is_some_and(|c| IGNORED_DIRS.contains(&c.as_os_str().to_string_lossy().as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tree_watcher_ignores_the_graph_and_the_caches() {
        for d in ["graft/x.md", ".git/index", ".cache/cargo-target/a", "node_modules/x", "target/debug/x"] {
            assert!(is_ignored_dir(Path::new(d)), "{d}");
        }
        assert!(!is_ignored_dir(Path::new("src/main.rs")));
    }

    /// Needs `graft` on PATH and a graph at the repo root: runs in the dev pod with
    /// `cargo test -p kloudlite-ide -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn graft_mcp_answers_a_repo_map_through_the_proxy() {
        let root = std::env::var("KL_IDE_TEST_ROOT").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/work/src"));
        let g = Graft::new(root, None);
        let v = g.call("graft_repo_map", json!({ "max_dirs": 4 })).await.unwrap();
        assert!(v["text"].as_str().unwrap().contains("graft"), "{v}");
        let b = g.blast(None, Some("1")).await.unwrap();
        assert!(b.get("basis").is_some(), "{b}");
    }
}
