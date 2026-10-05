//! `/term/{bench}/…`: the browser terminal proxy, against a mocked API server and a fake ttyd
//! (plain HTTP for the page, a `tokio-tungstenite` echo for the WebSocket).

use kloudlite_core::jwt::Jwt;
use kloudlite_gateway::tunnel::Gateway;
use kloudlite_workspaces::kube_test::{get, mock_client, Route};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const REGION: &str = "centralindia-k3s";
const BENCH: &str = "/apis/kloudlite.io/v1alpha1/workspaces/bench-1";
const BENCH_POD: &str = "/api/v1/namespaces/ws-alice/pods/bench";

fn bench() -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1",
        "kind": "Workspace",
        "metadata": { "name": "bench-1" },
        "spec": {
            "owner": "alice", "name": "b", "region": REGION, "image": "img",
            "desiredState": "running", "bench": { "model": "m" }
        },
        "status": { "phase": "ready", "nodeName": "node-1", "podRef": "ws-alice/bench" },
    })
}

fn pod(ip: &str) -> serde_json::Value {
    serde_json::json!({ "apiVersion": "v1", "kind": "Pod", "metadata": { "name": "bench", "namespace": "ws-alice" }, "status": { "podIP": ip } })
}

fn token(bench: &str, region: &str) -> String {
    Jwt::new(SECRET).unwrap().mint_bench_session("alice", bench, region).unwrap().0
}

/// A plain HTTP server standing in for ttyd's static page: answers every GET `200 ttyd`.
async fn fake_ttyd_http() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\nttyd").await;
            });
        }
    });
    port
}

/// A WebSocket echo standing in for ttyd's `/ws`.
async fn fake_ttyd_ws() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            tokio::spawn(async move {
                use futures::{SinkExt, StreamExt};
                // Echoes ttyd's own handshake: a client that asked for the `tty` subprotocol gets
                // it confirmed, or tungstenite's client half refuses the handshake outright.
                #[allow(clippy::result_large_err)]
                let callback = |_req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                                mut resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    resp.headers_mut().insert(
                        "sec-websocket-protocol",
                        tokio_tungstenite::tungstenite::http::HeaderValue::from_static("tty"),
                    );
                    Ok(resp)
                };
                let mut ws = tokio_tungstenite::accept_hdr_async(s, callback).await.unwrap();
                while let Some(Ok(msg)) = ws.next().await {
                    if msg.is_close() {
                        break;
                    }
                    if ws.send(msg).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

async fn serve(routes: Vec<Route>, term_port: u16) -> String {
    let (client, _) = mock_client(routes);
    let gw = Arc::new(Gateway::new(Jwt::new(SECRET).unwrap(), REGION.into(), client, 22, 7789, term_port));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, kloudlite_gateway::tunnel::app(gw)).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn a_term_request_without_a_token_is_401() {
    let base = serve(vec![], 0).await;
    let res = reqwest::get(format!("{base}/term/bench-1/")).await.unwrap();
    assert_eq!(res.status(), 401);
}

#[tokio::test]
async fn a_token_for_another_bench_is_403() {
    let base = serve(vec![], 0).await;
    let t = token("bench-2", REGION);
    let res = reqwest::get(format!("{base}/term/bench-1/?token={t}")).await.unwrap();
    assert_eq!(res.status(), 403);
}

#[tokio::test]
async fn the_first_request_sets_the_cookie_and_proxies() {
    let port = fake_ttyd_http().await;
    let base = serve(vec![get(BENCH, bench()), get(BENCH_POD, pod("127.0.0.1"))], port).await;
    let t = token("bench-1", REGION);
    let res = reqwest::get(format!("{base}/term/bench-1/?token={t}")).await.unwrap();
    assert_eq!(res.status(), 200);
    let set = res.headers().get("set-cookie").unwrap().to_str().unwrap().to_string();
    assert!(set.contains(&format!("kl_term={t}")), "cookie carries the token: {set}");
    assert!(set.contains("Path=/term/bench-1/"), "cookie scoped to this bench's path: {set}");
    assert!(set.contains("HttpOnly") && set.contains("Secure") && set.contains("SameSite=Strict"));
    let body = res.text().await.unwrap();
    assert_eq!(body, "ttyd");

    // A later request rides the cookie, no query token, and still proxies.
    let client = reqwest::Client::new();
    let res2 = client
        .get(format!("{base}/term/bench-1/"))
        .header("cookie", format!("kl_term={t}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res2.status(), 200);
}

#[tokio::test]
async fn a_websocket_is_pumped_both_ways() {
    let port = fake_ttyd_ws().await;
    let base = serve(vec![get(BENCH, bench()), get(BENCH_POD, pod("127.0.0.1"))], port).await;
    let t = token("bench-1", REGION);
    let ws_base = base.replacen("http://", "ws://", 1);
    use futures::{SinkExt, StreamExt};
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("{ws_base}/term/bench-1/ws?token={t}")).await.unwrap();
    ws.send(tokio_tungstenite::tungstenite::Message::Binary(b"hello".to_vec().into())).await.unwrap();
    let reply = ws.next().await.unwrap().unwrap();
    assert_eq!(reply.into_data().as_ref(), b"hello");
}
