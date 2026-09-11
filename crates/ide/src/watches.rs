//! Watches: a file-system watch over paths, or a command whose output lines are filtered by a
//! regex. Each keeps a ring of its last events and broadcasts them for `/stream/watch/{id}`.
use crate::procs::{Frame, Procs};
use notify::{RecursiveMode, Watcher};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

pub const MAX_WATCHES: usize = 32;
pub const RING_EVENTS: usize = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Running,
    Stopped,
}

pub struct Watch {
    pub id: String,
    pub what: String,
    pub state: State,
    /// Absolute index of `events[0]`.
    start: u64,
    events: VecDeque<Value>,
    pub tx: broadcast::Sender<Value>,
    once: bool,
    /// What stopping means for this kind: drop the notify watcher, or kill the process.
    stop: Stop,
}

enum Stop {
    /// Held only so the watcher lives as long as the watch: dropping it is what stops notify.
    Fs(#[allow(dead_code)] Option<notify::RecommendedWatcher>),
    Cmd(String, Arc<Procs>),
}

impl Watch {
    pub fn push(&mut self, ev: Value) {
        self.events.push_back(ev.clone());
        if self.events.len() > RING_EVENTS {
            self.events.pop_front();
            self.start += 1;
        }
        let _ = self.tx.send(ev);
        if self.once {
            self.state = State::Stopped;
        }
    }
    pub fn read_since(&self, since: u64) -> (Vec<Value>, u64, u64) {
        let dropped = self.start.saturating_sub(since);
        let from = (since.max(self.start) - self.start) as usize;
        (self.events.iter().skip(from).cloned().collect(), self.start + self.events.len() as u64, dropped)
    }
}

#[derive(Default)]
pub struct Watches {
    inner: Mutex<HashMap<String, Arc<Mutex<Watch>>>>,
}

impl Watches {
    fn insert(&self, what: String, once: bool, stop: Stop) -> Result<(String, Arc<Mutex<Watch>>), String> {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if map.len() >= MAX_WATCHES {
            return Err(format!("{MAX_WATCHES} watches already; stop one first"));
        }
        let id = format!("w-{:x}", crate::procs::rand_id());
        let (tx, _) = broadcast::channel(1024);
        let w = Arc::new(Mutex::new(Watch { id: id.clone(), what, state: State::Running, start: 0, events: VecDeque::new(), tx, once, stop }));
        map.insert(id.clone(), w.clone());
        Ok((id, w))
    }

    /// File-system events under `paths`, recursive, from the tokio runtime `rt` (notify calls
    /// back from its own thread; the handle lets the callback push into the watch).
    pub fn watch_paths(&self, paths: Vec<PathBuf>, once: bool) -> Result<String, String> {
        let what = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join(", ");
        let (id, w) = self.insert(what, once, Stop::Fs(None))?;
        let sink = w.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            let mut g = sink.lock().unwrap_or_else(|p| p.into_inner());
            if g.state == State::Stopped {
                return;
            }
            // A read is not a change: `access` fired on every `cat` and doubled the events an agent saw.
            if matches!(ev.kind, notify::EventKind::Access(_)) {
                return;
            }
            let kind = format!("{:?}", ev.kind).to_lowercase();
            let kind = kind.split('(').next().unwrap_or("other").to_string();
            for p in ev.paths {
                g.push(json!({ "path": p, "kind": kind }));
            }
        })
        .map_err(|e| format!("watcher: {e}"))?;
        for p in &paths {
            watcher.watch(p, RecursiveMode::Recursive).map_err(|e| format!("watch {}: {e}", p.display()))?;
        }
        w.lock().unwrap_or_else(|p| p.into_inner()).stop = Stop::Fs(Some(watcher));
        Ok(id)
    }

    /// A command's output lines, filtered by `pattern` (every line when absent), as events.
    pub fn watch_cmd(&self, procs: Arc<Procs>, cmd: tokio::process::Command, line: String, pattern: Option<regex::Regex>, once: bool) -> Result<String, String> {
        let pid = procs.spawn(cmd, line.clone())?;
        let (id, w) = self.insert(line, once, Stop::Cmd(pid.clone(), procs.clone()))?;
        let p = procs.get(&pid).ok_or("the process vanished")?;
        let mut rx = p.lock().unwrap_or_else(|q| q.into_inner()).tx.subscribe();
        let sink = w.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(Frame::Stdout(b)) | Ok(Frame::Stderr(b)) => {
                        let text = String::from_utf8_lossy(&b);
                        for l in text.lines() {
                            if !pattern.as_ref().is_none_or(|re| re.is_match(l)) {
                                continue;
                            }
                            // The guard ends before any await: what to kill is copied out first.
                            let kill: Option<Option<(String, Arc<Procs>)>> = {
                                let mut g = sink.lock().unwrap_or_else(|q| q.into_inner());
                                if g.state == State::Stopped {
                                    Some(None)
                                } else {
                                    g.push(json!({ "line": l }));
                                    if g.state == State::Stopped {
                                        Some(match &g.stop {
                                            Stop::Cmd(pid, procs) => Some((pid.clone(), procs.clone())),
                                            Stop::Fs(_) => None,
                                        })
                                    } else {
                                        None
                                    }
                                }
                            };
                            match kill {
                                None => {}
                                Some(None) => return,
                                Some(Some((pid, procs))) => {
                                    let _ = procs.kill(&pid, "TERM").await;
                                    return;
                                }
                            }
                        }
                    }
                    Ok(Frame::Exit(code)) => {
                        let mut g = sink.lock().unwrap_or_else(|q| q.into_inner());
                        g.push(json!({ "exit": code }));
                        g.state = State::Stopped;
                        return;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                }
            }
        });
        Ok(id)
    }

    pub fn get(&self, id: &str) -> Option<Arc<Mutex<Watch>>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).get(id).cloned()
    }

    pub async fn stop(&self, id: &str) -> Result<State, String> {
        let w = self.inner.lock().unwrap_or_else(|p| p.into_inner()).remove(id).ok_or_else(|| format!("no watch {id}"))?;
        let stop = {
            let mut g = w.lock().unwrap_or_else(|p| p.into_inner());
            g.state = State::Stopped;
            std::mem::replace(&mut g.stop, Stop::Fs(None))
        };
        if let Stop::Cmd(pid, procs) = stop {
            let _ = procs.kill(&pid, "TERM").await;
        }
        Ok(State::Stopped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_file_created_under_a_watched_dir_is_an_event() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Watches::default();
        let id = ws.watch_paths(vec![tmp.path().to_path_buf()], false).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        std::fs::write(tmp.path().join("new.txt"), "x").unwrap();
        let w = ws.get(&id).unwrap();
        for _ in 0..50 {
            if !w.lock().unwrap().read_since(0).0.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let (events, next, _) = w.lock().unwrap().read_since(0);
        assert!(events.iter().any(|e| e["path"].as_str().unwrap().ends_with("new.txt")), "{events:?}");
        assert!(next >= 1);
        assert_eq!(ws.stop(&id).await.unwrap(), State::Stopped);
    }

    #[tokio::test]
    async fn a_command_watch_with_once_stops_at_the_first_match() {
        let ws = Watches::default();
        let procs = Arc::new(Procs::default());
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg("echo a; echo ready; sleep 30");
        c.process_group(0);
        let id = ws.watch_cmd(procs.clone(), c, "sh".into(), Some(regex::Regex::new("ready").unwrap()), true).unwrap();
        let w = ws.get(&id).unwrap();
        for _ in 0..100 {
            if w.lock().unwrap().state == State::Stopped {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let g = w.lock().unwrap();
        assert_eq!(g.state, State::Stopped);
        let (events, _, _) = g.read_since(0);
        assert_eq!(events, vec![json!({ "line": "ready" })]);
    }
}
