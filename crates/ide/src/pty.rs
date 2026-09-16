//! `GET /stream/pty`: one WebSocket, one login shell under a real terminal.
//!
//! `exec` already runs arbitrary commands as uid 1000, so a PTY is not a new capability — it is
//! the same one with a controlling terminal, which is what job control, readline and every TUI
//! need. Confinement stays the namespace (`allow-bench-tools`, the owner's own tunnel); there is
//! no auth code here, exactly as for the tool routes.
//!
//! Wire (spec `2026-09-16-desktop-shells-design.md`): binary frames are raw bytes both ways, text
//! frames are control JSON — client `{"resize":{"cols":N,"rows":N}}`, server `{"exit":code}` or
//! `{"error":"…"}` and then close. No session id, no scrollback replay, no reconnect: a closed
//! socket is a `SIGHUP` to the shell's process group. One socket, one shell, one life.
//!
//! `libc::openpty` + `fork`/`setsid`/`TIOCSCTTY`/`execvp` rather than `portable-pty`: `libc` is
//! already here and `kl` is a musl binary with a size budget.
use crate::server::App;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::unix::AsyncFd;

/// At most this many live shells per tool server; the pod's cgroup is the memory ceiling, as it
/// is for `exec`. A 9th gets `{"error":"too many shells"}` and a close.
pub const MAX_SHELLS: usize = 8;

/// How long the first `resize` is waited for before the shell starts at the terminal default —
/// the client sends one immediately, so this only bounds a client that never will.
const FIRST_RESIZE: std::time::Duration = std::time::Duration::from_secs(2);

/// A forked shell and the master side of its terminal.
pub struct Pty {
    /// Closed by `Drop` BEFORE the child is reaped: a shell blocked writing to a terminal
    /// nobody reads is stuck in exit, where even SIGKILL to its group answers EPERM (seen on
    /// every drop of a live shell, 2026-09-16). Dropping it as a field would run after.
    master: std::mem::ManuallyDrop<AsyncFd<OwnedFd>>,
    child: libc::pid_t,
    /// Set once reaped, so `wait` is idempotent and `Drop` does not signal a pid that may by then
    /// belong to somebody else.
    exit: Option<i32>,
}

/// `$SHELL`, then the two shells every image has. argv0 is `-name`, which is how a shell is told
/// it is a login shell — the pod prelude's profile is what builds `PATH` and the caches.
fn candidates() -> Vec<CString> {
    let mut v: Vec<CString> = Vec::new();
    if let Ok(s) = std::env::var("SHELL") {
        if !s.is_empty() {
            v.push(CString::new(s).unwrap_or_else(|_| CString::new("/bin/sh").unwrap()));
        }
    }
    for f in ["/bin/bash", "/bin/sh"] {
        v.push(CString::new(f).unwrap());
    }
    v
}

fn login_argv0(path: &CString) -> CString {
    let s = path.to_string_lossy();
    let base = s.rsplit('/').next().unwrap_or("sh");
    CString::new(format!("-{base}")).unwrap_or_else(|_| CString::new("-sh").unwrap())
}

pub fn spawn(root: &Path, cols: u16, rows: u16) -> io::Result<Pty> {
    let (mut master, mut slave): (RawFd, RawFd) = (-1, -1);
    let mut ws = winsize(cols, rows);
    // The size is set at open, so the shell and everything it starts never see 80x24 first and
    // reflow on the client's resize.
    // Raw pointers cast with `as _` for the last three: they are `*const` on Linux and `*mut` on
    // macOS, and a `&mut` coerces to either but trips clippy's unnecessary_mut_passed on Linux.
    if unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut::<libc::c_char>(), std::ptr::null_mut::<libc::termios>(), std::ptr::addr_of_mut!(ws) as _) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // Everything the child needs is built BEFORE the fork: between fork and exec only
    // async-signal-safe calls are sound in a process with tokio's threads in it.
    let cwd = CString::new(root.as_os_str().as_encoded_bytes()).map_err(|_| io::Error::other("root has a NUL"))?;
    let shells = candidates();
    let argv0: Vec<CString> = shells.iter().map(login_argv0).collect();
    // The environment too: `setenv` allocates, so it is built here and handed to `execve`.
    let envp_owned: Vec<CString> = std::env::vars_os()
        .filter(|(k, _)| k != "TERM" && k != "COLORTERM")
        .map(|(k, v)| {
            let mut b = k.into_encoded_bytes();
            b.push(b'=');
            b.extend(v.into_encoded_bytes());
            CString::new(b).unwrap_or_default()
        })
        .chain([CString::new("TERM=xterm-256color").unwrap(), CString::new("COLORTERM=truecolor").unwrap()])
        .collect();
    let mut envp: Vec<*const libc::c_char> = envp_owned.iter().map(|e| e.as_ptr()).collect();
    envp.push(std::ptr::null());

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let e = io::Error::last_os_error();
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        return Err(e);
    }
    if pid == 0 {
        unsafe {
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
            for target in 0..3 {
                libc::dup2(slave, target);
            }
            if slave > 2 {
                libc::close(slave);
            }
            libc::close(master);
            libc::chdir(cwd.as_ptr());
            for (shell, a0) in shells.iter().zip(&argv0) {
                let argv = [a0.as_ptr(), std::ptr::null()];
                libc::execve(shell.as_ptr(), argv.as_ptr(), envp.as_ptr());
            }
            // No shell at all: 127 is what a shell itself reports for "not found".
            libc::_exit(127);
        }
    }
    unsafe { libc::close(slave) };
    // Nonblocking: `AsyncFd` only reports readiness, the read and write are ours.
    unsafe { libc::fcntl(master, libc::F_SETFL, libc::O_NONBLOCK) };
    let master = AsyncFd::new(unsafe { OwnedFd::from_raw_fd(master) })?;
    Ok(Pty { master: std::mem::ManuallyDrop::new(master), child: pid, exit: None })
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 }
}

impl Pty {
    pub fn resize(&self, cols: u16, rows: u16) {
        let ws = winsize(cols, rows);
        unsafe { libc::ioctl(self.master.get_ref().as_raw_fd(), libc::TIOCSWINSZ as _, &ws as *const libc::winsize) };
    }

    pub async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut ready = self.master.readable().await?;
            let done = ready.try_io(|fd| {
                let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match done {
                Ok(Ok(n)) => return Ok(n),
                // The last slave fd closed: Linux answers EIO on the master where a pipe would
                // answer EOF. The shell is gone either way, so it is an end of stream, not a fault.
                Ok(Err(e)) if e.raw_os_error() == Some(libc::EIO) => return Ok(0),
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
    }

    pub async fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        let mut rest = bytes;
        while !rest.is_empty() {
            let mut ready = self.master.writable().await?;
            let done = ready.try_io(|fd| {
                let n = unsafe { libc::write(fd.as_raw_fd(), rest.as_ptr().cast(), rest.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match done {
                Ok(Ok(n)) => rest = &rest[n..],
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }

    /// The child's exit code, 128+signal when it was killed. Called after the master reads EOF,
    /// where the child is already gone or microseconds from it — so a `WNOHANG` miss is waited out
    /// rather than reported as "still running".
    pub fn wait(&mut self) -> Option<i32> {
        if self.exit.is_some() {
            return self.exit;
        }
        let mut st = 0;
        let mut r = unsafe { libc::waitpid(self.child, &mut st, libc::WNOHANG) };
        if r == 0 {
            r = unsafe { libc::waitpid(self.child, &mut st, 0) };
        }
        if r != self.child {
            return None;
        }
        self.exit = Some(exit_code(st));
        self.exit
    }
}

fn exit_code(st: libc::c_int) -> i32 {
    if libc::WIFEXITED(st) {
        libc::WEXITSTATUS(st)
    } else if libc::WIFSIGNALED(st) {
        128 + libc::WTERMSIG(st)
    } else {
        -1
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let reaped = self.exit.is_some();
        // Safety: nothing touches `master` after this, and `Pty` is dropped once. The order is
        // the point: a shell blocked writing to a terminal nobody reads is stuck in exit, and the
        // close is what fails that write. Dropping it as a field would run after the reap.
        unsafe { std::mem::ManuallyDrop::drop(&mut self.master) };
        if reaped {
            return;
        }
        let child = self.child;
        // Off the runtime: a shell that is slow to die must not hold the worker that dropped it —
        // on a current-thread runtime that is every other socket on the server.
        std::thread::spawn(move || hangup(child));
    }
}

/// SIGHUP the shell's process group — what a person left running in the foreground is the point
/// of the hangup — then reap; SIGKILL only if it ignored the hangup.
fn hangup(child: libc::pid_t) {
    for pid in [-child, child] {
        unsafe { libc::kill(pid, libc::SIGHUP) };
    }
    for _ in 0..100 {
        let mut st = 0;
        if unsafe { libc::waitpid(child, &mut st, libc::WNOHANG) } != 0 {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    for pid in [-child, child] {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let mut st = 0;
    unsafe { libc::waitpid(child, &mut st, 0) };
}

/// One live shell against `App::shells`, released on every exit path because it is a drop.
struct Slot(Arc<App>);

impl Slot {
    fn take(app: &Arc<App>) -> Option<Slot> {
        app.shells
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < MAX_SHELLS).then_some(n + 1))
            .ok()
            .map(|_| Slot(app.clone()))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.shells.fetch_sub(1, Ordering::SeqCst);
    }
}

pub async fn handler(State(app): State<Arc<App>>, ws: WebSocketUpgrade) -> Response {
    let Some(slot) = Slot::take(&app) else {
        // Refused after the upgrade, not as an HTTP status: the tab is already a terminal and has
        // nowhere but the socket to print why.
        return ws.on_upgrade(|sock| fail(sock, "too many shells".into())).into_response();
    };
    let root = app.cfg.root.clone();
    ws.on_upgrade(move |sock| pump(sock, root, slot)).into_response()
}

async fn fail(mut sock: WebSocket, why: String) {
    let _ = sock.send(Message::Text(serde_json::json!({ "error": why }).to_string().into())).await;
    let _ = sock.close().await;
}

/// `{"resize":{"cols":N,"rows":N}}` → the pair, anything else → None.
fn resize_of(text: &str) -> Option<(u16, u16)> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let r = v.get("resize")?;
    let cols = u16::try_from(r.get("cols")?.as_u64()?).ok()?;
    let rows = u16::try_from(r.get("rows")?.as_u64()?).ok()?;
    (cols > 0 && rows > 0).then_some((cols, rows))
}

async fn pump(sock: WebSocket, root: std::path::PathBuf, _slot: Slot) {
    let (mut tx, mut rx) = sock.split();
    // The client's first frame is its size. Anything else it sent first is kept and delivered to
    // the shell once there is one, so no keystroke is lost to the race.
    let mut pending: Option<Vec<u8>> = None;
    let mut size = (80u16, 24u16);
    match tokio::time::timeout(FIRST_RESIZE, rx.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => size = resize_of(&t).unwrap_or(size),
        Ok(Some(Ok(Message::Binary(b)))) => pending = Some(b.to_vec()),
        Ok(Some(Ok(_))) | Err(_) => {}
        // Closed or broken before it asked for anything.
        Ok(None) | Ok(Some(Err(_))) => return,
    }

    let mut pty = match spawn(&root, size.0, size.1) {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(Message::Text(serde_json::json!({ "error": e.to_string() }).to_string().into())).await;
            let _ = tx.close().await;
            return;
        }
    };
    if let Some(b) = pending {
        let _ = pty.write_all(&b).await;
    }

    let mut buf = vec![0u8; 64 << 10];
    loop {
        tokio::select! {
            read = pty.read(&mut buf) => match read {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                        return;
                    }
                }
            },
            msg = rx.next() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    if pty.write_all(&b).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Text(t))) => {
                    if let Some((c, r)) = resize_of(&t) {
                        pty.resize(c, r);
                    }
                }
                Some(Ok(_)) => {}
                // Close or a broken socket: the drop hangs the shell up.
                Some(Err(_)) | None => return,
            },
        }
    }
    let code = pty.wait().unwrap_or(-1);
    let _ = tx.send(Message::Text(serde_json::json!({ "exit": code }).to_string().into())).await;
    let _ = tx.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    /// Every test here wants the same predictable shell, and `SHELL` is process-wide.
    fn use_sh() {
        std::env::set_var("SHELL", "/bin/sh");
    }

    async fn drain(pty: &Pty) -> String {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(5), pty.read(&mut buf)).await {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
                Ok(Err(_)) => break,
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[tokio::test]
    async fn a_shell_runs_under_the_terminal_and_reports_its_exit_code() {
        use_sh();
        let tmp = tempfile::tempdir().unwrap();
        let mut pty = spawn(tmp.path(), 80, 24).unwrap();
        pty.write_all(b"printf kl-%s ok\\n; exit 3\n").await.unwrap();
        let out = drain(&pty).await;
        assert!(out.contains("kl-ok"), "{out:?}");
        assert_eq!(pty.wait(), Some(3));
    }

    #[tokio::test]
    async fn a_resize_reaches_the_shell_as_a_window_size() {
        use_sh();
        let tmp = tempfile::tempdir().unwrap();
        let mut pty = spawn(tmp.path(), 80, 24).unwrap();
        pty.resize(120, 40);
        pty.write_all(b"stty size; exit 0\n").await.unwrap();
        let out = drain(&pty).await;
        assert!(out.contains("40 120"), "{out:?}");
        assert_eq!(pty.wait(), Some(0));
    }

    #[tokio::test]
    async fn dropping_the_pty_hangs_up_the_process_group() {
        use_sh();
        let tmp = tempfile::tempdir().unwrap();
        let pty = spawn(tmp.path(), 80, 24).unwrap();
        pty.write_all(b"sleep 300\n").await.unwrap();
        let pid = pty.child;
        drop(pty);
        for _ in 0..100 {
            // Gone AND reaped: ESRCH, not a zombie the pid still names.
            if unsafe { libc::kill(pid, 0) } == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("child {pid} outlived its socket by a second");
    }

    async fn open(addr: std::net::SocketAddr) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
        let (s, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/stream/pty")).await.unwrap();
        s
    }

    /// The 9th shell is refused on the socket, and a closed one frees its slot.
    #[tokio::test]
    async fn a_ninth_shell_is_refused_and_a_closed_one_frees_its_slot() {
        use_sh();
        use futures::StreamExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let app = Arc::new(App::new(Config { bind: "127.0.0.1:0".parse().unwrap(), root: home.clone(), home, graft_dir: None }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = crate::server::router(app.clone());
        tokio::spawn(async move { axum::serve(listener, served).await });

        // Seven of the eight slots are simply counted, not lived: one real shell is enough to
        // prove the gate, and eight forked login shells beside the rest of the suite is a load
        // this machine answers with unrelated failures.
        app.shells.fetch_add(MAX_SHELLS - 1, Ordering::SeqCst);
        let mut eighth = open(addr).await;
        eighth.send(tokio_tungstenite::tungstenite::Message::Text(r#"{"resize":{"cols":80,"rows":24}}"#.into())).await.unwrap();
        for _ in 0..200 {
            if app.shells.load(Ordering::SeqCst) == MAX_SHELLS {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(app.shells.load(Ordering::SeqCst), MAX_SHELLS);

        let mut ninth = open(addr).await;
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ninth.next()).await.unwrap().unwrap().unwrap();
        assert!(msg.into_text().unwrap().contains("too many shells"));

        drop(eighth);
        for _ in 0..200 {
            if app.shells.load(Ordering::SeqCst) < MAX_SHELLS {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(app.shells.load(Ordering::SeqCst), MAX_SHELLS - 1);
    }
}
