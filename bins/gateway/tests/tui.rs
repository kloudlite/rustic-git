//! `/tui/{bench}`: the laptop kl-tui's path to the bench daemon's TCP listener, no ssh. Same
//! token, reserve, spend and pump as `/tunnel`; only a bench-session token is accepted.
use futures::{SinkExt, StreamExt};
use kloudlite_core::jwt::Jwt;
use kloudlite_gateway::tunnel::Gateway;
use kloudlite_workspaces::kube_test::{get, mock_client};
use std::sync::Arc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const REGION: &str = "centralindia-k3s";
const BENCH_PATH: &str = "/apis/kloudlite.io/v1alpha1/workspaces/bench-1";
const BENCH_POD: &str = "/api/v1/namespaces/ws-alice/pods/bench";

fn jwt() -> Jwt {
    Jwt::new(SECRET).unwrap()
}

async fn echo() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    port
}

/// Serves the gateway with the TUI port at `tui` and every other port at a closed one, so a
/// pump to the wrong port is a 502, not a silent pass.
async fn serve(tui: u16) -> std::net::SocketAddr {
    let bench = serde_json::json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace",
        "metadata": { "name": "bench-1" },
        "spec": { "owner": "alice", "name": "b", "region": REGION, "image": "img",
                  "desiredState": "running", "bench": { "model": "m" } },
        "status": { "phase": "ready", "nodeName": "node-1", "podRef": "ws-alice/bench" },
    });
    let pod = serde_json::json!({ "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": "bench", "namespace": "ws-alice" }, "status": { "podIP": "127.0.0.1" } });
    let (client, _) = mock_client(vec![get(BENCH_PATH, bench), get(BENCH_POD, pod)]);
    let gw = Arc::new(Gateway::new(jwt(), REGION.into(), client, 1, 1, 1, tui));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, kloudlite_gateway::tunnel::app(gw)).await.unwrap() });
    addr
}

fn req(addr: std::net::SocketAddr, token: &str) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut r = format!("ws://{addr}/tui/bench-1").into_client_request().unwrap();
    r.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
    r
}

#[tokio::test]
async fn tui_route_pumps_to_the_tui_port() {
    let addr = serve(echo().await).await;
    let (token, _) = jwt().mint_bench_session("alice", "bench-1", REGION).unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(req(addr, &token)).await.unwrap();
    ws.send(Message::Binary(b"hello\n".to_vec().into())).await.unwrap();
    let Some(Ok(Message::Binary(b))) = ws.next().await else { panic!("no echo") };
    assert_eq!(&b[..], b"hello\n");
}

#[tokio::test]
async fn a_workspace_token_is_refused_on_the_tui_route() {
    let addr = serve(echo().await).await;
    let (token, _) = jwt().mint_ssh_session("alice", "bench-1", REGION).unwrap();
    let err = tokio_tungstenite::connect_async(req(addr, &token)).await.unwrap_err();
    assert!(err.to_string().contains("401"), "{err}");
}

#[tokio::test]
async fn a_spent_token_is_refused_on_the_tui_route() {
    let addr = serve(echo().await).await;
    let (token, _) = jwt().mint_bench_session("alice", "bench-1", REGION).unwrap();
    let first = tokio_tungstenite::connect_async(req(addr, &token)).await;
    assert!(first.is_ok());
    let err = tokio_tungstenite::connect_async(req(addr, &token)).await.unwrap_err();
    assert!(err.to_string().contains("401"), "{err}");
}
