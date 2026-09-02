//! Metrics count, and are NOT reachable on the peer listener: the peer port runs with
//! `networkPolicy: none`, so anything served there without the secret is readable by any pod, and
//! the scrape text lists every repository key this node has touched.
mod common;

#[tokio::test]
async fn the_peer_listener_no_longer_serves_metrics() {
    rustic_git_core::metrics::init();
    let (base, _e) = common::serve_peer().await;
    let res = reqwest::get(format!("{base}/metrics")).await.unwrap();
    // `trust_peer` is the outermost layer and gates on the secret before any path is matched, so
    // an unauthenticated request never reaches routing to observe the route is gone — it is
    // refused as forbidden rather than not-found. Both mean the same thing to a scraper with no
    // secret to present: unreachable.
    assert_eq!(res.status(), 403, "metrics moved to their own listener");
}

#[tokio::test]
async fn the_peer_listener_with_the_secret_still_has_no_metrics_route() {
    // The regression that matters isn't the auth gate above — it's someone re-merging
    // `metrics::routes()` into the peer router. A valid secret must still find nothing there.
    rustic_git_core::metrics::init();
    let (base, _e) = common::serve_peer().await;
    let res = common::peer_get(&base, "/metrics").await;
    assert_eq!(res.status(), 404, "metrics must not be routed on the peer listener");
}

#[tokio::test]
async fn the_metrics_listener_serves_prometheus_text_and_counts() {
    rustic_git_core::metrics::init();
    let (base, _e) = common::serve_peer().await;
    // One request through the middleware so the series exists before the scrape.
    assert_eq!(common::peer_get(&base, "/healthz").await.status(), 200);

    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, rustic_git_core::metrics::routes::<()>().with_state(())).await.unwrap();
    });
    let res = reqwest::get(format!("http://{addr}/metrics")).await.unwrap();
    assert_eq!(res.status(), 200);
    let body = res.text().await.unwrap();
    assert!(
        body.contains(r#"http_requests_total{listener="peer",class="probe",status="2xx"}"#),
        "no request series in:\n{body}"
    );
    assert!(body.contains("http_request_duration_seconds_bucket{"), "durations are histograms:\n{body}");
}
