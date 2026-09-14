//! The two seams every hop uses: `server_span` on the way in, `inject` on the way out.
//!
//! Inbound: the caller's `traceparent` becomes the parent of one server span per request, named by
//! the route CLASS (bounded cardinality), never the raw path. Whether the caller's sampled flag is
//! obeyed is decided by the sampler (`trust_remote_sampled`), not here: the trace id is always
//! continued. Health and metrics paths get a span under `UNTRACED`, which the OpenTelemetry layer
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

pub const UNTRACED: &str = "kloudlite_trace::untraced";

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
    let parent = global::get_text_map_propagator(|p| p.extract(&HeaderExtractor(headers)));
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
}

pub fn inject_reqwest(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let mut h = http::HeaderMap::new();
    inject(&mut h);
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
