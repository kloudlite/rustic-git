//! The forwarder against a stub target on loopback. Nothing here needs a cluster.

use kloudlite_intercept_proxy::{parse, pump, Args, DEFAULT_MAX_CONNS};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A target that echoes every byte back and then closes when its peer half-closes.
async fn echo_target() -> u16 {
    let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => {
                            let _ = s.shutdown().await;
                            return;
                        }
                        Ok(n) => {
                            if s.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    port
}

/// Start the forwarder on an ephemeral listen port forwarding to `to`, and answer its port.
async fn proxy_to(to: u16) -> u16 {
    let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = l.local_addr().unwrap().port();
    let limit = Arc::new(tokio::sync::Semaphore::new(DEFAULT_MAX_CONNS));
    tokio::spawn(kloudlite_intercept_proxy::accept_loop(
        l,
        to,
        Arc::new("127.0.0.1".to_string()),
        limit,
    ));
    port
}

#[test]
fn args_carry_the_remap_and_refuse_nonsense() {
    let a = parse(
        [
            "--target",
            "t.ws-alice.svc",
            "--forward",
            "8080:3000",
            "--forward",
            "9229:9229",
        ]
        .iter()
        .map(|s| s.to_string()),
    )
    .unwrap();
    assert_eq!(
        a,
        Args {
            target: "t.ws-alice.svc".into(),
            forwards: vec![(8080, 3000), (9229, 9229)],
            max_conns: DEFAULT_MAX_CONNS
        }
    );
    assert!(
        parse(["--target", "t"].iter().map(|s| s.to_string())).is_err(),
        "no --forward must be refused"
    );
    assert!(
        parse(["--forward", "8080:3000"].iter().map(|s| s.to_string())).is_err(),
        "no --target must be refused"
    );
    assert!(
        parse(
            ["--target", "t", "--forward", "8080:0"]
                .iter()
                .map(|s| s.to_string())
        )
        .is_err(),
        "0 is not a port"
    );
    assert!(
        parse(
            [
                "--target",
                "t",
                "--forward",
                "8080:3000",
                "--max-conns",
                "0"
            ]
            .iter()
            .map(|s| s.to_string())
        )
        .is_err(),
        "a zero ceiling binds every port and serves nothing, which must be refused"
    );
}

#[tokio::test]
async fn bytes_round_trip_through_the_remapped_port() {
    let target = echo_target().await;
    let listen = proxy_to(target).await;
    let mut c = TcpStream::connect(("127.0.0.1", listen)).await.unwrap();
    c.write_all(b"hello intercept").await.unwrap();
    let mut buf = [0u8; 15];
    c.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello intercept");
}

#[tokio::test]
async fn a_half_close_reaches_the_target_and_the_answer_comes_back() {
    // The HTTP/1.0 shape: write a request, half-close, read until EOF. Without half-close
    // propagation the echo target never sees EOF and this hangs.
    let target = echo_target().await;
    let listen = proxy_to(target).await;
    let mut c = TcpStream::connect(("127.0.0.1", listen)).await.unwrap();
    c.write_all(b"GET / HTTP/1.0\r\n\r\n").await.unwrap();
    c.shutdown().await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), c.read_to_end(&mut out))
        .await
        .expect("half-close did not propagate")
        .unwrap();
    assert_eq!(out, b"GET / HTTP/1.0\r\n\r\n");
}

#[tokio::test]
async fn an_idle_connection_is_dropped_at_the_deadline() {
    // `pump` directly with a paused clock — the real IDLE_SECS is ten minutes and a test that
    // waits it out is ten minutes of CI for one assertion.
    tokio::time::pause();
    let target = echo_target().await;
    let a = TcpStream::connect(("127.0.0.1", target)).await.unwrap();
    let b = TcpStream::connect(("127.0.0.1", target)).await.unwrap();
    let h = tokio::spawn(pump(a, b));
    tokio::time::advance(std::time::Duration::from_secs(
        kloudlite_intercept_proxy::IDLE_SECS + 1,
    ))
    .await;
    let e = h
        .await
        .unwrap()
        .expect_err("an idle connection must be dropped");
    assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
}

#[tokio::test]
async fn the_semaphore_bounds_connections_in_flight() {
    let target = echo_target().await;
    let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let listen = l.local_addr().unwrap().port();
    let limit = Arc::new(tokio::sync::Semaphore::new(1));
    tokio::spawn(kloudlite_intercept_proxy::accept_loop(
        l,
        target,
        Arc::new("127.0.0.1".to_string()),
        limit.clone(),
    ));
    let mut held = TcpStream::connect(("127.0.0.1", listen)).await.unwrap();
    held.write_all(b"x").await.unwrap();
    let mut one = [0u8; 1];
    held.read_exact(&mut one).await.unwrap();
    // The one permit is taken; a second connection is accepted by the kernel but not served until
    // the first closes, so a read on it times out.
    let mut second = TcpStream::connect(("127.0.0.1", listen)).await.unwrap();
    second.write_all(b"y").await.unwrap();
    let mut buf = [0u8; 1];
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(300),
            second.read_exact(&mut buf)
        )
        .await
        .is_err(),
        "a connection over the bound must wait, not be served"
    );
    drop(held);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        second.read_exact(&mut buf),
    )
    .await
    .expect("the bound never released")
    .unwrap();
    assert_eq!(&buf, b"y");
}
