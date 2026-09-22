//! `kl-connect bench` — the bench's local gate, on the laptop. It binds one local port, and every
//! accepted TCP connection gets its own session token (one `bench_session` call, single-use,
//! valid 60 s) and its own tunnel: no connection is multiplexed onto another's. The token never
//! appears in output, same rule as `proxy.rs`.

use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

use crate::api::{self, SessionAnswer};
use crate::config::Config;

pub const BENCH_START_WAIT: Duration = Duration::from_secs(90);

/// `region` is the `--region` flag: it only matters for an unbound personal bench (its first use
/// binds the region), so it is sent to `create_bench` only when `team` is absent or names the
/// caller's own handle — a team's region is the team's, not this laptop's flag.
pub async fn bench(team: Option<&str>, port: u16, start: bool, region: Option<&str>) -> Result<(), String> {
    let cfg = crate::config::load()?;
    if start {
        let personal = team.is_none_or(|t| t == cfg.username);
        api::create_bench(&cfg, team, region.filter(|_| personal))
            .await
            .map_err(|e| e.to_string())?;
    }
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| e.to_string())?;
    println!("127.0.0.1:{}", listener.local_addr().map_err(|e| e.to_string())?.port());
    use std::io::Write;
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    bench_on(listener, cfg, team.map(str::to_string)).await;
    Ok(())
}

/// The testable core: takes a pre-bound listener (and no stdout line) so a test needs no port
/// scraping. Never returns on its own — the listener is served until the process exits, and a
/// per-connection error is logged and the loop continues.
async fn bench_on(listener: TcpListener, cfg: Config, team: Option<String>) {
    loop {
        let (sock, _) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                eprintln!("kl-connect: bench: accept: {e}");
                continue;
            }
        };
        let cfg = cfg.clone();
        let team = team.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_conn(sock, &cfg, team.as_deref()).await {
                if e.starts_with("your login has expired") {
                    eprintln!("kl-connect: {e}");
                    std::process::exit(1);
                }
                eprintln!("kl-connect: bench: {e}");
            }
        });
    }
}

/// One local connection's lifetime: wait for the bench to be `Ready` (re-asking every 1 s while
/// it answers `Waking`), then dial its tunnel and pump. The socket's bytes are never read before
/// the pump starts — a client that wrote while the bench slept still gets it delivered.
async fn serve_conn(sock: TcpStream, cfg: &Config, team: Option<&str>) -> Result<(), String> {
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
    let (r, w) = sock.into_split();
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

    use crate::test_env::ENV;

    fn cfg(api: String) -> Config {
        Config {
            api,
            token: "t".into(),
            expires_at: "2030".into(),
            username: "k".into(),
        }
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

    #[tokio::test]
    async fn each_local_connection_gets_its_own_tunnel_and_token() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let _env = ENV.lock().await;
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

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cfg = cfg(format!("http://127.0.0.1:{app_port}"));
        tokio::spawn(bench_on(listener, cfg, None));

        for byte in *b"ab" {
            let mut s = TcpStream::connect(addr).await.unwrap();
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
        let _env = ENV.lock().await;
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

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cfg = cfg(format!("http://127.0.0.1:{app_port}"));
        tokio::spawn(bench_on(listener, cfg, None));

        let fut = async {
            let mut s = TcpStream::connect(addr).await.unwrap();
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
