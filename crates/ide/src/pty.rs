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
//! A `?session=<name>` runs `tmux -L kl new-session -A -s <name>` under the terminal instead of a
//! bare login shell, so a dropped socket detaches a client rather than hanging a shell up and a
//! reconnect with the same name lands in the same session with tmux redrawing its scrollback.
//! Without a name the route keeps the one-shot shell, which is what `exec`-style callers and the
//! probe want. `/stream/pty/sessions` lists and `DELETE /stream/pty/sessions/{name}` kills; the
//! server itself is the workspace container's, saved by tmux-resurrect into `{ws}/.cache/tmux`.
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

/// The tmux server every named terminal lives on. One per pod, named rather than the default so
/// nothing a person starts by hand shares it.
pub const SOCKET: &str = "kl";

/// `[a-z0-9-]{1,48}`. The desktop names sessions (`kl-<tab>-<n>`); the server never invents one
/// and never passes anything else to tmux, where a name is also a shell-free but `-t`-matched
/// target.
pub fn valid_session(name: &str) -> bool {
    let body = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-';
    // Not a leading dash: the name is an argument to `-s`/`-t`, and one that looks like a flag is
    // a trap waiting for the next tmux version to parse it as one.
    name.len() <= 48 && name.bytes().next().is_some_and(|b| body(&b) && b != b'-') && name.bytes().all(|b| body(&b))
}

/// `tmux` as the profile puts it on PATH. Resolved in the parent: between fork and exec only
/// async-signal-safe calls are sound, and `execvp` would search PATH inside the child.
fn which(bin: &str) -> Option<CString> {
    for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
        let c = CString::new(dir.join(bin).as_os_str().as_encoded_bytes()).ok()?;
        if unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0 {
            return Some(c);
        }
    }
    None
}

/// What the terminal runs, in the order to try it: one tmux attach-or-create, or every candidate
/// login shell. `-A` is what makes a reconnect an attach; `-x`/`-y` start the session at the
/// client's size so nothing reflows from 80x24 on the first redraw.
fn program(root: &Path, cols: u16, rows: u16, session: Option<&str>) -> io::Result<Vec<(CString, Vec<CString>)>> {
    let Some(name) = session else {
        return Ok(candidates().into_iter().map(|sh| { let a0 = login_argv0(&sh); (sh, vec![a0]) }).collect());
    };
    if !valid_session(name) {
        return Err(io::Error::other("bad session name"));
    }
    let tmux = which("tmux").ok_or_else(|| io::Error::other("tmux is not on PATH"))?;
    let args = ["tmux".to_string(), "-L".into(), SOCKET.into(), "new-session".into(), "-A".into(), "-s".into(), name.into(), "-x".into(), cols.to_string(), "-y".into(), rows.to_string(), "-c".into(), root.to_string_lossy().into_owned()];
    let argv = args.into_iter().map(|a| CString::new(a).map_err(|_| io::Error::other("a NUL in the tmux argv"))).collect::<io::Result<Vec<_>>>()?;
    Ok(vec![(tmux, argv)])
}

/// What `kl ide serve` was started with is NOT what an ssh login gets: the prelude starts it
/// through `su kl -s /bin/sh`, and the pod env it inherits is only half the story. Three groups
/// go, one comes back.
///
/// `SHELL` is the load-bearing one. `su -s /bin/sh` leaves it `/bin/sh`, and this process passes
/// it down; sshd instead sets it from the passwd entry. `spawn` already execs the passwd shell
/// (that was the first half of this bug — every PTY landed in busybox ash while ssh got the Nix
/// zsh, so the prompt was ash's `dir $` with no rc and no starship), but anything the person's
/// shell RESPAWNS reads `$SHELL` — above all tmux, whose `default-shell` defaults to it, which
/// would have put every named terminal straight back in ash one layer down.
///
/// `TERM`/`COLORTERM` are ours to state: this end is a real terminal whatever the server's env
/// says. `TMUX` is only ever set when this process is itself inside a session (a dev machine,
/// never a pod), where tmux refuses to nest without `-d`.
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

pub fn spawn(root: &Path, cols: u16, rows: u16, session: Option<&str>) -> io::Result<Pty> {
    // Before the pty is opened, so a refusal leaks no descriptors.
    let attempts = program(root, cols, rows, session)?;
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

#[derive(serde::Deserialize)]
pub struct Query {
    /// The tmux session to attach to or create. Absent = today's one-shot login shell.
    session: Option<String>,
}

pub async fn handler(State(app): State<Arc<App>>, axum::extract::Query(q): axum::extract::Query<Query>, ws: WebSocketUpgrade) -> Response {
    // A bad name never reaches tmux, and it is a request error rather than something to print
    // into a terminal: the desktop, not a person, chooses these.
    if q.session.as_deref().is_some_and(|n| !valid_session(n)) {
        return (axum::http::StatusCode::BAD_REQUEST, "session must match [a-z0-9-]{1,48}").into_response();
    }
    let Some(slot) = Slot::take(&app) else {
        // Refused after the upgrade, not as an HTTP status: the tab is already a terminal and has
        // nowhere but the socket to print why.
        return ws.on_upgrade(|sock| fail(sock, "too many shells".into())).into_response();
    };
    let root = app.cfg.root.clone();
    ws.on_upgrade(move |sock| pump(sock, root, q.session, slot)).into_response()
}

/// `tmux -L kl <args>`, with the workspace's own environment. Every session route is one of
/// these: the tmux server is the state, so there is nothing to keep in this process.
async fn tmux(args: &[&str]) -> std::io::Result<std::process::Output> {
    tokio::process::Command::new("tmux").arg("-L").arg(SOCKET).args(args).output().await
}

/// `GET /stream/pty/sessions` → `[{name, windows, attached, created}]`, and an empty list when no
/// server is running: another device shows the same terminals and reattaches them by name.
pub async fn sessions() -> axum::Json<Vec<serde_json::Value>> {
    const FORMAT: &str = "#{session_name} #{session_windows} #{session_attached} #{session_created}";
    let out = match tmux(&["ls", "-F", FORMAT]).await {
        // No server (nothing started yet, or the last session ended) is an empty list, not a fault.
        Ok(o) if o.status.success() => o.stdout,
        _ => return axum::Json(Vec::new()),
    };
    let rows = String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| {
            let mut f = l.split(' ');
            let name = f.next()?.to_string();
            let n = |f: &mut std::str::Split<char>| f.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            Some(serde_json::json!({ "name": name, "windows": n(&mut f), "attached": n(&mut f) > 0, "created": n(&mut f) }))
        })
        .collect();
    axum::Json(rows)
}

/// `DELETE /stream/pty/sessions/{name}`: killing a shell is the person's choice, which a dropped
/// socket never is — so it is a route of its own and not something the detach does.
pub async fn kill_session(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    if !valid_session(&name) {
        return (axum::http::StatusCode::BAD_REQUEST, "session must match [a-z0-9-]{1,48}").into_response();
    }
    match tmux(&["kill-session", "-t", &name]).await {
        Ok(o) if o.status.success() => axum::http::StatusCode::NO_CONTENT.into_response(),
        // tmux says "can't find session" for an unknown name and the same nonzero exit for no
        // server at all; both mean the terminal the caller named is gone.
        Ok(_) => axum::http::StatusCode::NOT_FOUND.into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Restore what tmux-resurrect last saved into `{root}/.cache/tmux/resurrect`, once, before the
/// first socket arrives — so after a pod stop, a move or a clone the same terminal names come
/// back with their layout, cwd and pane text. The processes do not; no tool can bring those back.
/// Never fatal: a workspace with no save, no tmux or a broken plugin still serves terminals.
pub async fn restore(root: &Path) {
    let dir = root.join(".cache/tmux/resurrect");
    // tmux-resurrect writes `last` as a symlink to the newest save file; no save, nothing to do.
    // `symlink_metadata`: a dangling link is still a save it should try, and `exists` follows.
    if dir.join("last").symlink_metadata().is_err() {
        return;
    }
    let profile = std::env::var("NIX_PROFILE").unwrap_or_else(|_| "/nix/profile/current".into());
    let script = format!("{profile}/share/tmux-plugins/resurrect/scripts/restore.sh");
    let set = format!("set -g @resurrect-dir {}", dir.display());
    if let Err(e) = tmux(&["start-server", ";", &set, ";", "run-shell", &script]).await {
        tracing::warn!(error = %e, "tmux.restore.failed");
        return;
    }
    let n = match tmux(&["ls", "-F", "#{session_name}"]).await {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).lines().count(),
        _ => 0,
    };
    tracing::info!(sessions = n, "tmux.restored");
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

async fn pump(sock: WebSocket, root: std::path::PathBuf, session: Option<String>, _slot: Slot) {
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

    let mut pty = match spawn(&root, size.0, size.1, session.as_deref()) {
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
        let mut pty = spawn(tmp.path(), 80, 24, None).unwrap();
        pty.write_all(b"printf kl-%s ok\\n; exit 3\n").await.unwrap();
        let out = drain(&pty).await;
        assert!(out.contains("kl-ok"), "{out:?}");
        assert_eq!(pty.wait(), Some(3));
    }

    #[tokio::test]
    async fn a_resize_reaches_the_shell_as_a_window_size() {
        use_sh();
        let tmp = tempfile::tempdir().unwrap();
        let mut pty = spawn(tmp.path(), 80, 24, None).unwrap();
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
        let pty = spawn(tmp.path(), 80, 24, None).unwrap();
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
        let home = tmp.path().canonicalize().unwrap();
        let app = Arc::new(App::new(Config { bind: "127.0.0.1:0".parse().unwrap(), root: home.clone(), home, graft_dir: None }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = crate::server::router(app.clone());
        tokio::spawn(async move { axum::serve(listener, served).await });
        (tmp, app, addr)
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

    /// tmux comes from the Nix profile in a workspace; this machine may not have one, and nothing
    /// here may install it — so the tmux tests say so and pass rather than fail on a precondition
    /// that has nothing to do with the code.
    fn tmux_here() -> bool {
        if which("tmux").is_some() {
            // One private tmux server for the whole test binary, so a run never touches (or is
            // confused by) whatever the person running it has open. Process-wide, like `SHELL`.
            static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
            std::env::set_var("TMUX_TMPDIR", DIR.get_or_init(|| tempfile::tempdir().unwrap()).path());
            return true;
        }
        eprintln!("skipping: tmux is not on PATH");
        false
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

    /// The whole named-terminal contract against a live server: the session exists under
    /// `tmux -L kl`, a second socket with the same name ATTACHES (tmux redraws what the first left
    /// on screen instead of starting a new shell), the listing shows it, and only a kill removes it.
    #[tokio::test]
    async fn a_named_terminal_is_a_tmux_session_a_second_socket_reattaches() {
        use tokio_tungstenite::tungstenite::Message;
        if !tmux_here() {
            return;
        }
        let (_tmp, _app, addr) = served().await;
        let name = format!("kl-test-{}", std::process::id());
        let mark = format!("MARK-{name}");
        let mut first = dial(addr, &format!("?session={name}")).await.unwrap();
        say(&mut first, Message::Text(r#"{"resize":{"cols":80,"rows":24}}"#.into())).await;
        say(&mut first, Message::Binary(format!("echo {mark}\n").into())).await;
        let out = read_until(&mut first, &mark).await;
        assert!(out.contains(&mark), "{out:?}");

        // Polled: tmux forks its server, so the socket exists a moment after the shell inside it
        // has already answered.
        let mut listed = String::new();
        for _ in 0..100 {
            listed = String::from_utf8_lossy(&tmux(&["ls", "-F", "#{session_name}"]).await.unwrap().stdout).into_owned();
            if listed.lines().any(|l| l == name) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(listed.lines().any(|l| l == name), "{listed:?}");

        // The socket closing is a DETACH: the session, and what is on its screen, outlive it.
        drop(first);
        let mut again = dial(addr, &format!("?session={name}")).await.unwrap();
        say(&mut again, Message::Text(r#"{"resize":{"cols":80,"rows":24}}"#.into())).await;
        let redraw = read_until(&mut again, &mark).await;
        assert!(redraw.contains(&mark), "the reattach redrew nothing: {redraw:?}");

        let (status, body) = http(addr, "GET", "/stream/pty/sessions").await;
        assert_eq!(status, 200);
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        let row = rows.iter().find(|r| r["name"] == name.as_str()).unwrap_or_else(|| panic!("{rows:?}"));
        assert_eq!((row["windows"].as_u64(), row["attached"].as_bool()), (Some(1), Some(true)));
        assert!(row["created"].as_u64().unwrap() > 0, "{row}");
        drop(again);

        // Killing a shell is the person's choice; a dropped socket never is.
        assert_eq!(http(addr, "DELETE", &format!("/stream/pty/sessions/{name}")).await.0, 204);
        assert_eq!(http(addr, "DELETE", &format!("/stream/pty/sessions/{name}")).await.0, 404);
        assert_eq!(http(addr, "DELETE", "/stream/pty/sessions/NOPE").await.0, 400);
        // `exit-empty` takes the server with the last session, so this is also the no-server
        // path: a failed `tmux ls` is an empty list with a 200, which is what the desktop asks
        // for on every tab before anything has been opened.
        let (status, body) = http(addr, "GET", "/stream/pty/sessions").await;
        assert_eq!(status, 200);
        assert!(!String::from_utf8_lossy(&body).contains(&name), "{body:?}");
    }

}
