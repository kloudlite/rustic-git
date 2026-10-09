//! `kl-connect [team]` — straight into a bench: your own with no argument, a team's by its name.
//! It runs the laptop `kl-tui` over `kl-connect bench-proxy --tui [team]`: one session token (one
//! `bench_session` call, single-use, valid 60 s) and one websocket to the gateway's `/tui/{bench}`
//! per connection. The bench has no sshd. The token never appears in output, same rule as
//! `proxy.rs`.

use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::api::{self, SessionAnswer};
use crate::config::Config;

pub const BENCH_START_WAIT: Duration = Duration::from_secs(90);

/// `region` is the `--region` flag: it only matters for an unbound personal bench (its first use
/// binds the region), so it is sent to `create_bench` only when `team` is absent or names the
/// caller's own handle — a team's region is the team's, not this laptop's flag.
pub async fn bench(team: Option<&str>, start: bool, region: Option<&str>) -> Result<(), String> {
    let cfg = crate::config::load()?;
    // The team lands in the proxy's argv.
    if let Some(t) = team.filter(|t| !crate::sshconfig::safe_name(t)) {
        return Err(format!("team {t:?} is not a valid team name"));
    }
    if start {
        let personal = team.is_none_or(|t| t == cfg.username);
        api::create_bench(&cfg, team, region.filter(|_| personal))
            .await
            .map_err(|e| e.to_string())?;
    }
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let tui = kl_tui_beside(&me)?;
    let st = tokio::process::Command::new(&tui)
        .args(tui_argv(&me, team))
        .status()
        .await;
    // kl-tui that exited (not killed) left the alternate screen itself and printed why below it; a
    // second `?1049l` restores the cursor saved at alt-screen entry and the prompt overwrites that
    // reason (seen in tmux, 2026-10-09). Piping its stderr instead stopped it drawing at all.
    restore_terminal(!matches!(&st, Ok(s) if s.code().is_some()));
    let st = st.map_err(|e| format!("running {}: {e}", tui.display()))?;
    // exit 3 = protocol mismatch between this kl-tui and the bench: report it, never retry another way
    if st.code() == Some(3) {
        return Err("kl-tui and the bench speak different protocols: update kl-connect and kl-tui".into());
    }
    std::process::exit(st.code().unwrap_or(1));
}

/// What kl-tui is started with: it dials the bench by running `kl-connect bench-proxy --tui` as its pipe.
fn tui_argv(me: &std::path::Path, team: Option<&str>) -> Vec<String> {
    let mut v = vec!["--pipe".to_string(), me.display().to_string(), "bench-proxy".into(), "--tui".into()];
    v.extend(team.map(str::to_string));
    v
}

/// The laptop TUI ships beside kl-connect (same release, same install dir).
fn kl_tui_beside(me: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let p = me.with_file_name("kl-tui");
    if p.is_file() {
        Ok(p)
    } else {
        Err(format!("{} not found: kl-tui must be installed beside kl-connect", p.display()))
    }
}

/// Undo the modes kl-tui switches on. A killed kl-tui never runs its own cleanup, so the laptop
/// shell was left in the alternate screen with modifyOtherKeys on (Ctrl+L arriving as
/// `[27;5;108~`). Each sequence is a no-op when its mode is already off, so this runs after every
/// session, clean or not. `alt_screen: false` leaves the screen to a kl-tui that exited on its own
/// (see the caller).
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

/// `kl-connect bench-proxy [team]`: the pipe the laptop kl-tui runs (`kl-tui --pipe ...`), stdio
/// pumped to the bench daemon's TUI port through the gateway's `/tui/{bench}`.
pub async fn proxy(team: Option<&str>) -> Result<(), String> {
    let cfg = crate::config::load()?;
    serve(&cfg, team, tokio::io::stdin(), tokio::io::stdout()).await
}

/// One connection's lifetime: wait for the bench to be `Ready` (re-asking every 1 s while it
/// answers `Waking`; the state goes to stderr, which kl-tui shows), then dial its TUI door and pump.
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
    // The api hands out the /tunnel URL; the TUI door is the same gateway, same token, next route.
    let gateway = session.gateway.replacen("/tunnel/", "/tui/", 1);
    let url = crate::proxy::gateway_url(&gateway);
    let ws = crate::proxy::connect(&url, &session.token).await?;
    crate::proxy::pump_io(ws, r, w, Some(Duration::from_secs(15))).await
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
    use std::path::Path;
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

    /// What kl-tui's end of the pipe sees: one in-memory stream, its other half driven
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
    fn kl_tui_found_beside_the_exe() {
        let dir = tempfile::tempdir().unwrap();
        let me = dir.path().join("kl-connect");
        std::fs::write(&me, b"").unwrap();
        assert!(kl_tui_beside(&me).is_err());
        std::fs::write(dir.path().join("kl-tui"), b"").unwrap();
        assert_eq!(kl_tui_beside(&me), Ok(dir.path().join("kl-tui")));
    }

    #[test]
    fn the_bench_runs_kl_tui_over_the_pipe() {
        let argv = tui_argv(Path::new("/opt/kl/kl-connect"), Some("team-a"));
        assert_eq!(argv, ["--pipe", "/opt/kl/kl-connect", "bench-proxy", "--tui", "team-a"]);
    }

    #[test]
    fn a_missing_kl_tui_names_the_path() {
        let err = kl_tui_beside(Path::new("/nowhere/kl-connect")).unwrap_err();
        assert!(err.contains("/nowhere/kl-tui"), "{err}");
    }

    /// Mock api (one Ready session) plus `gw` as the gateway, env pointed at both.
    async fn harness(gw: Router) -> (Config, tokio::sync::MutexGuard<'static, ()>, tempfile::TempDir) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let guard = ENV_LOCK.lock().await;
        let counter = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/v1/bench/session", post(session_handler_ok))
            .with_state(counter)
            .merge(gw);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let d = tempfile::tempdir().unwrap();
        std::env::set_var("KL_CONFIG_DIR", d.path());
        std::env::set_var("KL_GATEWAY_OVERRIDE", format!("ws://127.0.0.1:{port}"));
        (cfg(format!("http://127.0.0.1:{port}")), guard, d)
    }

    #[tokio::test]
    async fn tui_proxy_dials_the_tui_route() {
        let hits = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let h = hits.clone();
        let gw = Router::new().route(
            "/{kind}/{bench}",
            get(move |axum::extract::Path((kind, _b)): axum::extract::Path<(String, String)>, up: WebSocketUpgrade| {
                h.lock().unwrap().push(kind);
                async move { up.on_upgrade(|mut s| async move { let _ = s.send(AxMessage::Close(None)).await; }) }
            }),
        );
        let (cfg, _g, _d) = harness(gw).await;
        let (_a, b) = tokio::io::duplex(64);
        let (r, w) = tokio::io::split(b);
        serve(&cfg, None, r, w).await.unwrap();
        std::env::remove_var("KL_GATEWAY_OVERRIDE");
        assert_eq!(*hits.lock().unwrap(), ["tui"]);
    }

    #[tokio::test]
    async fn a_refused_upgrade_names_the_status() {
        let (cfg, _g, _d) = harness(Router::new()).await; // every gateway path 404s
        let (_a, b) = tokio::io::duplex(64);
        let (r, w) = tokio::io::split(b);
        let err = serve(&cfg, None, r, w).await.unwrap_err();
        std::env::remove_var("KL_GATEWAY_OVERRIDE");
        assert!(err.contains("404"), "{err}");
    }

    #[tokio::test]
    async fn each_connection_gets_its_own_tunnel_and_token() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let _env = ENV_LOCK.lock().await;
        let counter = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/v1/bench/session", post(session_handler_ok))
            .route("/tui/bench-1", get(tunnel_handler))
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
            .route("/tui/bench-1", get(tunnel_counting))
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
