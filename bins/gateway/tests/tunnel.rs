//! The gateway's authorization path and its pump, against a mocked API server
//! (`rustic_git_workspaces::kube_test`) and a local TCP echo standing in for the pod's sshd.
//!
//! Everything the gateway decides happens BEFORE the upgrade, so these tests are mostly about
//! which HTTP status a bad connect gets — the one test that upgrades proves the bytes actually
//! cross, which is the only thing the pump can get wrong that a type does not catch.

use rustic_git_core::jwt::{Jwt, SshSessionClaims};
use rustic_git_gateway::Gateway;
use rustic_git_workspaces::kube_test::{get, mock_client, Route};
use std::sync::Arc;
use tokio_tungstenite::tungstenite;

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const REGION: &str = "centralindia-k3s";
const WS: &str = "/apis/rustic-git.io/v1alpha1/workspaces/ws-1";
const POD: &str = "/api/v1/namespaces/ws-alice/pods/ws-1-abc";

fn workspace(phase: &str, pod_ref: Option<&str>) -> serde_json::Value {
    let mut status = serde_json::json!({ "phase": phase, "nodeName": "node-1" });
    if let Some(r) = pod_ref {
        status["podRef"] = serde_json::json!(r);
    }
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1",
        "kind": "Workspace",
        "metadata": { "name": "ws-1" },
        "spec": {
            "owner": "alice", "name": "gh", "region": REGION, "image": "img",
            "desiredState": "running"
        },
        "status": status,
    })
}

fn pod(ip: Option<&str>) -> serde_json::Value {
    let mut status = serde_json::json!({ "phase": "Running" });
    if let Some(ip) = ip {
        status["podIP"] = serde_json::json!(ip);
    }
    serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": "ws-1-abc", "namespace": "ws-alice" },
        "status": status,
    })
}

/// A TCP echo on a free port, standing in for sshd. Returns the port the gateway must dial.
async fn echo() -> u16 {
    echo_on(0).await
}

async fn echo_on(port: u16) -> u16 {
    let l = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
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

/// The gateway serving on a free port; returns its base ws:// URL.
async fn serve(routes: Vec<Route>, ssh_port: u16) -> String {
    let (client, _) = mock_client(routes);
    let gw = Arc::new(Gateway::new(Jwt::new(SECRET).unwrap(), REGION.into(), client, ssh_port));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, rustic_git_gateway::app(gw)).await.unwrap() });
    format!("ws://{addr}")
}

fn token(ws: &str, region: &str) -> String {
    Jwt::new(SECRET).unwrap().mint_ssh_session("alice", ws, region).unwrap().0
}

/// Connect to `/tunnel/{ws}`; `Ok` is the upgraded socket, `Err` the pre-upgrade status.
async fn connect(
    base: &str,
    ws: &str,
    token: &str,
) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, u16> {
    use tungstenite::client::IntoClientRequest;
    let mut req = format!("{base}/tunnel/{ws}").into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    match tokio_tungstenite::connect_async(req).await {
        Ok((s, _)) => Ok(s),
        Err(tungstenite::Error::Http(r)) => Err(r.status().as_u16()),
        Err(e) => panic!("unexpected connect error: {e}"),
    }
}

#[tokio::test]
async fn a_valid_session_is_pumped_to_the_pod_and_spent() {
    use futures::{SinkExt, StreamExt};
    let port = echo().await;
    let base = serve(
        vec![get(WS, workspace("ready", Some("ws-alice/ws-1-abc"))), get(POD, pod(Some("127.0.0.1")))],
        port,
    )
    .await;

    let tok = token("ws-1", REGION);
    let mut sock = connect(&base, "ws-1", &tok).await.expect("upgrade");
    sock.send(tungstenite::Message::binary(b"SSH-2.0-hello".to_vec())).await.unwrap();
    let back = sock.next().await.unwrap().unwrap();
    assert_eq!(back.into_data(), b"SSH-2.0-hello".as_slice());

    // Single use: the same jti a second time is not a second tunnel, even while the first is open.
    assert_eq!(connect(&base, "ws-1", &tok).await.err(), Some(401));
}

#[tokio::test]
async fn a_token_for_another_workspace_is_refused() {
    let base = serve(
        vec![get(WS, workspace("ready", Some("ws-alice/ws-1-abc"))), get(POD, pod(Some("127.0.0.1")))],
        22,
    )
    .await;
    assert_eq!(connect(&base, "ws-1", &token("ws-2", REGION)).await.err(), Some(401));
    // ...and a token minted for a different region's gateway is no better.
    assert_eq!(connect(&base, "ws-1", &token("ws-1", "westeurope-k3s")).await.err(), Some(401));
}

#[tokio::test]
async fn an_unready_workspace_is_409() {
    let base = serve(vec![get(WS, workspace("creating", None))], 22).await;
    assert_eq!(connect(&base, "ws-1", &token("ws-1", REGION)).await.err(), Some(409));

    // No such workspace at all is a 404, not a 409: the caller can tell "gone" from "wait".
    let gone = serve(vec![rustic_git_workspaces::kube_test::not_found(WS)], 22).await;
    assert_eq!(connect(&gone, "ws-1", &token("ws-1", REGION)).await.err(), Some(404));
}

#[tokio::test]
async fn an_expired_token_is_401() {
    let claims = SshSessionClaims {
        sub: "alice".into(),
        ws: "ws-1".into(),
        region: REGION.into(),
        jti: "deadbeef".into(),
        iat: 0,
        exp: 1,
        typ: "ssh-session".into(),
    };
    let raw = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(SECRET.as_bytes()),
    )
    .unwrap();
    let base = serve(vec![get(WS, workspace("ready", Some("ws-alice/ws-1-abc")))], 22).await;
    assert_eq!(connect(&base, "ws-1", &raw).await.err(), Some(401));
    // A token signed by anything else, and no token at all, land in the same place.
    assert_eq!(connect(&base, "ws-1", "not-a-token").await.err(), Some(401));
}

/// A refusal the caller can retry must not consume the token, or "too many connections right
/// now" turns into "log in again". The 409 above is one such refusal; the connection limit is
/// the other, and it is the one that used to burn the token.
#[tokio::test]
async fn hitting_the_connection_limit_does_not_spend_the_token() {
    let port = echo().await;
    let base = serve(
        vec![get(WS, workspace("ready", Some("ws-alice/ws-1-abc"))), get(POD, pod(Some("127.0.0.1")))],
        port,
    )
    .await;

    // Held open: each is one slot against the per-workspace limit.
    let mut open = Vec::new();
    for _ in 0..10 {
        open.push(connect(&base, "ws-1", &token("ws-1", REGION)).await.expect("under the limit"));
    }

    let tok = token("ws-1", REGION);
    assert_eq!(connect(&base, "ws-1", &tok).await.err(), Some(503), "the 11th is refused");
    // The same token again: still 503, NOT 401 — proof it was never spent.
    assert_eq!(connect(&base, "ws-1", &tok).await.err(), Some(503), "the token survived the 503");
    drop(open);
}

/// A connect that fails AFTER the slot is taken — here the pod is not listening yet — must give
/// the slot back, or a workspace whose pod is still booting locks itself out after ten attempts.
#[tokio::test]
async fn failed_dials_do_not_use_up_the_limit() {
    // A port nothing listens on yet: bound, read, released — and the echo takes it over below.
    let port = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
    let base = serve(
        vec![get(WS, workspace("ready", Some("ws-alice/ws-1-abc"))), get(POD, pod(Some("127.0.0.1")))],
        port,
    )
    .await;
    for _ in 0..15 {
        let tok = token("ws-1", REGION);
        assert_eq!(connect(&base, "ws-1", &tok).await.err(), Some(502));
        // A 502 is retryable, so it must not have spent the token.
        assert_eq!(connect(&base, "ws-1", &tok).await.err(), Some(502));
    }
    echo_on(port).await;
    let _live = connect(&base, "ws-1", &token("ws-1", REGION)).await.expect("nothing leaked");
}
