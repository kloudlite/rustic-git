//! `kl-connect [team]` — straight into a bench: your own with no argument, a team's by its name.
//! ssh reaches the bench through `kl-connect bench-proxy [team]` as its ProxyCommand, so there is
//! no local port and no `127.0.0.1`: each ssh gets one proxy child, one session token (one
//! `bench_session` call, single-use, valid 60 s) and one tunnel. The token never appears in
//! output, same rule as `proxy.rs`.

use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::api::{self, SessionAnswer};
use crate::config::Config;

pub const BENCH_START_WAIT: Duration = Duration::from_secs(90);

/// `region` is the `--region` flag: it only matters for an unbound personal bench (its first use
/// binds the region), so it is sent to `create_bench` only when `team` is absent or names the
/// caller's own handle — a team's region is the team's, not this laptop's flag.
pub async fn bench(team: Option<&str>, start: bool, region: Option<&str>, remote_tui: bool) -> Result<(), String> {
    let cfg = crate::config::load()?;
    // The team lands in ssh's argv and in a /bin/sh-parsed ProxyCommand.
    if let Some(t) = team.filter(|t| !crate::sshconfig::safe_name(t)) {
        return Err(format!("team {t:?} cannot be passed to ssh"));
    }
    if start {
        let personal = team.is_none_or(|t| t == cfg.username);
        api::create_bench(&cfg, team, region.filter(|_| personal))
            .await
            .map_err(|e| e.to_string())?;
    }
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let known_hosts = crate::config::dir().join("bench_known_hosts");
    let owner = team.unwrap_or(&cfg.username);
    // Ctrl+V on the bench reads THIS laptop's clipboard (clip.rs): a local socket, forwarded by
    // ssh. Spawned, not exec'd as before, because this process now serves that socket while ssh
    // runs. pid-named and short: macOS caps a socket path at 104 bytes.
    let local = crate::config::dir().join(format!("clip-{}.sock", std::process::id()));
    let listener = crate::clip::listen(&local).map_err(|e| format!("clipboard socket {}: {e}", local.display()))?;
    let clip = tokio::spawn(crate::clip::serve(listener));
    let remote_clip = crate::clip::remote_path();
    let fwd = Some((remote_clip.as_path(), local.as_path()));
    let laptop = if remote_tui { None } else { kl_tui_beside(&me) };
    let mut st = match &laptop {
        Some(tui) => {
            let mut args = vec!["--ssh".to_string()];
            args.extend(ssh_argv(&me, &known_hosts, owner, team, fwd, Some("kl-host"), false));
            tokio::process::Command::new(tui).args(args).status().await
        }
        None => Err(std::io::Error::other("no kl-tui")),
    };
    // 3 = the bench predates the laptop TUI (or speaks another protocol): run it there instead.
    let fallback = laptop.is_none() || matches!(&st, Ok(s) if s.code() == Some(3));
    if fallback {
        restore_terminal(true);
        st = tokio::process::Command::new("ssh")
            .args(ssh_argv(&me, &known_hosts, owner, team, fwd, None, true))
            .status()
            .await;
    }
    clip.abort();
    let _ = std::fs::remove_file(&local);
    // kl-tui that exited (not killed) left the alternate screen itself and printed why below it; a
    // second `?1049l` restores the cursor saved at alt-screen entry and the prompt overwrites that
    // reason (seen in tmux, 2026-10-09). Piping its stderr instead stopped it drawing at all.
    restore_terminal(fallback || !matches!(&st, Ok(s) if s.code().is_some()));
    let st = st.map_err(|e| format!("running ssh: {e}"))?;
    std::process::exit(st.code().unwrap_or(1));
}

/// The laptop TUI ships beside kl-connect (same release, same install dir); `None` means run the
/// TUI on the bench as before.
fn kl_tui_beside(me: &std::path::Path) -> Option<std::path::PathBuf> {
    let p = me.with_file_name("kl-tui");
    p.is_file().then_some(p)
}

/// Undo the modes the bench TUI switches on. A dropped connection kills it before its own cleanup
/// runs, and ssh restores only the tty's line discipline, so the laptop shell was left in the
/// alternate screen with modifyOtherKeys on (Ctrl+L arriving as `[27;5;108~`). Each sequence is a
/// no-op when its mode is already off, so this runs after every session, clean or not.
/// `alt_screen: false` leaves the screen to a kl-tui that exited on its own (see the caller).
fn restore_terminal(alt_screen: bool) {
    use std::io::{IsTerminal, Write};
    let mut out = std::io::stdout();
    if !out.is_terminal() {
        return;
    }
    // modifyOtherKeys off, kitty keyboard stack popped, mouse/focus/bracketed paste off, main
    // screen (when asked), cursor shown, colours reset.
    let _ = out.write_all(b"\x1b[>4;0m\x1b[<u\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l\x1b[?2004l");
    if alt_screen {
        let _ = out.write_all(b"\x1b[?1049l");
    }
    let _ = out.write_all(b"\x1b[?25h\x1b[0m");
    let _ = out.flush();
}

/// The ssh argv for the bench: `-t` when the remote runs the TUI itself (its ForceCommand needs a
/// pty); the laptop TUI's `kl-host` speaks frames on plain pipes, so it gets none. `HostKeyAlias` pins the key per bench
/// owner (the name ssh dials is never resolved: the ProxyCommand carries the bytes).
/// `LogLevel=ERROR` drops the one first-contact "Permanently added" line; a changed key is still
/// an error and still shown. The exe path is double-quoted, as in `ws.rs`: it can hold spaces.
/// `-R` forwards the bench-side `remote` socket to the clipboard server at `local`, and `SetEnv`
/// tells the bench's xclip shim where it is (sshd `AcceptEnv KL_CLIP`). A refused forward is not
/// fatal (ssh's default `ExitOnForwardFailure no`): the session runs, pastes find no image.
fn ssh_argv(
    me: &std::path::Path,
    known_hosts: &std::path::Path,
    owner: &str,
    team: Option<&str>,
    clip: Option<(&std::path::Path, &std::path::Path)>,
    command: Option<&str>,
    tty: bool,
) -> Vec<String> {
    let proxy = match team {
        Some(t) => format!("ProxyCommand=\"{}\" bench-proxy {t}", me.display()),
        None => format!("ProxyCommand=\"{}\" bench-proxy", me.display()),
    };
    let mut v = vec![
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".to_string(),
        "-o".to_string(),
        format!("UserKnownHostsFile={}", known_hosts.display()),
        "-o".to_string(),
        format!("HostKeyAlias=kl-bench-{owner}"),
        "-o".to_string(),
        "LogLevel=ERROR".to_string(),
        // a dead bench (pod gone, tunnel cut) sends no FIN: without probes kl-tui froze silently
        // forever. 3 missed 15 s probes end ssh, and kl-tui reports the lost connection.
        "-o".to_string(),
        "ServerAliveInterval=15".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=3".to_string(),
        "-o".to_string(),
        proxy,
    ];
    if let Some((remote, local)) = clip {
        v.extend([
            "-R".to_string(),
            format!("{}:{}", remote.display(), local.display()),
            "-o".to_string(),
            format!("SetEnv=KL_CLIP={}", remote.display()),
        ]);
    }
    if tty {
        v.push("-t".to_string());
    }
    v.push(format!("kl@bench-{owner}"));
    // The bench's ForceCommand (bench/term/login-shell) allow-lists this word.
    v.extend(command.map(String::from));
    v
}

/// `kl-connect claude login`: the bench's Claude Code signs in over the same ssh path as the TUI,
/// minus the clipboard forward (nothing to paste an image into). The bench is started first, as
/// `--start` does, so a sleeping one wakes before ssh dials it.
pub async fn claude_login(team: Option<&str>) -> Result<(), String> {
    let cfg = crate::config::load()?;
    if let Some(t) = team.filter(|t| !crate::sshconfig::safe_name(t)) {
        return Err(format!("team {t:?} cannot be passed to ssh"));
    }
    // No --region here: the region flag belongs to `kl-connect --start`, so personal or team alike
    // sends none.
    api::create_bench(&cfg, team, None)
        .await
        .map_err(|e| e.to_string())?;
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let known_hosts = crate::config::dir().join("bench_known_hosts");
    let owner = team.unwrap_or(&cfg.username);
    println!(
        "Signing in Claude Code on bench-{owner}. Open the URL it prints, sign in, paste the code back here."
    );
    let st = tokio::process::Command::new("ssh")
        .args(ssh_argv(&me, &known_hosts, owner, team, None, Some("claude-login"), true))
        .status()
        .await;
    restore_terminal(true);
    let st = st.map_err(|e| format!("running ssh: {e}"))?;
    std::process::exit(st.code().unwrap_or(1));
}

/// `kl-connect bench-proxy [team]`, ssh's ProxyCommand: pumps this process's stdio to the bench.
pub async fn proxy(team: Option<&str>) -> Result<(), String> {
    let cfg = crate::config::load()?;
    serve(&cfg, team, tokio::io::stdin(), tokio::io::stdout()).await
}

/// One connection's lifetime: wait for the bench to be `Ready` (re-asking every 1 s while it
/// answers `Waking`; the state goes to stderr, which ssh shows), then dial its tunnel and pump.
/// `r` is never read before the pump starts — bytes written while the bench slept still arrive.
async fn serve<R, W>(cfg: &Config, team: Option<&str>, r: R, w: W) -> Result<(), String>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
{
    let started = tokio::time::Instant::now();
    let deadline = started + BENCH_START_WAIT;
    let wait = Progress::start(team);
    let session = loop {
        match api::bench_session(cfg, team).await {
            Ok(SessionAnswer::Ready(s)) => break s,
            Ok(SessionAnswer::Waking(state)) => {
                if tokio::time::Instant::now() >= deadline {
                    wait.stop(false).await;
                    return Err("bench did not start within 90 s".to_string());
                }
                wait.state(&state);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => {
                wait.stop(false).await;
                return Err(e.to_string());
            }
        }
    };
    wait.stop(true).await;
    let url = crate::proxy::gateway_url(&session.gateway);
    let ws = crate::proxy::connect(&url, &session.token).await?;
    crate::proxy::pump_io(ws, r, w).await
}

/// The wait on stderr, which ssh hands straight to the terminal (it is not in raw mode until the
/// session starts). On a terminal: one spinner line redrawn in place every 100 ms by its own task,
/// so it moves even while an api call is slow, naming the phase in plain words and the time so
/// far. Piped: one line per phase change.
struct Progress {
    label: tokio::sync::watch::Sender<String>,
    ticker: Option<tokio::task::JoinHandle<()>>,
    whose: String,
    started: tokio::time::Instant,
    waited: std::cell::Cell<bool>,
}

impl Progress {
    fn start(team: Option<&str>) -> Self {
        use std::io::IsTerminal;
        let whose = team.map_or("your bench".to_string(), |t| format!("{t}'s bench"));
        let (label, rx) = tokio::sync::watch::channel(format!("Connecting to {whose}"));
        let started = tokio::time::Instant::now();
        let ticker = std::io::stderr().is_terminal().then(|| tokio::spawn(spin(rx, started)));
        Progress { label, ticker, whose, started, waited: std::cell::Cell::new(false) }
    }

    fn state(&self, state: &str) {
        let label = phase_label(state, &self.whose);
        self.waited.set(true);
        if *self.label.borrow() != label {
            if self.ticker.is_none() {
                eprintln!("{label}");
            }
            self.label.send_replace(label);
        }
    }

    /// Clears the line; after a real wait that ended in a session, says so.
    /// Waits out the aborted ticker first: on a multi-threaded runtime it could otherwise draw
    /// once more after the clear and leave a stale spinner line above the session.
    async fn stop(mut self, ready: bool) {
        if let Some(t) = self.ticker.take() {
            t.abort();
            let _ = t.await;
            eprint!("\r\x1b[2K");
        }
        if ready && self.waited.get() {
            eprintln!("✓ {} is ready ({}s)", capitalise(&self.whose), self.started.elapsed().as_secs());
        }
    }
}

async fn spin(label: tokio::sync::watch::Receiver<String>, started: tokio::time::Instant) {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    // Silent for the first 300 ms: an awake bench connects before anything is drawn.
    tokio::time::sleep(Duration::from_millis(300)).await;
    for f in FRAMES.iter().cycle() {
        eprint!("\r\x1b[2K{f} {}… {}s", *label.borrow(), started.elapsed().as_secs());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The api's phase (`crd::Phase::as_str`, or `waking`) as a person would say it.
fn phase_label(state: &str, whose: &str) -> String {
    match state {
        "waking" | "idle" => format!("Waking {whose}"),
        "pending" => format!("Finding a node for {whose}"),
        "creating" => format!("Creating {whose}"),
        "starting" => format!("Starting {whose}"),
        "unavailable" => format!("The node holding {whose} is unreachable; waiting"),
        other => format!("{} is {other}; waiting", capitalise(whose)),
    }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::ws::{Message as AxMessage, WebSocketUpgrade};
    use axum::extract::State;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[test]
    fn phases_read_as_plain_words() {
        assert_eq!(phase_label("waking", "your bench"), "Waking your bench");
        assert_eq!(phase_label("creating", "acme's bench"), "Creating acme's bench");
        assert_eq!(phase_label("error", "your bench"), "Your bench is error; waiting");
    }

    // Shared with config.rs's tests: KL_CONFIG_DIR etc. are process-global.
    use crate::config::ENV_LOCK;

    fn cfg(api: String) -> Config {
        Config {
            api,
            token: "t".into(),
            expires_at: "2030".into(),
            username: "k".into(),
        }
    }

    /// What ssh's end of the ProxyCommand pipe sees: one in-memory stream, its other half driven
    /// by `serve` the way `proxy` drives stdin/stdout.
    fn connection(cfg: Config) -> tokio::io::DuplexStream {
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        let (r, w) = tokio::io::split(theirs);
        tokio::spawn(async move {
            if let Err(e) = serve(&cfg, None, r, w).await {
                panic!("serve: {e}");
            }
        });
        ours
    }

    async fn session_handler_ok(State(counter): State<Arc<AtomicUsize>>) -> impl IntoResponse {
        counter.fetch_add(1, Ordering::SeqCst);
        axum::Json(serde_json::json!({
            "id": "bench-1", "token": "tok", "gateway": "wss://x/tunnel/bench-1", "expires_at": "2030"
        }))
        .into_response()
    }

    async fn tunnel_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
        ws.on_upgrade(|mut socket| async move {
            while let Some(Ok(m)) = socket.recv().await {
                if let AxMessage::Binary(b) = m {
                    let _ = socket.send(AxMessage::Binary(b)).await;
                }
            }
        })
    }

    #[test]
    fn ssh_argv_matches_the_bench_ssh_invocation() {
        let me = std::path::Path::new("/opt/kl connect/kl-connect");
        let kh = std::path::Path::new("/home/k/.config/kl-connect/bench_known_hosts");
        let remote = std::path::Path::new("/tmp/kl-clip-1.sock");
        let local = std::path::Path::new("/home/k/.config/kl-connect/clip-1.sock");
        let tail = |owner: &str, proxy: &str| {
            vec![
                "-o".to_string(),
                "StrictHostKeyChecking=accept-new".to_string(),
                "-o".to_string(),
                "UserKnownHostsFile=/home/k/.config/kl-connect/bench_known_hosts".to_string(),
                "-o".to_string(),
                format!("HostKeyAlias=kl-bench-{owner}"),
                "-o".to_string(),
                "LogLevel=ERROR".to_string(),
                "-o".to_string(),
                "ServerAliveInterval=15".to_string(),
                "-o".to_string(),
                "ServerAliveCountMax=3".to_string(),
                "-o".to_string(),
                proxy.to_string(),
                "-R".to_string(),
                "/tmp/kl-clip-1.sock:/home/k/.config/kl-connect/clip-1.sock".to_string(),
                "-o".to_string(),
                "SetEnv=KL_CLIP=/tmp/kl-clip-1.sock".to_string(),
                "-t".to_string(),
                format!("kl@bench-{owner}"),
            ]
        };
        assert_eq!(
            ssh_argv(me, kh, "k", None, Some((remote, local)), None, true),
            tail("k", "ProxyCommand=\"/opt/kl connect/kl-connect\" bench-proxy")
        );
        assert_eq!(
            ssh_argv(me, kh, "acme", Some("acme"), Some((remote, local)), None, true),
            tail("acme", "ProxyCommand=\"/opt/kl connect/kl-connect\" bench-proxy acme")
        );
    }

    #[test]
    fn login_argv_has_no_clipboard_forward_and_ends_with_the_command() {
        let me = std::path::Path::new("/bin/kl-connect");
        let kh = std::path::Path::new("/k/known");
        let a = ssh_argv(me, kh, "acme", Some("acme"), None, Some("claude-login"), true);
        assert!(!a.iter().any(|x| x == "-R" || x.starts_with("SetEnv")));
        assert!(a.contains(&"-t".to_string()));
        assert_eq!(a[a.len() - 2..], ["kl@bench-acme", "claude-login"]);
    }

    #[test]
    fn ssh_argv_tty_is_optional() {
        let p = std::path::Path::new("/x/kl-connect");
        let k = std::path::Path::new("/x/kh");
        let with = ssh_argv(p, k, "me", None, None, None, true);
        let without = ssh_argv(p, k, "me", None, None, Some("kl-host"), false);
        assert!(with.contains(&"-t".to_string()));
        assert!(!without.contains(&"-t".to_string()));
        assert_eq!(without.last().unwrap(), "kl-host");
    }

    #[test]
    fn kl_tui_found_beside_the_exe() {
        let dir = tempfile::tempdir().unwrap();
        let me = dir.path().join("kl-connect");
        std::fs::write(&me, b"").unwrap();
        assert!(kl_tui_beside(&me).is_none());
        std::fs::write(dir.path().join("kl-tui"), b"").unwrap();
        assert_eq!(kl_tui_beside(&me), Some(dir.path().join("kl-tui")));
    }

    #[tokio::test]
    async fn each_connection_gets_its_own_tunnel_and_token() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let _env = ENV_LOCK.lock().await;
        let counter = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/v1/bench/session", post(session_handler_ok))
            .route("/tunnel/bench-1", get(tunnel_handler))
            .with_state(counter.clone());
        let app_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app_port = app_listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(app_listener, app).await.unwrap() });

        let d = tempfile::tempdir().unwrap();
        std::env::set_var("KL_CONFIG_DIR", d.path());
        std::env::set_var("KL_GATEWAY_OVERRIDE", format!("ws://127.0.0.1:{app_port}"));

        let cfg = cfg(format!("http://127.0.0.1:{app_port}"));

        for byte in *b"ab" {
            let mut s = connection(cfg.clone());
            s.write_all(&[byte]).await.unwrap();
            let mut buf = [0u8; 1];
            s.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf[0], byte);
        }
        assert_eq!(counter.load(Ordering::SeqCst), 2);
        std::env::remove_var("KL_GATEWAY_OVERRIDE");
    }

    #[tokio::test]
    async fn a_sleeping_bench_is_waited_for_and_the_early_bytes_arrive() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let _env = ENV_LOCK.lock().await;
        let counter = Arc::new(AtomicUsize::new(0));
        let upgrades = Arc::new(AtomicUsize::new(0));

        async fn waking_then_ready(
            State((counter, upgrades)): State<(Arc<AtomicUsize>, Arc<AtomicUsize>)>,
        ) -> impl IntoResponse {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let _ = &upgrades;
            if n < 2 {
                (
                    axum::http::StatusCode::ACCEPTED,
                    axum::Json(serde_json::json!({"state": "waking"})),
                )
                    .into_response()
            } else {
                (
                    axum::http::StatusCode::CREATED,
                    axum::Json(serde_json::json!({
                        "id": "bench-1", "token": "tok", "gateway": "wss://x/tunnel/bench-1", "expires_at": "2030"
                    })),
                )
                    .into_response()
            }
        }

        async fn tunnel_counting(
            ws: WebSocketUpgrade,
            State((_, upgrades)): State<(Arc<AtomicUsize>, Arc<AtomicUsize>)>,
        ) -> impl IntoResponse {
            upgrades.fetch_add(1, Ordering::SeqCst);
            ws.on_upgrade(|mut socket| async move {
                while let Some(Ok(m)) = socket.recv().await {
                    if let AxMessage::Binary(b) = m {
                        let _ = socket.send(AxMessage::Binary(b)).await;
                    }
                }
            })
        }

        let app = Router::new()
            .route("/v1/bench/session", post(waking_then_ready))
            .route("/tunnel/bench-1", get(tunnel_counting))
            .with_state((counter.clone(), upgrades.clone()));
        let app_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app_port = app_listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(app_listener, app).await.unwrap() });

        let d = tempfile::tempdir().unwrap();
        std::env::set_var("KL_CONFIG_DIR", d.path());
        std::env::set_var("KL_GATEWAY_OVERRIDE", format!("ws://127.0.0.1:{app_port}"));

        let cfg = cfg(format!("http://127.0.0.1:{app_port}"));

        let fut = async {
            let mut s = connection(cfg);
            s.write_all(b"early").await.unwrap();
            let mut buf = [0u8; 5];
            s.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"early");
        };
        tokio::time::timeout(Duration::from_secs(5), fut).await.expect("timed out");

        assert_eq!(counter.load(Ordering::SeqCst), 3);
        assert_eq!(upgrades.load(Ordering::SeqCst), 1);
        std::env::remove_var("KL_GATEWAY_OVERRIDE");
    }
}
