//! `pull_one`'s trace span, in its own binary: tracing caches each span site's interest process-wide,
//! so a sibling test on another thread that hits `peer pull` first with no subscriber made this
//! test record no span about half the time when it lived in `peer.rs`.

use kloudlite_agent::peer::{peer_http_client, pull_one, receive_ceiling};
use kloudlite_core::settings::LiveSettings;
use kloudlite_workspaces::engine::{Engine, Pool as EnginePool};
use kloudlite_workspaces::settings::AgentSettings;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

fn test_settings() -> LiveSettings<AgentSettings> {
    LiveSettings::new(AgentSettings::from_env())
}

/// A fake `btrfs` whose `receive` arm creates the destination subvolume and then exits non-zero
/// — exactly what a real `btrfs receive` does on a stream that dies mid-way — and whose
/// `subvolume delete` arm actually removes what it created, so `pull_one`'s own cleanup has
/// something real to act on.
fn write_fake_btrfs_receive_fails_after_creating(dir: &std::path::Path) -> String {
    let path = dir.join("btrfs-receive-fails");
    let script = r#"#!/bin/sh
if [ "$1" = "receive" ]; then
    mkdir -p "$2/c1"
    exit 1
fi
if [ "$1" = "subvolume" ] && [ "$2" = "delete" ]; then
    rm -rf "$3"
    exit 0
fi
"#;
    std::fs::write(&path, script).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path.to_string_lossy().into_owned()
}

/// A transfer is ONE client span however many bytes it streams, and its request carries that
/// span's `traceparent` to the source.
#[tokio::test(flavor = "current_thread")]
async fn a_pull_is_one_span_and_sends_its_traceparent() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    kloudlite_trace::bind_ratio(|| 1.0);
    let (dispatch, spans) = kloudlite_trace::testing::subscriber();
    let _g = tracing::dispatcher::set_default(&dispatch);
    let tmp = tempfile::tempdir().unwrap();
    let fake = write_fake_btrfs_receive_fails_after_creating(tmp.path());
    let engine = Engine::new(EnginePool::new(tmp.path()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let n = socket.read(&mut buf).await.unwrap();
        let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
        let body = vec![7u8; 256 * 1024];
        let _ = socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await;
        let _ = socket.write_all(&body).await;
        let _ = socket.shutdown().await;
    });
    let settings = test_settings();
    let _ = pull_one(&engine, &fake, &peer_http_client().unwrap(), &addr, "s3cret", "v1", "c1", None, receive_ceiling(0, &settings), Duration::from_secs(60)).await;
    let head = rx.await.unwrap().to_lowercase();
    let got = spans.get_finished_spans().unwrap();
    assert_eq!(got.len(), 1, "{:?}", got.iter().map(|s| &s.name).collect::<Vec<_>>());
    assert_eq!(got[0].name, "peer pull");
    let tp = format!("traceparent: 00-{}-{}-01", got[0].span_context.trace_id(), got[0].span_context.span_id());
    assert!(head.contains(&tp), "{head}");
    assert!(!format!("{:?}", got[0]).contains("s3cret"));
}
