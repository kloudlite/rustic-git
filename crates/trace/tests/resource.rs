//! The exported OTLP payload carries the service name `provider()` was built with — the path
//! `layer()` takes in every binary. Read off a real HTTP receiver: protobuf strings are raw bytes.

use opentelemetry::trace::{TraceContextExt as _, Tracer as _, TracerProvider as _};
use std::io::{Read as _, Write as _};
use std::net::TcpListener;

#[test]
fn exported_spans_carry_service_name() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let got = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut body = Vec::new();
        let mut buf = [0u8; 8192];
        // Read until the body the Content-Length names has arrived.
        loop {
            let n = conn.read(&mut buf).unwrap();
            body.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&body).to_lowercase();
            if let Some(end) = text.find("\r\n\r\n") {
                let len: usize = text.lines().find_map(|l| l.strip_prefix("content-length:")).map_or(0, |v| v.trim().parse().unwrap());
                if body.len() >= end + 4 + len {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        conn.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").unwrap();
        body
    });

    let provider = kloudlite_trace::provider(&url, "kloudlite-probe-svc").unwrap();
    // A probe-marked root is not needed: `Promote` exports an errored root regardless of the ratio.
    let tracer = provider.tracer("kloudlite");
    tracer.in_span("s", |cx| cx.span().set_status(opentelemetry::trace::Status::error("x")));
    provider.force_flush().unwrap();

    let body = got.join().unwrap();
    let hay = |needle: &[u8]| body.windows(needle.len()).any(|w| w == needle);
    assert!(hay(b"service.name"), "no service.name in the export");
    assert!(hay(b"kloudlite-probe-svc"), "service.name is not the configured one");
}
