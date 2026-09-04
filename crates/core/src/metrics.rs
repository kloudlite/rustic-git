//! One Prometheus recorder, shared by all the binaries — same reason `log::init` is shared: the
//! `metrics` macros are silent without a recorder, and six `main`s drifting apart is how one of
//! them ends up exporting nothing while its dashboards stay green.
//!
//! Exposure is deliberately NOT on any public listener. Every binary, the server included, serves
//! a dedicated listener via `serve_if_configured` on `KLOUDLITE_GIT_METRICS_ADDR`, unset in dev.
//! Metric text lists every repository key a node has touched — that is an enumeration oracle.

use axum::{extract::Request, middleware::Next, response::Response, routing::get, Router};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use std::sync::OnceLock;
use std::time::Instant;

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the process-wide recorder. Idempotent, like `log::init`; call it right after.
pub fn init() {
    HANDLE.get_or_init(|| {
        PrometheusBuilder::new()
            // Durations are histograms, not the exporter's default summaries: a summary cannot
            // be aggregated across pods, and every alert in `deploy/alerts.md` is fleet-wide.
            .set_buckets_for_metric(
                Matcher::Suffix("_seconds".into()),
                &[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0],
            )
            .expect("non-empty bucket list")
            .install_recorder()
            .expect("first recorder in this process")
    });
}

/// The current scrape text. Empty (not a panic) before `init`, so a test that never installed
/// the recorder still gets a well-formed reply.
pub fn render() -> String {
    HANDLE.get().map(PrometheusHandle::render).unwrap_or_default()
}

/// `GET /metrics`, to merge into an internal router.
pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new().route("/metrics", get(|| async { render() }))
}

/// A whole listener for the binaries that have no internal one (worker, agent, gateway, api).
/// Returns immediately when `KLOUDLITE_GIT_METRICS_ADDR` is unset; a bind failure is fatal
/// because a pod annotated for scraping that silently serves nothing is the failure mode this
/// module exists to prevent.
pub async fn serve_if_configured() {
    let Ok(addr) = std::env::var("KLOUDLITE_GIT_METRICS_ADDR") else { return };
    let l = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("binding KLOUDLITE_GIT_METRICS_ADDR={addr}: {e}"));
    tracing::info!(listener = "metrics", %addr, "listener.started");
    let app = routes().route("/healthz", get(|| async { "ok" }));
    tokio::spawn(async move {
        if let Err(e) = axum::serve(l, app).await {
            tracing::error!(listener = "metrics", %addr, error = %e, "listener.failed");
        }
    });
}

/// One entry for `register`: the metric's name, what it is, and the label set to touch.
pub type Series = (&'static str, Kind, &'static [(&'static str, &'static str)]);

/// What `register` does to a name to bring its series into existence.
pub enum Kind {
    Counter,
    Gauge,
    /// Listed for completeness and NOT touched: a histogram's zero observation is a real
    /// observation, and it would skew every quantile the series is read for. A histogram with no
    /// samples is legitimately absent from `/metrics`, so a rule over one must read an empty
    /// result as "nothing happened", never as a broken exporter.
    Histogram,
}

/// Touch every series a binary can emit, so it exists on `/metrics` from boot.
///
/// `metrics-rs` creates a series on its first increment, so an idle worker exports nothing and
/// every rule over it reads "no samples in the window" forever — `unknown` on the Signals page
/// rather than the `ok` the silence actually means. Labels are part of a series' identity, so a
/// rule that filters on one (`status="5xx"`, `state="error"`) needs THAT combination registered,
/// in the same label order the emitting call site uses — a different order is a different key and
/// would export the same label set twice.
pub fn register(series: &[Series]) {
    for (name, kind, labels) in series {
        let labels: Vec<metrics::Label> =
            labels.iter().map(|(k, v)| metrics::Label::from_static_parts(k, v)).collect();
        match kind {
            Kind::Counter => metrics::counter!(*name, labels).absolute(0),
            Kind::Gauge => metrics::gauge!(*name, labels).set(0.0),
            Kind::Histogram => {}
        }
    }
}

/// Per-request count and latency, labelled by listener, route class and status. Mount it
/// OUTERMOST so it sees the status every inner layer (auth, routing) settles on.
/// `axum::middleware::from_fn_with_state("peer", http_metrics)`.
pub async fn http_metrics(
    axum::extract::State(listener): axum::extract::State<&'static str>,
    req: Request,
    next: Next,
) -> Response {
    let class = route_class(req.uri().path());
    let start = Instant::now();
    let res = next.run(req).await;
    let labels = [
        ("listener", listener),
        ("class", class),
        ("status", status_class(res.status().as_u16())),
    ];
    metrics::counter!("http_requests_total", &labels).increment(1);
    metrics::histogram!("http_request_duration_seconds", "listener" => listener, "class" => class)
        .record(start.elapsed().as_secs_f64());
    res
}

/// A bounded label set: the path itself would be one series per repository.
fn route_class(path: &str) -> &'static str {
    if path.ends_with("/git-upload-pack") || path.ends_with("/git-receive-pack") || path.ends_with("/info/refs") {
        "git"
    } else if path.starts_with("/v2/") {
        "registry"
    } else if path.starts_with("/api/") {
        "browse"
    } else if path.starts_with("/v1/") {
        "v1"
    } else if path.starts_with("/own/") {
        "own"
    } else if path.starts_with("/tunnel/") {
        "tunnel"
    } else if path == "/healthz" || path == "/metrics" {
        "probe"
    } else {
        "other"
    }
}

/// The exact code stays in the logs; `413` and `421` get their own class because each is a
/// named incident in this repo (the body cap and a follower asked to write the map).
fn status_class(code: u16) -> &'static str {
    match code {
        413 => "413",
        421 => "421",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn classes_are_bounded_and_named() {
        assert_eq!(super::route_class("/alice/repo/git-receive-pack"), "git");
        assert_eq!(super::route_class("/v2/alice/img/blobs/sha256:00"), "registry");
        assert_eq!(super::route_class("/api/alice/repo/tree"), "browse");
        assert_eq!(super::route_class("/own/claim"), "own");
        assert_eq!(super::route_class("/whatever"), "other");
        assert_eq!(super::status_class(421), "421");
        assert_eq!(super::status_class(502), "5xx");
    }

    /// The whole point of `register`: a series a nothing-has-happened-yet process can be scraped
    /// for, so a quiet rule reads `ok` instead of `unknown`. The histogram is the deliberate
    /// exception — it stays absent until something is actually observed.
    #[test]
    fn registered_series_exist_before_anything_touches_them() {
        use super::Kind::*;
        super::init();
        super::register(&[
            ("test_counter_total", Counter, &[("state", "error")]),
            ("test_gauge", Gauge, &[]),
            ("test_duration_seconds", Histogram, &[]),
        ]);
        let text = super::render();
        assert!(text.contains("test_counter_total{state=\"error\"} 0"), "{text}");
        assert!(text.contains("test_gauge 0"), "{text}");
        assert!(!text.contains("test_duration_seconds"), "{text}");
    }
}
