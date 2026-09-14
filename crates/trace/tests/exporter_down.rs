//! A collector that is not there must cost a request nothing: no error, no stall.

use std::time::{Duration, Instant};
use tracing_subscriber::layer::SubscriberExt as _;

#[tokio::test(flavor = "current_thread")]
async fn a_dead_collector_never_fails_or_slows_a_request() {
    // Port 9 (discard) on loopback: nothing listens, every export fails fast or times out.
    let layer = kloudlite_trace::layer_with("http://127.0.0.1:9", "test").expect("exporter builds without connecting");
    let _g = tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));
    kloudlite_trace::bind_ratio(|| 1.0);

    let app = axum::Router::new()
        .route("/x", axum::routing::get(|| async { "ok" }))
        .layer(axum::middleware::from_fn(kloudlite_trace::traced));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // Built after `layer_with`, which installed the ring provider this client needs too.
    let http = reqwest::Client::new();
    let started = Instant::now();
    // More requests than the queue holds, so the full-queue drop path runs too.
    for _ in 0..3_000 {
        let r = http.get(format!("http://{addr}/x")).header("traceparent", "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01").send().await.unwrap();
        assert_eq!(r.status(), 200);
    }
    assert!(started.elapsed() < Duration::from_secs(10), "3000 loopback requests took {:?}", started.elapsed());
}
