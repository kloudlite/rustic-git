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
pub async fn bench(team: Option<&str>, start: bool, region: Option<&str>) -> Result<(), String> {
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
    let st = tokio::process::Command::new("ssh")
        .args(ssh_argv(&me, &known_hosts, owner, team, &crate::clip::remote_path(), &local))
        .status()
        .await;
    clip.abort();
    let _ = std::fs::remove_file(&local);
    let st = st.map_err(|e| format!("running ssh: {e}"))?;
    std::process::exit(st.code().unwrap_or(1));
}

/// The ssh argv for the bench: `-t` because the remote's `ForceCommand` is the graphcode TUI, not
/// a one-shot command, so ssh must allocate a pty for it. `HostKeyAlias` pins the key per bench
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
    remote: &std::path::Path,
    local: &std::path::Path,
) -> Vec<String> {
    let proxy = match team {
        Some(t) => format!("ProxyCommand=\"{}\" bench-proxy {t}", me.display()),
        None => format!("ProxyCommand=\"{}\" bench-proxy", me.display()),
    };
    vec![
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".to_string(),
        "-o".to_string(),
        format!("UserKnownHostsFile={}", known_hosts.display()),
        "-o".to_string(),
        format!("HostKeyAlias=kl-bench-{owner}"),
        "-o".to_string(),
        "LogLevel=ERROR".to_string(),
        "-o".to_string(),
        proxy,
        "-R".to_string(),
        format!("{}:{}", remote.display(), local.display()),
        "-o".to_string(),
        format!("SetEnv=KL_CLIP={}", remote.display()),
        "-t".to_string(),
        format!("kl@bench-{owner}"),
    ]
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
    let deadline = tokio::time::Instant::now() + BENCH_START_WAIT;
    let mut last_state: Option<String> = None;
    let session = loop {
        match api::bench_session(cfg, team).await {
            Ok(SessionAnswer::Ready(s)) => break s,
            Ok(SessionAnswer::Waking(state)) => {
                if last_state.as_deref() != Some(state.as_str()) {
                    eprintln!("bench is {state}; waiting");
                    last_state = Some(state);
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err("bench did not start within 90 s".to_string());
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let url = crate::proxy::gateway_url(&session.gateway);
    let ws = crate::proxy::connect(&url, &session.token).await?;
    crate::proxy::pump_io(ws, r, w).await
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
            ssh_argv(me, kh, "k", None, remote, local),
            tail("k", "ProxyCommand=\"/opt/kl connect/kl-connect\" bench-proxy")
        );
        assert_eq!(
            ssh_argv(me, kh, "acme", Some("acme"), remote, local),
            tail("acme", "ProxyCommand=\"/opt/kl connect/kl-connect\" bench-proxy acme")
        );
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
