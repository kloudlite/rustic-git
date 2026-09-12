//! Its own binary: the receive-pack permit count is process-global, and a push held open here
//! to exhaust it would 503 an unrelated test's push in the same process.
mod common;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Open a push whose headers promise a huge body and then send nothing. What comes back — and
/// whether anything comes back at all — is the whole test: a server that reads the body before
/// authenticating waits here forever.
async fn open_push(base: &str, auth: Option<&str>) -> TcpStream {
    let mut s = TcpStream::connect(base.strip_prefix("http://").unwrap()).await.unwrap();
    let auth = auth
        .map(|t| {
            use base64::Engine;
            let cred = base64::engine::general_purpose::STANDARD.encode(format!("x:{t}"));
            format!("Authorization: Basic {cred}\r\n")
        })
        .unwrap_or_default();
    let req = format!(
        "POST /alice/web.git/git-receive-pack HTTP/1.1\r\nHost: x\r\n{auth}\
         Content-Type: application/x-git-receive-pack-request\r\nContent-Length: 1000000000\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await.unwrap();
    s
}

async fn status_line(s: &mut TcpStream) -> String {
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(3), s.read(&mut buf))
        .await
        .expect("the server answered without waiting for the body")
        .unwrap();
    String::from_utf8_lossy(&buf[..n]).lines().next().unwrap().to_string()
}

/// The receive-pack permit pool is process-global (`OnceLock`), so these tests cannot share a
/// process concurrently: one exhausting the permits makes another see 503 instead of what it
/// is asserting. Every test in this binary takes this first.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Retry `send` until it answers `want`, or give up after five seconds and return the last
/// response so the caller's own `assert_eq!` reports what it actually got.
async fn poll_status<F, R>(send: &F, want: u16) -> reqwest::Response
where
    F: Fn() -> R,
    R: std::future::Future<Output = reqwest::Result<reqwest::Response>>,
{
    poll_until(send, move |s| s == want).await
}

async fn poll_until<F, R>(send: &F, ok: impl Fn(u16) -> bool) -> reqwest::Response
where
    F: Fn() -> R,
    R: std::future::Future<Output = reqwest::Result<reqwest::Response>>,
{
    let mut last = send().await.unwrap();
    for _ in 0..250 {
        if ok(last.status().as_u16()) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        last = send().await.unwrap();
    }
    last
}

#[tokio::test(flavor = "multi_thread")]
async fn an_anonymous_push_is_refused_before_its_body_is_read() {
    let _serial = SERIAL.lock().await;
    let (base, e) = common::serve_public().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let mut s = open_push(&base, None).await;
    assert!(status_line(&mut s).await.contains(" 401 "));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_third_concurrent_push_gets_503() {
    let _serial = SERIAL.lock().await;
    let (base, e) = common::serve_public().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let token = e.store.create_token("alice").await.unwrap();
    // Two authenticated pushes hold both default permits by never delivering their bodies.
    let a = open_push(&base, Some(&token)).await;
    let _b = open_push(&base, Some(&token)).await;
    let push = || {
        reqwest::Client::new()
            .post(format!("{base}/alice/web.git/git-receive-pack"))
            .basic_auth("x", Some(&token))
            .body("0000")
            .send()
    };
    // Poll for the state, do not sleep towards it (2026-09-12): the two holders take their
    // permits when the server gets round to their headers, which is not a fixed 200 ms. A poll
    // that never sees the state fails saying so, where the sleep quietly asserted the wrong thing.
    let r = poll_status(&push, 503).await;
    assert_eq!(r.status(), 503, "the permits were never exhausted");
    assert_eq!(r.headers().get("retry-after").unwrap(), "5");
    // Dropping a holder frees its permit: the next push is served.
    drop(a);
    let r = poll_until(&push, |s| s != 503).await;
    assert_ne!(r.status(), 503, "the freed permit was never handed on");
}

/// The push body is not read into memory any more, so the cap cannot be a 413 up front: it is
/// applied to the bytes as they stream, and a request that runs past it is refused where it
/// stands. Here that is before the pack, so the status can still say so.
#[tokio::test(flavor = "multi_thread")]
async fn a_push_body_past_max_body_is_refused_as_it_streams() {
    let _serial = SERIAL.lock().await;
    std::env::set_var("KLOUDLITE_MAX_BODY", "65536");
    let (base, e) = common::serve_public().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let token = e.store.create_token("alice").await.unwrap();
    // Command lines, never a flush: 60 KiB pkt-lines cross the cap on the second one.
    let mut body = Vec::new();
    for _ in 0..4 {
        let line = format!("{} {} refs/heads/{}", "0".repeat(40), "1".repeat(40), "x".repeat(60_000));
        kloudlite_core::pktline::write_text(&mut body, &line).unwrap();
    }
    let r = reqwest::Client::new()
        .post(format!("{base}/alice/web.git/git-receive-pack"))
        .basic_auth("x", Some(&token))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("too large"));
}

/// The negotiation is kilobytes; the cap that used to apply here was `max_body` (2 GiB), buffered
/// in memory by `to_bytes` on a route an anonymous client reaches on any public repo. A handful of
/// those OOMs the pod, and an OOM moves repo ownership on the attacker's schedule.
#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_upload_pack_negotiation_is_refused_anonymously() {
    let _serial = SERIAL.lock().await;
    let (base, e) = common::serve_public().await;
    e.store.create_repo("alice", "web").await.unwrap();
    // Public, so this reaches `read_body` with no credentials at all — the amplifier the cap
    // exists for.
    e.store.set_public("alice", "web", true).await.unwrap();
    let r = reqwest::Client::new()
        .post(format!("{base}/alice/web.git/git-upload-pack"))
        .header("content-type", "application/x-git-upload-pack-request")
        .header("git-protocol", "version=2")
        .body(vec![b'0'; 9 * 1024 * 1024])
        .send()
        .await;
    // Either outcome is the same refusal, and the point of the cap — that the body is never
    // buffered — holds for both: the limit answers 413 without reading the rest, and hyper resets
    // the connection rather than draining 9 MiB it will not use. Which one the client sees is a
    // race between the response and the reset, so asserting only the 413 flakes.
    match r {
        Ok(r) => assert_eq!(r.status(), 413),
        Err(e) => assert!(e.is_request(), "expected a refusal, got {e}"),
    }
    // And a real-sized negotiation still gets through to the protocol, so the cap is not simply
    // refusing everything: a v2 command with no flush is a client error, never a 413.
    let mut body = Vec::new();
    kloudlite_core::pktline::write_text(&mut body, "command=ls-refs").unwrap();
    let r = reqwest::Client::new()
        .post(format!("{base}/alice/web.git/git-upload-pack"))
        .header("content-type", "application/x-git-upload-pack-request")
        .header("git-protocol", "version=2")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_ne!(r.status(), 413, "a kilobyte negotiation must not hit the cap");
}
