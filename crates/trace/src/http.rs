//! The two seams every hop uses: `server_span` on the way in, `inject` on the way out.
//!
//! Inbound: the caller's `traceparent` becomes the parent of one server span per request, named by
//! the route CLASS (bounded cardinality), never the raw path. The trace id is always continued;
//! the caller's sampled flag is obeyed only for probe traffic (`is_probe`, decided in the sampler). Health and metrics paths get a span under `UNTRACED`, which the OpenTelemetry layer
//! filters out, so `req_id` still lands on their log lines but they never reach a trace.
//!
//! Outbound: `inject` writes the CURRENT span's context. There is no client span for a plain
//! hop — the callee's server span is the child of the caller's server span, which is what the
//! waterfall and the service map need; a client span per hop would double the span count for a
//! row that says "the network took 1 ms".

use opentelemetry::global;
use opentelemetry::trace::TraceContextExt;
use opentelemetry_http::{HeaderExtractor, HeaderInjector};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::sampler::Probe;

pub const UNTRACED: &str = "kloudlite_trace::untraced";

/// Sent by the SLO probe beside `traceparent`, and re-sent by `inject` on every hop of a probe
/// trace. A dedicated header rather than an `x-request-id` prefix: the request id stays exactly
/// as it is (Global Constraints), and a header survives hops that mint their own request id.
/// It is a sampling hint, not a credential — forging it buys a caller a kept trace, nothing else.
pub const PROBE_HEADER: &str = "x-kloudlite-probe";

pub fn is_probe(headers: &http::HeaderMap) -> bool {
    headers.get(PROBE_HEADER).is_some_and(|v| v == "1")
}

pub fn untraced(path: &str) -> bool {
    matches!(path, "/healthz" | "/readyz" | "/livez" | "/metrics" | "/api/health")
}

/// Record this span's trace id as the `trace_id` field, so JSON log lines inside it carry
/// `span.trace_id`. A no-op when no tracing layer is installed (the context is invalid).
pub fn stamp(span: &tracing::Span) {
    let sc = span.context().span().span_context().clone();
    if sc.is_valid() {
        span.record("trace_id", sc.trace_id().to_string());
    }
}

pub fn server_span(method: &http::Method, path: &str, headers: &http::HeaderMap, req_id: &str, route: &str) -> tracing::Span {
    if untraced(path) {
        return tracing::info_span!(target: UNTRACED, "http", req_id = %req_id);
    }
    let span = tracing::info_span!(
        "http",
        otel.name = %format!("{method} {route}"),
        otel.kind = "server",
        otel.status_code = tracing::field::Empty,
        http.request.method = %method,
        url.path = %path,
        http.response.status_code = tracing::field::Empty,
        req_id = %req_id,
        trace_id = tracing::field::Empty,
    );
    let mut parent = global::get_text_map_propagator(|p| p.extract(&HeaderExtractor(headers)));
    if is_probe(headers) {
        parent = parent.with_value(Probe);
    }
    // Err only when no OpenTelemetry layer is installed — exactly the case with nothing to do.
    let _ = span.set_parent(parent);
    stamp(&span);
    span
}

pub fn finish(span: &tracing::Span, status: u16) {
    span.record("http.response.status_code", status);
    if status >= 500 {
        span.record("otel.status_code", "ERROR");
    }
}

pub fn client_span(name: &'static str, method: &str, path: &str) -> tracing::Span {
    let span = tracing::info_span!(
        "client",
        otel.name = name,
        otel.kind = "client",
        otel.status_code = tracing::field::Empty,
        http.request.method = %method,
        url.path = %path,
        kube.timeout_layer = tracing::field::Empty,
        trace_id = tracing::field::Empty,
    );
    stamp(&span);
    span
}

pub fn inject(headers: &mut http::HeaderMap) {
    let cx = tracing::Span::current().context();
    global::get_text_map_propagator(|p| p.inject_context(&cx, &mut HeaderInjector(headers)));
    if cx.get::<Probe>().is_some() {
        headers.insert(PROBE_HEADER, http::HeaderValue::from_static("1"));
    }
}

pub fn inject_reqwest(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let mut h = http::HeaderMap::new();
    inject(&mut h);
    // `RequestBuilder::headers` REPLACES each name it is given (reqwest `util::replace_headers`),
    // so a caller's own `traceparent` is overwritten, never duplicated; the test holds that.
    req.headers(h)
}

/// For listeners that do not mount `kloudlite_core::metrics::http_metrics` (agent peer, tool
/// server, builder gate): the same server span, no metrics. Named by the matched route so ids in
/// `/peer/v1/snapshot/{volume}/{name}` and `/stream/*/{id}` never reach a span name.
pub async fn traced(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    use tracing::Instrument;
    let (method, path) = (req.method().clone(), req.uri().path().to_string());
    let route = req.extensions().get::<axum::extract::MatchedPath>().map_or_else(|| path.clone(), |m| m.as_str().to_string());
    let span = server_span(&method, &path, req.headers(), "", &route);
    let res = next.run(req).instrument(span.clone()).await;
    finish(&span, res.status().as_u16());
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::TraceId;

    const PARENT: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

    fn headers(probe: bool) -> http::HeaderMap {
        let mut h = http::HeaderMap::new();
        h.insert("traceparent", http::HeaderValue::from_static(PARENT));
        if probe {
            h.insert(PROBE_HEADER, http::HeaderValue::from_static("1"));
        }
        h
    }

    fn trace_of(span: &tracing::Span) -> (TraceId, bool) {
        let sc = span.context().span().span_context().clone();
        (sc.trace_id(), sc.is_sampled())
    }

    #[test]
    fn is_probe_wants_the_header_set_to_one() {
        assert!(is_probe(&headers(true)));
        assert!(!is_probe(&headers(false)));
        let mut h = headers(false);
        h.insert(PROBE_HEADER, http::HeaderValue::from_static("yes"));
        assert!(!is_probe(&h));
    }

    // The default ratio (0.1) does not pick this trace id, so "sampled" can only come from the flag.
    #[test]
    fn an_outside_sampled_flag_is_ignored_but_the_trace_continues() {
        let (d, _) = crate::testing::subscriber();
        tracing::dispatcher::with_default(&d, || {
            let span = server_span(&http::Method::GET, "/v1/x", &headers(false), "r", "/v1/x");
            let (id, sampled) = trace_of(&span);
            assert_eq!(id, TraceId::from_hex("0af7651916cd43dd8448eb211c80319c").unwrap());
            assert!(!sampled);
            let mut out = http::HeaderMap::new();
            span.in_scope(|| inject(&mut out));
            assert!(out.get(PROBE_HEADER).is_none());
        });
    }

    #[test]
    fn a_probe_sampled_flag_is_kept_and_passed_on() {
        let (d, _) = crate::testing::subscriber();
        tracing::dispatcher::with_default(&d, || {
            let span = server_span(&http::Method::GET, "/v1/x", &headers(true), "r", "/v1/x");
            assert!(trace_of(&span).1);
            let child = span.in_scope(|| tracing::info_span!("child"));
            assert!(trace_of(&child).1, "a child of a probe span inherits the marker");
            let mut out = http::HeaderMap::new();
            child.in_scope(|| inject(&mut out));
            assert_eq!(out.get(PROBE_HEADER).unwrap(), "1");
        });
    }

    #[test]
    fn inject_reqwest_replaces_an_existing_traceparent() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (d, _) = crate::testing::subscriber();
        tracing::dispatcher::with_default(&d, || {
            let span = server_span(&http::Method::GET, "/v1/x", &headers(true), "r", "/v1/x");
            let req = span.in_scope(|| {
                let stale = reqwest::Client::new().get("http://127.0.0.1:1/").header("traceparent", "00-11111111111111111111111111111111-2222222222222222-00");
                inject_reqwest(stale).build().unwrap()
            });
            let got: Vec<_> = req.headers().get_all("traceparent").iter().collect();
            assert_eq!(got.len(), 1);
            assert!(got[0].to_str().unwrap().starts_with("00-0af7651916cd43dd8448eb211c80319c-"));
        });
    }
}
