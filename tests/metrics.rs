//! `/metrics` on the peer listener: scrapeable without the peer secret, and counting.
mod common;

#[tokio::test]
async fn peer_listener_serves_prometheus_text_without_the_secret() {
    rustic_git_core::metrics::init();
    let (base, _e) = common::serve_peer().await;
    // One request through the middleware so the series exists before the scrape.
    assert_eq!(common::peer_get(&base, "/healthz").await.status(), 200);

    let res = reqwest::get(format!("{base}/metrics")).await.unwrap();
    assert_eq!(res.status(), 200);
    let body = res.text().await.unwrap();
    assert!(
        body.contains(r#"http_requests_total{listener="peer",class="probe",status="2xx"}"#),
        "no request series in:\n{body}"
    );
    assert!(body.contains("http_request_duration_seconds_bucket{"), "durations are histograms:\n{body}");
}
