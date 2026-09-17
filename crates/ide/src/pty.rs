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
//! A `?session=<name>` is a PTY this process KEEPS after the socket drops (spec §7 of
//! `2026-09-17-terminals-tmux-and-tabs-design.md`): output goes to a per-session ring
//! (`PTY_RING_BYTES`), a reconnect with the same name replays the ring and then streams live, and a
//! second attach detaches the first. tmux used to own this and is gone — it owned scrollback, the
//! mouse and a prefix key, which is what made the terminal feel foreign; xterm.js owns them now.
//! Without a name the route keeps the one-shot shell, which is what `exec`-style callers and the
//! probe want. `/stream/pty/sessions` lists and `DELETE /stream/pty/sessions/{name}` kills.
//!
//! Processes never survive the pod, but TEXT does: the ring is flushed to
//! `{ws}/.cache/shell/<name>.log` on detach and every 30 s, and a session created under a name that
//! has a log replays it once before the new shell's first prompt.
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

/// The uid's passwd shell, when it has one that exists and is executable. `getpwuid_r` rather
/// than `$SHELL`: `kl ide serve` is started through `su -s /bin/sh`, so `$SHELL` in a workspace
/// pod is `/bin/sh` and every PTY landed in ash while ssh to the same pod got the Nix zsh. The
/// passwd entry is what ssh reads, so reading it here is what makes the two shells one shell.
fn passwd_shell() -> Option<CString> {
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = [0u8; 4096];
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: `buf` outlives the `pwd` we read from, which is all `getpwuid_r` borrows it for.
    let rc = unsafe { libc::getpwuid_r(libc::getuid(), &mut pwd, buf.as_mut_ptr().cast(), buf.len(), &mut out) };
    if rc != 0 || out.is_null() || pwd.pw_shell.is_null() {
        return None;
    }
    let shell = unsafe { std::ffi::CStr::from_ptr(pwd.pw_shell) }.to_owned();
    if shell.as_bytes().is_empty() || unsafe { libc::access(shell.as_ptr(), libc::X_OK) } != 0 {
        return None;
    }
    Some(shell)
}

/// The passwd shell, then `$SHELL`, then the two shells every image has. argv0 is `-name`, which
/// is how a shell is told it is a login shell — the pod prelude's profile is what builds `PATH`
/// and the caches.
fn candidates() -> Vec<CString> {
    let mut v: Vec<CString> = Vec::new();
    v.extend(passwd_shell());
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

/// `[a-z0-9-]{1,48}`, no leading dash. The desktop names sessions (`kl-<tab>-<n>`); the server
/// never invents one, and the name is also the `.log` file's, so nothing else may reach a path.
pub fn valid_session(name: &str) -> bool {
    let body = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-';
    name.len() <= 48 && name.bytes().next().is_some_and(|b| body(&b) && b != b'-') && name.bytes().all(|b| body(&b))
}

/// Every candidate login shell, in the order to try it. Named or not, a terminal is a login shell:
/// the persistence above it is this process's, not another program in the path.
fn program() -> Vec<(CString, Vec<CString>)> {
    candidates()
        .into_iter()
        .map(|sh| {
            let a0 = login_argv0(&sh);
            (sh, vec![a0])
        })
        .collect()
}

/// What `kl ide serve` was started with is NOT what an ssh login gets: the prelude starts it
/// through `su kl -s /bin/sh`, and the pod env it inherits is only half the story. Three groups
/// go, one comes back.
///
/// `SHELL` is the load-bearing one. `su -s /bin/sh` leaves it `/bin/sh`, and this process passes
/// it down; sshd instead sets it from the passwd entry. `spawn` already execs the passwd shell
/// (that was the first half of this bug — every PTY landed in busybox ash while ssh got the Nix
/// zsh, so the prompt was ash's `dir $` with no rc and no starship), but anything the person's
/// shell RESPAWNS reads `$SHELL`, and `/bin/sh` there is the same bug one layer down.
///
/// `TERM`/`COLORTERM` are ours to state: this end is a real terminal whatever the server's env
/// says. `TMUX` is only ever set when this process is itself inside a session (a dev machine,
/// never a pod), where it would make a person's own tmux refuse to nest.
///
/// The rest are this process's own private state, which an ssh login never carries: the serve
/// command's telemetry identity (anything the person then runs would report as `kl-ide`), and the
/// position vars a forked child must not inherit — `spawn` chdirs, so `PWD`/`OLDPWD` would name a
/// directory the shell is not in (zsh recomputes them, `sh` believes them) and `SHLVL` would start
/// the person two levels deep.
fn child_env(vars: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>, shell: Option<&std::ffi::CStr>) -> Vec<CString> {
    const DROP: [&str; 9] = ["TERM", "COLORTERM", "TMUX", "SHLVL", "PWD", "OLDPWD", "_", "OTEL_SERVICE_NAME", "KLOUDLITE_OTLP_URL"];
    let replace_shell = shell.is_some();
    let shell = shell.map(|s| {
        let mut b = b"SHELL=".to_vec();
        b.extend_from_slice(s.to_bytes());
        CString::new(b).unwrap_or_default()
    });
    // `SHELL` is replaced, not dropped: with no passwd entry to read there is nothing better to
    // say than what we were started with.
    vars.filter(|(k, _)| !DROP.iter().any(|d| k == d) && !(replace_shell && k == "SHELL"))
        .map(|(k, v)| {
            let mut b = k.into_encoded_bytes();
            b.push(b'=');
            b.extend(v.into_encoded_bytes());
            CString::new(b).unwrap_or_default()
        })
        .chain([CString::new("TERM=xterm-256color").unwrap(), CString::new("COLORTERM=truecolor").unwrap()])
        .chain(shell)
        .collect()
}

pub fn spawn(root: &Path, cols: u16, rows: u16) -> io::Result<Pty> {
    let attempts = program();
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
    let argv_ptrs: Vec<Vec<*const libc::c_char>> = attempts.iter().map(|(_, a)| a.iter().map(|c| c.as_ptr()).chain([std::ptr::null()]).collect()).collect();
    // The environment too: `setenv` allocates, so it is built here and handed to `execve`.
    let envp_owned = child_env(std::env::vars_os(), passwd_shell().as_deref());
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
            for ((path, _), argv) in attempts.iter().zip(&argv_ptrs) {
                libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr());
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

/// What a named session keeps of its own output. 4 MiB is xterm.js's 10 000 lines with room to
/// spare, and it is the ceiling on the `.log` file too.
pub const PTY_RING_BYTES: usize = 4 << 20;

/// How often a live session's ring reaches the disk, so a pod that is killed rather than stopped
/// still leaves the last half-minute behind.
const FLUSH_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// What an attached socket asks the session's own task to do; the task owns the `Pty`, so nothing
/// else ever reads it, writes it or reaps it.
enum Cmd {
    In(Vec<u8>),
    Resize(u16, u16),
    /// `DELETE`: dropping the `Pty` is what hangs the process GROUP up.
    Kill,
}

/// The ring and the live fan-out under ONE lock: an attach takes its snapshot and its subscription
/// together, so no byte can land between the two and be lost or shown twice.
struct Fan {
    ring: std::collections::VecDeque<u8>,
    tx: tokio::sync::broadcast::Sender<Vec<u8>>,
}

/// One named terminal: a shell that outlives its socket.
pub struct Session {
    created: u64,
    pid: libc::pid_t,
    input: tokio::sync::mpsc::Sender<Cmd>,
    out: std::sync::Mutex<Fan>,
    /// Bumped by every attach; the previous attach watches it and lets go. One socket at a time is
    /// the whole point — two would fight over the same screen.
    attach: tokio::sync::watch::Sender<u64>,
    exit: tokio::sync::watch::Sender<Option<i32>>,
    log: std::path::PathBuf,
    /// Held by the SESSION, not the socket: a named shell counts against `MAX_SHELLS` for as long
    /// as it lives, which is now longer than any one client.
    _slot: Slot,
}

impl Session {
    fn snapshot(&self) -> (Vec<u8>, tokio::sync::broadcast::Receiver<Vec<u8>>) {
        let f = self.out.lock().expect("pty ring");
        (f.ring.iter().copied().collect(), f.tx.subscribe())
    }

    fn push(&self, bytes: &[u8]) {
        let mut f = self.out.lock().expect("pty ring");
        f.ring.extend(bytes);
        let over = f.ring.len().saturating_sub(PTY_RING_BYTES);
        f.ring.drain(..over);
        // No receiver is the ordinary detached case, not an error.
        let _ = f.tx.send(bytes.to_vec());
    }

    /// The ring as it stands, to the session's log. Blocking: at most `PTY_RING_BYTES` to a local
    /// path, twice a minute per terminal.
    fn flush(&self) {
        let bytes: Vec<u8> = self.out.lock().expect("pty ring").ring.iter().copied().collect();
        if let Some(dir) = self.log.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&self.log, &bytes) {
            tracing::warn!(error = %e, path = %self.log.display(), "pty.log.write.failed");
        }
    }
}

/// Every live named terminal in this process. The table IS the state — there is no other server to
/// ask, which is why a pod restart brings back text and never a process.
#[derive(Default)]
pub struct Sessions(std::sync::Mutex<std::collections::HashMap<String, Arc<Session>>>);

impl Sessions {
    fn get(&self, name: &str) -> Option<Arc<Session>> {
        self.0.lock().expect("pty sessions").get(name).cloned()
    }

    fn list(&self) -> Vec<serde_json::Value> {
        let map = self.0.lock().expect("pty sessions");
        let mut rows: Vec<_> = map
            .iter()
            .map(|(name, s)| {
                serde_json::json!({ "name": name, "windows": 1, "attached": s.attach.receiver_count() > 0, "created": s.created, "pid": s.pid })
            })
            .collect();
        rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        rows
    }
}

/// The path a session's text is kept at, confined under the workspace like every other path this
/// server touches. The name is already `[a-z0-9-]`, so this can only refuse a root that is itself
/// outside the home.
fn log_path(app: &App, name: &str) -> io::Result<std::path::PathBuf> {
    crate::paths::confine(&app.cfg.root, &app.cfg.home, &format!(".cache/shell/{name}.log")).map_err(|e| io::Error::other(format!("{e:?}")))
}

/// Open a named session, or attach to the one that is already there. A NEW one replays whatever a
/// previous life of this pod left in its log, once, ahead of the shell's first prompt.
fn open(app: &Arc<App>, name: &str, cols: u16, rows: u16) -> io::Result<Arc<Session>> {
    if let Some(s) = app.ptys.get(name) {
        return Ok(s);
    }
    let log = log_path(app, name)?;
    let _slot = Slot::take(app).ok_or_else(|| io::Error::other("too many shells"))?;
    let mut pty = spawn(&app.cfg.root, cols, rows)?;
    let (input, mut rx) = tokio::sync::mpsc::channel(64);
    let (tx, _) = tokio::sync::broadcast::channel(256);
    let session = Arc::new(Session {
        created: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        pid: pty.child,
        input,
        out: std::sync::Mutex::new(Fan { ring: std::collections::VecDeque::new(), tx }),
        attach: tokio::sync::watch::Sender::new(0),
        exit: tokio::sync::watch::Sender::new(None),
        log,
        _slot,
    });
    if let Ok(history) = std::fs::read(&session.log) {
        session.push(&history);
    }
    app.ptys.0.lock().expect("pty sessions").insert(name.to_string(), session.clone());

    let (app, sess, name) = (app.clone(), session.clone(), name.to_string());
    tokio::spawn(async move {
        let mut buf = vec![0u8; 64 << 10];
        let mut tick = tokio::time::interval(FLUSH_EVERY);
        tick.tick().await;
        let mut killed = false;
        loop {
            tokio::select! {
                read = pty.read(&mut buf) => match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sess.push(&buf[..n]),
                },
                cmd = rx.recv() => match cmd {
                    Some(Cmd::In(b)) => if pty.write_all(&b).await.is_err() { break },
                    Some(Cmd::Resize(c, r)) => pty.resize(c, r),
                    // `None` is the table dropping the last sender, which only the kill does.
                    Some(Cmd::Kill) | None => { killed = true; break }
                },
                _ = tick.tick() => sess.flush(),
            }
        }
        app.ptys.0.lock().expect("pty sessions").remove(&name);
        sess.flush();
        // A killed shell is reaped by `Pty::drop`; only a shell that ended on its own has a code
        // worth telling the client.
        let code = if killed { -1 } else { pty.wait().unwrap_or(-1) };
        drop(pty);
        let _ = sess.exit.send(Some(code));
    });
    Ok(session)
}

#[derive(serde::Deserialize)]
pub struct Query {
    /// The session to attach to or create. Absent = today's one-shot login shell.
    session: Option<String>,
}

pub async fn handler(State(app): State<Arc<App>>, axum::extract::Query(q): axum::extract::Query<Query>, ws: WebSocketUpgrade) -> Response {
    // A bad name is a request error rather than something to print into a terminal: the desktop,
    // not a person, chooses these.
    if q.session.as_deref().is_some_and(|n| !valid_session(n)) {
        return (axum::http::StatusCode::BAD_REQUEST, "session must match [a-z0-9-]{1,48}").into_response();
    }
    if let Some(name) = q.session {
        return ws.on_upgrade(move |sock| attach(sock, app, name)).into_response();
    }
    let Some(slot) = Slot::take(&app) else {
        // Refused after the upgrade, not as an HTTP status: the tab is already a terminal and has
        // nowhere but the socket to print why.
        return ws.on_upgrade(|sock| fail(sock, "too many shells".into())).into_response();
    };
    let root = app.cfg.root.clone();
    ws.on_upgrade(move |sock| pump(sock, root, slot)).into_response()
}

/// `GET /stream/pty/sessions` → `[{name, windows, attached, created, pid}]`: another device shows
/// the same terminals and reattaches them by name.
pub async fn sessions(State(app): State<Arc<App>>) -> axum::Json<Vec<serde_json::Value>> {
    axum::Json(app.ptys.list())
}

/// `DELETE /stream/pty/sessions/{name}`: killing a shell is the person's choice, which a dropped
/// socket never is — so it is a route of its own and not something the detach does.
pub async fn kill_session(State(app): State<Arc<App>>, axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    if !valid_session(&name) {
        return (axum::http::StatusCode::BAD_REQUEST, "session must match [a-z0-9-]{1,48}").into_response();
    }
    let Some(s) = app.ptys.get(&name) else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };
    let _ = s.input.send(Cmd::Kill).await;
    // Not "asked to die": the table is what the caller will look at next, so the delete waits for
    // the name to leave it.
    for _ in 0..200 {
        if app.ptys.get(&name).is_none() {
            return axum::http::StatusCode::NO_CONTENT.into_response();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "the shell did not die").into_response()
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

/// The client's first frame is its size. Anything else it sent first is kept and delivered to the
/// shell once there is one, so no keystroke is lost to the race. `None` = it went away first.
async fn first_frame(rx: &mut futures::stream::SplitStream<WebSocket>) -> Option<((u16, u16), Option<Vec<u8>>)> {
    let mut size = (80u16, 24u16);
    let mut pending = None;
    match tokio::time::timeout(FIRST_RESIZE, rx.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => size = resize_of(&t).unwrap_or(size),
        Ok(Some(Ok(Message::Binary(b)))) => pending = Some(b.to_vec()),
        Ok(Some(Ok(_))) | Err(_) => {}
        Ok(None) | Ok(Some(Err(_))) => return None,
    }
    Some((size, pending))
}

/// A named terminal: open or reattach, replay the ring, then stream live. The socket closing is a
/// DETACH — the shell and its screen stay — and a second attach with the same name takes the
/// session over, because two clients on one screen fight.
async fn attach(sock: WebSocket, app: Arc<App>, name: String) {
    let (mut tx, mut rx) = sock.split();
    let Some((size, pending)) = first_frame(&mut rx).await else { return };
    let session = match open(&app, &name, size.0, size.1) {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(Message::Text(serde_json::json!({ "error": e.to_string() }).to_string().into())).await;
            let _ = tx.close().await;
            return;
        }
    };
    // Both under the ring's lock, so nothing lands between the replay and the live stream.
    let (replay, mut live) = session.snapshot();
    let mut mine = 0;
    session.attach.send_modify(|g| {
        *g += 1;
        mine = *g;
    });
    let mut detached = session.attach.subscribe();
    let mut exited = session.exit.subscribe();
    if !replay.is_empty() && tx.send(Message::Binary(replay.into())).await.is_err() {
        return;
    }
    // The size is the NEW client's: a reattach from a different window resizes the shell rather
    // than leaving it drawing for the one that left.
    let _ = session.input.send(Cmd::Resize(size.0, size.1)).await;
    if let Some(b) = pending {
        let _ = session.input.send(Cmd::In(b)).await;
    }

    loop {
        tokio::select! {
            out = live.recv() => match out {
                Ok(b) => if tx.send(Message::Binary(b.into())).await.is_err() { break },
                // A client too slow for 256 chunks loses bytes rather than the session: the ring
                // still has them, and the next reattach replays it.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            },
            msg = rx.next() => match msg {
                Some(Ok(Message::Binary(b))) => if session.input.send(Cmd::In(b.to_vec())).await.is_err() { break },
                Some(Ok(Message::Text(t))) => if let Some((c, r)) = resize_of(&t) {
                    let _ = session.input.send(Cmd::Resize(c, r)).await;
                },
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            _ = detached.changed() => if *detached.borrow() != mine { break },
            _ = exited.changed() => break,
        }
    }
    // The text reaches the disk at the moment a person walks away, not only on the next tick.
    session.flush();
    let code = *session.exit.borrow();
    if let Some(code) = code {
        let _ = tx.send(Message::Text(serde_json::json!({ "exit": code }).to_string().into())).await;
        let _ = tx.close().await;
    }
}

async fn pump(sock: WebSocket, root: std::path::PathBuf, _slot: Slot) {
    let (mut tx, mut rx) = sock.split();
    let Some((size, pending)) = first_frame(&mut rx).await else { return };

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

    /// The PTY's shell must see what an ssh login sees: the workspace's own `login_env`, a
    /// `SHELL` naming the shell we actually exec (tmux's `default-shell` reads it, and `/bin/sh`
    /// there is how the prompt bug came back one layer down), and none of `kl ide serve`'s own
    /// process state.
    #[test]
    fn the_child_env_matches_an_ssh_login() {
        let os = |s: &str| std::ffi::OsString::from(s);
        let inherited = [
            ("ZDOTDIR", "/home/kl/.config/zsh"),
            ("KL_WORKSPACE", "/home/kl/workspaces/w"),
            ("PATH", "/nix/profile/current/bin:/bin"),
            // `su -s /bin/sh`'s, not the person's.
            ("SHELL", "/bin/sh"),
            // The serve process's own, never an ssh login's.
            ("OTEL_SERVICE_NAME", "kl-ide"),
            ("KLOUDLITE_OTLP_URL", "http://otel:4318"),
            ("SHLVL", "2"),
            ("PWD", "/"),
            ("OLDPWD", "/"),
            ("TERM", "dumb"),
        ];
        let zsh = std::ffi::CString::new("/nix/profile/current/bin/zsh").unwrap();
        let env: Vec<String> = child_env(inherited.iter().map(|(k, v)| (os(k), os(v))), Some(&zsh)).iter().map(|c| c.to_string_lossy().into_owned()).collect();
        let has = |k: &str| env.iter().any(|e| e.starts_with(&format!("{k}=")));

        assert!(env.contains(&"SHELL=/nix/profile/current/bin/zsh".to_string()), "{env:?}");
        assert!(env.contains(&"ZDOTDIR=/home/kl/.config/zsh".to_string()));
        assert!(env.contains(&"KL_WORKSPACE=/home/kl/workspaces/w".to_string()));
        assert!(env.contains(&"PATH=/nix/profile/current/bin:/bin".to_string()));
        assert!(env.contains(&"TERM=xterm-256color".to_string()), "the terminal is ours to state");
        assert!(env.contains(&"COLORTERM=truecolor".to_string()));
        for gone in ["OTEL_SERVICE_NAME", "KLOUDLITE_OTLP_URL", "SHLVL", "PWD", "OLDPWD"] {
            assert!(!has(gone), "{gone} is the serve process's, not the person's: {env:?}");
        }
        assert_eq!(env.iter().filter(|e| e.starts_with("SHELL=")).count(), 1);
        assert_eq!(env.iter().filter(|e| e.starts_with("TERM=")).count(), 1);
    }

    /// `$SHELL` is `/bin/sh` inside a workspace pod because `kl ide serve` is started under
    /// `su -s /bin/sh`; the passwd entry is the one that names the person's real shell.
    #[test]
    fn the_passwd_shell_beats_shell() {
        use_sh();
        let want = passwd_shell();
        let first = candidates().into_iter().next().unwrap();
        match want {
            Some(p) => assert_eq!(first, p, "the uid's passwd shell comes first"),
            None => assert_eq!(first.to_str().unwrap(), "/bin/sh", "$SHELL is next when there is none"),
        }
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

    type Sock = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

    async fn open(addr: std::net::SocketAddr) -> Sock {
        dial(addr, "").await.unwrap()
    }

    async fn dial(addr: std::net::SocketAddr, query: &str) -> Result<Sock, tokio_tungstenite::tungstenite::Error> {
        tokio_tungstenite::connect_async(format!("ws://{addr}/stream/pty{query}")).await.map(|(s, _)| s)
    }

    /// A server on a port, as a client sees it.
    async fn served() -> (tempfile::TempDir, Arc<App>, std::net::SocketAddr) {
        let tmp = tempfile::tempdir().unwrap();
        let (app, addr) = serve_at(tmp.path()).await;
        (tmp, app, addr)
    }

    /// A server over a given workspace root — called twice over one root, it is what a restarted
    /// pod looks like to a client.
    async fn serve_at(root: &Path) -> (Arc<App>, std::net::SocketAddr) {
        let home = root.canonicalize().unwrap();
        let app = Arc::new(App::new(Config { bind: "127.0.0.1:0".parse().unwrap(), root: home.clone(), home, graft_dir: None }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = crate::server::router(app.clone());
        tokio::spawn(async move { axum::serve(listener, served).await });
        (app, addr)
    }

    /// The 9th shell is refused on the socket, and a closed one frees its slot.
    #[tokio::test]
    async fn a_ninth_shell_is_refused_and_a_closed_one_frees_its_slot() {
        use_sh();
        use futures::StreamExt as _;
        let (_tmp, app, addr) = served().await;

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

    #[test]
    fn a_session_name_is_lowercase_digits_and_dashes() {
        for ok in ["kl-bench-1", "a", "0", &"a".repeat(48)] {
            assert!(valid_session(ok), "{ok:?}");
        }
        for bad in ["", &"a".repeat(49), "Kl-1", "kl_1", "kl 1", "../x", "kl;ls", "kl\n1", "-d"] {
            assert!(!valid_session(bad), "{bad:?}");
        }
    }

    /// The name is the desktop's, and a bad one is refused at the handshake — before tmux, and
    /// before there is a terminal to print an error into.
    #[tokio::test]
    async fn a_bad_session_name_is_refused_with_400() {
        let (_tmp, _app, addr) = served().await;
        let e = dial(addr, "?session=Bad%20Name").await.expect_err("a bad name must not upgrade");
        match e {
            tokio_tungstenite::tungstenite::Error::Http(r) => assert_eq!(r.status(), 400),
            other => panic!("{other:?}"),
        }
    }

    async fn read_until(sock: &mut Sock, needle: &str) -> String {
        use futures::StreamExt as _;
        let mut seen = String::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(5), sock.next()).await {
                Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(b)))) => seen.push_str(&String::from_utf8_lossy(&b)),
                // Control frames too: an `{"exit":N}` is as much a thing to wait for as output is.
                Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t)))) => seen.push_str(&t),
                Ok(Some(Ok(_))) => {}
                _ => break,
            }
            if seen.contains(needle) {
                break;
            }
        }
        seen
    }

    async fn say(sock: &mut Sock, m: tokio_tungstenite::tungstenite::Message) {
        sock.send(m).await.unwrap();
    }

    /// A plain HTTP call against the running server: these tests share a port with their sockets,
    /// where the route tests elsewhere use `oneshot`.
    async fn http(addr: std::net::SocketAddr, method: &str, path: &str) -> (u16, Vec<u8>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("{method} {path} HTTP/1.0\r\nHost: x\r\n\r\n").as_bytes()).await.unwrap();
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).await.unwrap();
        let text = String::from_utf8_lossy(&raw).into_owned();
        let status = text.split(' ').nth(1).unwrap().parse().unwrap();
        let body = text.split_once("\r\n\r\n").map(|(_, b)| b.as_bytes().to_vec()).unwrap_or_default();
        (status, body)
    }

    /// Everything a named terminal promises, against a live server: the shell outlives its socket,
    /// a reattach replays the ring, a second attach takes the session over, the listing names it,
    /// and only a kill removes it.
    #[tokio::test]
    async fn a_named_terminal_outlives_its_socket_and_replays_on_reattach() {
        use tokio_tungstenite::tungstenite::Message;
        use_sh();
        let (_tmp, app, addr) = served().await;
        let name = format!("kl-test-{}", std::process::id());
        let mark = format!("MARK-{name}");
        let mut first = dial(addr, &format!("?session={name}")).await.unwrap();
        say(&mut first, Message::Text(r#"{"resize":{"cols":80,"rows":24}}"#.into())).await;
        say(&mut first, Message::Binary(format!("echo {mark}\n").into())).await;
        let out = read_until(&mut first, &mark).await;
        assert!(out.contains(&mark), "{out:?}");

        // The socket closing is a DETACH: the shell, and everything it has printed, outlive it.
        drop(first);
        let mut again = dial(addr, &format!("?session={name}")).await.unwrap();
        say(&mut again, Message::Text(r#"{"resize":{"cols":80,"rows":24}}"#.into())).await;
        let redraw = read_until(&mut again, &mark).await;
        assert!(redraw.contains(&mark), "the reattach replayed nothing: {redraw:?}");

        let (status, body) = http(addr, "GET", "/stream/pty/sessions").await;
        assert_eq!(status, 200);
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        let row = rows.iter().find(|r| r["name"] == name.as_str()).unwrap_or_else(|| panic!("{rows:?}"));
        assert_eq!(row["attached"].as_bool(), Some(true));
        assert!(row["created"].as_u64().unwrap() > 0 && row["pid"].as_i64().unwrap() > 0, "{row}");

        // A second attach takes the screen; the first is let go rather than left fighting for it.
        let mut third = dial(addr, &format!("?session={name}")).await.unwrap();
        say(&mut third, Message::Text(r#"{"resize":{"cols":80,"rows":24}}"#.into())).await;
        assert!(read_until(&mut third, &mark).await.contains(&mark));
        let displaced = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(Ok(m)) = again.next().await {
                // Only the detach ends this socket; an exit frame would mean the shell died.
                assert!(m.into_text().map(|t| !t.contains("exit")).unwrap_or(true));
            }
        })
        .await;
        assert!(displaced.is_ok(), "the first socket was not detached by the second attach");
        drop(third);

        // Killing a shell is the person's choice; a dropped socket never is.
        assert_eq!(http(addr, "DELETE", &format!("/stream/pty/sessions/{name}")).await.0, 204);
        assert_eq!(http(addr, "DELETE", &format!("/stream/pty/sessions/{name}")).await.0, 404);
        assert_eq!(http(addr, "DELETE", "/stream/pty/sessions/NOPE").await.0, 400);
        let (status, body) = http(addr, "GET", "/stream/pty/sessions").await;
        assert_eq!(status, 200);
        assert!(!String::from_utf8_lossy(&body).contains(&name), "{body:?}");
        // The slot goes with the session, not with the last socket that held it.
        assert_eq!(app.shells.load(Ordering::SeqCst), 0);
    }

    /// A shell that ends is the one thing that closes a named terminal on its own: the client gets
    /// its code and the table loses the name.
    #[tokio::test]
    async fn the_shells_exit_ends_the_session_and_reaches_the_client() {
        use tokio_tungstenite::tungstenite::Message;
        use_sh();
        let (_tmp, _app, addr) = served().await;
        let name = format!("kl-exit-{}", std::process::id());
        let mut sock = dial(addr, &format!("?session={name}")).await.unwrap();
        say(&mut sock, Message::Text(r#"{"resize":{"cols":80,"rows":24}}"#.into())).await;
        say(&mut sock, Message::Binary("exit 3\n".into())).await;
        let frame = read_until(&mut sock, "\"exit\"").await;
        assert!(frame.contains("\"exit\":3"), "{frame:?}");
        let (_, body) = http(addr, "GET", "/stream/pty/sessions").await;
        assert!(!String::from_utf8_lossy(&body).contains(&name), "{body:?}");
    }

    /// The pod restart path, with the restart standing in as a second server over the same
    /// workspace: the log is what carries the text, and a NEW session under a name that has one
    /// replays it before its own first prompt.
    #[tokio::test]
    async fn a_new_session_replays_the_log_a_previous_one_left() {
        use tokio_tungstenite::tungstenite::Message;
        use_sh();
        let (tmp, _app, addr) = served().await;
        let name = format!("kl-log-{}", std::process::id());
        let mark = format!("MARK-{name}");
        let mut first = dial(addr, &format!("?session={name}")).await.unwrap();
        say(&mut first, Message::Text(r#"{"resize":{"cols":80,"rows":24}}"#.into())).await;
        say(&mut first, Message::Binary(format!("echo {mark}\n").into())).await;
        assert!(read_until(&mut first, &mark).await.contains(&mark));
        // The detach is what flushes; the kill after it is this test being a good citizen.
        drop(first);
        let log = tmp.path().join(format!(".cache/shell/{name}.log"));
        let mut written = String::new();
        for _ in 0..200 {
            written = std::fs::read_to_string(&log).unwrap_or_default();
            if written.contains(&mark) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(written.contains(&mark), "the detach flushed no log: {written:?}");
        assert_eq!(http(addr, "DELETE", &format!("/stream/pty/sessions/{name}")).await.0, 204);

        // A second server over the same workspace is what a restarted pod is.
        let (_app2, addr2) = serve_at(tmp.path()).await;
        let mut back = dial(addr2, &format!("?session={name}")).await.unwrap();
        say(&mut back, Message::Text(r#"{"resize":{"cols":80,"rows":24}}"#.into())).await;
        let replay = read_until(&mut back, &mark).await;
        assert!(replay.contains(&mark), "a new session ignored the log it had: {replay:?}");
    }
}
