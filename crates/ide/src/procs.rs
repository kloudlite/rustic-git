//! Detached processes: the table `exec {detach: true}` registers into, each with a bounded ring
//! of what it printed and a broadcast of frames for the WebSocket stream. Two words, kept apart
//! on purpose: a JOB runs to completion inside one `exec` call and answers its exit code; a
//! PROCESS outlives the call, is read by offset, and ends when it exits or is killed.
use bytes::Bytes;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::broadcast;

pub const RING_BYTES: usize = 4 << 20;
pub const MAX_PROCS: usize = 32;
/// How long an exited process stays listed so a caller can still read its tail.
pub const KEEP_EXITED_SECS: u64 = 600;

/// The last `cap` bytes of a stream, addressed by ABSOLUTE offset so a reader that comes back
/// later asks "everything since byte N" and learns how much it missed.
pub struct Ring {
    buf: VecDeque<u8>,
    cap: usize,
    /// Absolute offset of `buf[0]`.
    start: u64,
}

impl Ring {
    pub fn new(cap: usize) -> Self {
        Ring { buf: VecDeque::with_capacity(cap.min(1 << 16)), cap, start: 0 }
    }
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend(bytes);
        while self.buf.len() > self.cap {
            let over = self.buf.len() - self.cap;
            self.buf.drain(..over);
            self.start += over as u64;
        }
    }
    /// Total bytes ever written.
    pub fn end(&self) -> u64 {
        self.start + self.buf.len() as u64
    }
    /// Bytes from `since` on, the `next` offset, and how many bytes before `since` were already
    /// dropped (0 when the caller is inside the window).
    pub fn read_since(&self, since: u64) -> (Vec<u8>, u64, u64) {
        let dropped = self.start.saturating_sub(since);
        let from = since.max(self.start) - self.start;
        let bytes: Vec<u8> = self.buf.iter().skip(from as usize).copied().collect();
        (bytes, self.end(), dropped)
    }
}

#[derive(Clone, Debug)]
pub enum Frame {
    Stdout(Bytes),
    Stderr(Bytes),
    Exit(i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Running,
    Exited,
}

pub struct Proc {
    pub id: String,
    /// Set when the process died on a bind failure: the sentence `port_conflict` composed, which
    /// a model reads instead of the stack trace it would otherwise have to parse.
    pub failed: Option<String>,
    /// The tree this process was started in. A process id NAMES its tree: listing from one tree
    /// never shows another's, and reaching one by id from the wrong tree is a miss — a subagent
    /// must not be able to kill the main session's dev server, or even see that it is there.
    pub tree: String,
    pub cmd: String,
    pub started_at: String,
    pub state: State,
    pub exit_code: Option<i32>,
    pub exited_at: Option<std::time::Instant>,
    pub out: Ring,
    pub err: Ring,
    /// Behind its own async lock: taking it out for the duration of a write made a CONCURRENT
    /// second write read "takes no more input" instead of waiting its turn (2026-09-12).
    pub stdin: Arc<tokio::sync::Mutex<Option<ChildStdin>>>,
    pub tx: broadcast::Sender<Frame>,
    child: Option<Child>,
}

#[derive(Default)]
pub struct Procs {
    inner: Mutex<HashMap<String, Arc<Mutex<Proc>>>>,
}

fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    // Whole seconds are enough for a listing; no chrono dependency for one field.
    format!("{secs}")
}

impl Procs {
    /// Spawn `command` detached: pipes wired, readers pumping into the rings and the broadcast,
    /// the exit recorded when it comes. `command` must already carry argv, cwd and env.
    pub fn spawn(&self, tree: &str, mut command: Command, cmdline: String) -> Result<String, String> {
        {
            let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            self.reap_locked(&mut map);
            if map.len() >= MAX_PROCS {
                return Err(format!("{MAX_PROCS} processes already; kill one first"));
            }
        }
        command.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        let mut child = command.spawn().map_err(|e| format!("spawn: {e}"))?;
        let id = format!("p-{:x}", rand_u64());
        let (tx, _) = broadcast::channel(1024);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = Arc::new(tokio::sync::Mutex::new(child.stdin.take()));
        let proc_ = Arc::new(Mutex::new(Proc { id: id.clone(), failed: None, tree: tree.to_string(), cmd: cmdline, started_at: now_rfc3339(), state: State::Running, exit_code: None, exited_at: None, out: Ring::new(RING_BYTES), err: Ring::new(RING_BYTES), stdin, tx: tx.clone(), child: None }));
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).insert(id.clone(), proc_.clone());
        let pump = |reader: Option<tokio::process::ChildStdout>, err_reader: Option<tokio::process::ChildStderr>, p: Arc<Mutex<Proc>>| {
            if let Some(r) = reader {
                let p = p.clone();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(r);
                    let mut buf = Vec::new();
                    loop {
                        buf.clear();
                        match lines.read_until(b'\n', &mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let mut g = p.lock().unwrap_or_else(|q| q.into_inner());
                                g.out.push(&buf);
                                let _ = g.tx.send(Frame::Stdout(Bytes::copy_from_slice(&buf)));
                            }
                        }
                    }
                });
            }
            if let Some(r) = err_reader {
                tokio::spawn(async move {
                    let mut lines = BufReader::new(r);
                    let mut buf = Vec::new();
                    loop {
                        buf.clear();
                        match lines.read_until(b'\n', &mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let mut g = p.lock().unwrap_or_else(|q| q.into_inner());
                                g.err.push(&buf);
                                let _ = g.tx.send(Frame::Stderr(Bytes::copy_from_slice(&buf)));
                            }
                        }
                    }
                });
            }
        };
        pump(stdout, stderr, proc_.clone());
        // The waiter owns the child; a kill goes through `Child::start_kill` on this handle,
        // which is why the handle is kept in the Proc rather than moved here alone.
        proc_.lock().unwrap_or_else(|p| p.into_inner()).child = Some(child);
        let p = proc_.clone();
        tokio::spawn(async move {
            loop {
                let status = {
                    let mut g = p.lock().unwrap_or_else(|q| q.into_inner());
                    match g.child.as_mut() {
                        Some(c) => c.try_wait(),
                        None => break,
                    }
                };
                match status {
                    Ok(Some(st)) => {
                        let code = st.code().unwrap_or(-1);
                        let mut g = p.lock().unwrap_or_else(|q| q.into_inner());
                        g.state = State::Exited;
                        g.exit_code = Some(code);
                        g.exited_at = Some(std::time::Instant::now());
                        g.child = None;
                        // A server that could not take its port: the answer is a sentence naming
                        // the port and who holds it, composed once here rather than left for the
                        // model to read out of a stack trace (spec §4.6).
                        if code != 0 {
                            let (err, _, _) = g.err.read_since(0);
                            if let Some(port) = port_conflict(&err, &g.cmd) {
                                g.failed = Some(format!("port {port} in use"));
                            }
                        }
                        // Not waited on: a write in flight owns the lock and its own error says
                        // the pipe is gone; the handle drops with the Proc either way.
                        if let Ok(mut s) = g.stdin.try_lock() {
                            *s = None;
                        }
                        let _ = g.tx.send(Frame::Exit(code));
                        tracing::info!(process = %g.id, code, "ide.process.exited");
                        break;
                    }
                    Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
                    Err(_) => break,
                }
            }
        });
        tracing::info!(process = %id, "ide.process.started");
        Ok(id)
    }

    /// A process by id, WITHOUT the tree check — the stream routes and the reaper, which address
    /// a process they were already handed. Every tool path goes through `get_in` instead.
    pub fn get(&self, id: &str) -> Option<Arc<Mutex<Proc>>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).get(id).cloned()
    }

    /// A process by id, as seen FROM `tree`. A process of another tree answers `None`, the same as
    /// one that never existed: which trees hold which processes is not something to leak through
    /// the difference between "not yours" and "no such id".
    pub fn get_in(&self, tree: &str, id: &str) -> Option<Arc<Mutex<Proc>>> {
        self.get(id).filter(|p| p.lock().unwrap_or_else(|q| q.into_inner()).tree == tree)
    }

    pub fn list(&self) -> Vec<Arc<Mutex<Proc>>> {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        self.reap_locked(&mut map);
        map.values().cloned().collect()
    }

    /// One tree's processes, which is all any caller of `process_list` ever sees.
    pub fn list_in(&self, tree: &str) -> Vec<Arc<Mutex<Proc>>> {
        self.list().into_iter().filter(|p| p.lock().unwrap_or_else(|q| q.into_inner()).tree == tree).collect()
    }

    /// TERM now; KILL if it is still there five seconds later.
    pub async fn kill(&self, tree: &str, id: &str, signal: &str) -> Result<State, String> {
        let p = self.get_in(tree, id).ok_or_else(|| format!("no process {id}"))?;
        let pid = {
            let g = p.lock().unwrap_or_else(|q| q.into_inner());
            if g.state == State::Exited {
                return Ok(State::Exited);
            }
            g.child.as_ref().and_then(|c| c.id())
        };
        let Some(pid) = pid else { return Ok(State::Exited) };
        // The whole process group (`setsid` at spawn), so a shell's children go too.
        let sig = if signal == "KILL" { libc::SIGKILL } else { libc::SIGTERM };
        // SAFETY: a plain kill(2) on a pid this table spawned.
        unsafe { libc::kill(-(pid as i32), sig) };
        if sig == libc::SIGTERM {
            for _ in 0..50 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                if p.lock().unwrap_or_else(|q| q.into_inner()).state == State::Exited {
                    return Ok(State::Exited);
                }
            }
            // SAFETY: as above.
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if p.lock().unwrap_or_else(|q| q.into_inner()).state == State::Exited {
                return Ok(State::Exited);
            }
        }
        let state = p.lock().unwrap_or_else(|q| q.into_inner()).state;
        Ok(state)
    }

    pub async fn write_stdin(&self, tree: &str, id: &str, data: &[u8]) -> Result<usize, String> {
        let p = self.get_in(tree, id).ok_or_else(|| format!("no process {id}"))?;
        let slot = p.lock().unwrap_or_else(|q| q.into_inner()).stdin.clone();
        let mut g = slot.lock().await;
        let Some(s) = g.as_mut() else { return Err(format!("process {id} takes no more input")) };
        let r = s.write_all(data).await.map(|_| data.len()).map_err(|e| format!("stdin: {e}"));
        let _ = s.flush().await;
        r
    }

    fn reap_locked(&self, map: &mut HashMap<String, Arc<Mutex<Proc>>>) {
        map.retain(|_, p| {
            let g = p.lock().unwrap_or_else(|q| q.into_inner());
            !(g.state == State::Exited && g.exited_at.is_some_and(|t| t.elapsed().as_secs() > KEEP_EXITED_SECS))
        });
    }
}

pub(crate) fn rand_u64() -> u64 {
    // No rand dependency for an id: the clock and the pid are unique enough for one table.
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0) as u64;
    t ^ ((std::process::id() as u64) << 32) ^ (t >> 17)
}

/// The port a detached process failed to bind, when its output says it failed to bind one.
///
/// The net under §4.6's convention: `PORT` and `KL_PORT_RANGE` move most dev servers into their
/// tree's block, and the ones that ignore both collide with the main tree. A model reading a raw
/// `EADDRINUSE` stack trace guesses; a sentence naming the port and the holder does not.
///
/// Best effort by design, and in the one safe direction: the bind failure must be IN THE OUTPUT
/// before any number is claimed, and the number comes from the command line's own
/// `--port`/`-p`/`PORT=` hint. A failure with no readable hint answers `None` — the process is
/// still reported failed, with no port invented for it.
pub fn port_conflict(ring: &[u8], cmdline: &str) -> Option<u16> {
    // The first 4 KiB: a bind failure is the first thing a server prints, and scanning a full
    // 4 MiB ring for a string on every exit is work nothing asked for.
    let head = String::from_utf8_lossy(&ring[..ring.len().min(4096)]).to_lowercase();
    if !head.contains("eaddrinuse") && !head.contains("address already in use") {
        return None;
    }
    let words: Vec<&str> = cmdline.split_whitespace().collect();
    words
        .iter()
        .enumerate()
        .find_map(|(i, w)| match *w {
            "--port" | "-p" => words.get(i + 1).and_then(|n| n.parse().ok()),
            _ => w
                .strip_prefix("--port=")
                .or_else(|| w.strip_prefix("PORT="))
                .and_then(|n| n.parse().ok()),
        })
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ring_keeps_the_tail_and_reports_what_a_late_reader_missed() {
        let mut r = Ring::new(8);
        r.push(b"0123456789");
        assert_eq!(r.end(), 10);
        let (bytes, next, dropped) = r.read_since(0);
        assert_eq!(bytes, b"23456789");
        assert_eq!((next, dropped), (10, 2));
        let (bytes, next, dropped) = r.read_since(9);
        assert_eq!((bytes, next, dropped), (b"9".to_vec(), 10, 0));
    }

    #[tokio::test]
    async fn a_detached_process_streams_exits_and_can_be_read_by_offset() {
        let procs = Procs::default();
        let mut c = Command::new("sh");
        c.arg("-c").arg("echo one; echo two >&2; echo three");
        let id = procs.spawn("main", c, "sh -c …".into()).unwrap();
        let p = procs.get(&id).unwrap();
        for _ in 0..100 {
            if p.lock().unwrap().state == State::Exited {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let g = p.lock().unwrap();
        assert_eq!(g.exit_code, Some(0));
        assert_eq!(g.out.read_since(0).0, b"one\nthree\n");
        assert_eq!(g.err.read_since(0).0, b"two\n");
    }

    #[tokio::test]
    async fn two_writes_to_one_stdin_both_land() {
        let procs = Procs::default();
        let mut c = Command::new("sh");
        c.arg("-c").arg("cat");
        let id = procs.spawn("main", c, "cat".into()).unwrap();
        let (a, b) = tokio::join!(procs.write_stdin("main", &id, b"one\n"), procs.write_stdin("main", &id, b"two\n"));
        assert_eq!((a.unwrap(), b.unwrap()), (4, 4));
        let p = procs.get(&id).unwrap();
        for _ in 0..100 {
            if p.lock().unwrap().out.end() >= 8 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(p.lock().unwrap().out.end(), 8);
        procs.kill("main", &id, "KILL").await.unwrap();
    }

    #[tokio::test]
    async fn kill_ends_a_sleeping_shell_and_its_child() {
        let procs = Procs::default();
        let mut c = Command::new("sh");
        c.arg("-c").arg("sleep 30");
        c.process_group(0);
        let id = procs.spawn("main", c, "sh -c sleep".into()).unwrap();
        let st = procs.kill("main", &id, "TERM").await.unwrap();
        assert_eq!(st, State::Exited);
    }
}
