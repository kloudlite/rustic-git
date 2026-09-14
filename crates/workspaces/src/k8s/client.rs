//! A kube client whose ordinary calls are BOUNDED, and whose slow ones say so.
//!
//! kube's default read timeout is 295 s because a watch is a long read — which means a plain
//! `get`/`patch` against a region cluster over the internet can hang for five minutes with no log
//! line at all. On 2026-09-12 03:55:53 `POST /admin/requests/{id}/deny` did exactly that: twenty
//! seconds of silence on the admin process (nothing else in the fleet slow, no audit or history
//! error) while a `get` and a `patch_status` went to the region cluster, and the probe abandoned
//! the request. The only awaits on that path with no bound of their own were the kube calls.
//!
//! So: one tower layer over the client's service stack that bounds every NON-watch request and
//! warns on any that takes more than `KUBE_SLOW_MS`. Watches stay unbounded — the reflectors in
//! `history::watch` and every controller-shaped watcher depend on a read that lasts.
//!
//! 2026-09-14: reads (GET/LIST) are bounded at 5 s and retried once; writes keep 30 s and are never
//! retried. A second, slightly shorter bound sits directly on hyper, so `kube.timeout layer=inner`
//! places a stall in hyper's pool/connect/dispatch and `layer=outer` alone places it above hyper.

use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use kube::client::Body;
use tower::ServiceExt as _;

/// Writes keep the old bound and are never retried: a PATCH that timed out may still have landed.
pub const KUBE_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// GET and LIST. The API server's own metrics put every one of them under 5 s since k3s start
/// (415,972 `LIST nodes`), while ~284 calls in a day hung exactly 30 s without a byte reaching the
/// socket (`k3s-stall` investigation, 2026-09-13). A read past this bound is a stuck client, not a
/// slow server, so it is abandoned and tried once more.
pub const KUBE_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The inner layer sits directly on hyper, under auth/retry/base-uri, and fires this much earlier.
/// When only the outer layer fires the stall is above hyper (auth token filter, kube retry, Buffer);
/// when the inner one fires it is in hyper's pool, connect or HTTP/1 dispatch.
pub const KUBE_INNER_MARGIN: Duration = Duration::from_millis(500);

/// Above this, a non-watch call is logged. Two seconds is already far outside what an API server
/// on the same continent answers in, so this names a real outlier rather than filling the log.
pub const KUBE_SLOW_MS: u128 = 2_000;

/// Only enough to reach the API server. A connect that has not landed in ten seconds is a
/// network fact, not a slow query.
pub const KUBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A watch is a deliberately long read and must never be bounded. The API server takes it as a
/// query parameter, which is the only place the two kinds of request differ by the time they
/// reach this layer.
pub fn is_watch(uri: &http::Uri) -> bool {
    uri.query().is_some_and(|q| q.split('&').any(|kv| kv == "watch=true" || kv == "watch=1"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Watch,
    Read,
    Write,
}

/// Only headers are bounded (the layer resolves when hyper hands back the response), so a pod-log
/// `follow` GET is safe here too. No upgrade GET reaches this client: kube's `ws` feature is off.
pub fn classify(method: &http::Method, uri: &http::Uri) -> Kind {
    if is_watch(uri) {
        Kind::Watch
    } else if method == http::Method::GET {
        Kind::Read
    } else {
        Kind::Write
    }
}

/// `layer` is which of the two bounds fired; the outer layer retries a read on either.
#[derive(Debug)]
pub struct KubeTimeout {
    pub layer: &'static str,
    pub method: http::Method,
    pub path: String,
    pub after: Duration,
}

impl std::fmt::Display for KubeTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "kubernetes {} {} did not answer in {:?} ({} layer)", self.method, self.path, self.after, self.layer)
    }
}

impl std::error::Error for KubeTimeout {}

#[derive(Clone, Copy)]
pub struct BoundLayer {
    layer: &'static str,
}

impl BoundLayer {
    /// Wraps the whole stack: bounds, retries a read once, logs `kube.slow`.
    pub const OUTER: BoundLayer = BoundLayer { layer: "outer" };
    /// Wraps hyper alone: bounds a little earlier and only reports.
    pub const INNER: BoundLayer = BoundLayer { layer: "inner" };
}

impl<S> tower::Layer<S> for BoundLayer {
    type Service = Bounded<S>;
    fn layer(&self, inner: S) -> Bounded<S> {
        Bounded { inner, layer: self.layer }
    }
}

#[derive(Clone)]
pub struct Bounded<S> {
    inner: S,
    layer: &'static str,
}

fn bound(kind: Kind, layer: &'static str) -> Duration {
    let outer = if kind == Kind::Read { KUBE_READ_TIMEOUT } else { KUBE_CALL_TIMEOUT };
    if layer == "inner" { outer - KUBE_INNER_MARGIN } else { outer }
}

fn copy_request(req: &http::Request<Body>) -> Option<http::Request<Body>> {
    let mut out = http::Request::new(req.body().try_clone()?);
    *out.method_mut() = req.method().clone();
    *out.uri_mut() = req.uri().clone();
    *out.version_mut() = req.version();
    *out.headers_mut() = req.headers().clone();
    Some(out)
}

type BoxFut<R> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<R, tower::BoxError>> + Send>>;

/// One bounded attempt; logs `kube.timeout` for the layer that owns the bound.
async fn attempt<R, E: Into<tower::BoxError>>(
    fut: impl std::future::Future<Output = Result<R, E>>,
    after: Duration,
    layer: &'static str,
    method: &http::Method,
    path: &str,
) -> Result<R, tower::BoxError> {
    let dials = Dials::now();
    match tokio::time::timeout(after, fut).await {
        Ok(r) => r.map_err(Into::into),
        Err(_) => {
            let (dials, dials_ok) = dials.since();
            let inflight = INFLIGHT.load(std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(%method, %path, layer, secs = after.as_secs_f32(), inflight, dials, dials_ok, "kube.timeout");
            Err(KubeTimeout { layer, method: method.clone(), path: path.to_string(), after }.into())
        }
    }
}

impl<S> tower::Service<http::Request<Body>> for Bounded<S>
where
    S: tower::Service<http::Request<Body>> + Clone + Send + 'static,
    S::Error: Into<tower::BoxError>,
    S::Future: Send + 'static,
    S::Response: Send + 'static,
{
    type Response = S::Response;
    type Error = tower::BoxError;
    type Future = BoxFut<S::Response>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let (layer, method, path) = (self.layer, req.method().clone(), req.uri().path().to_string());
        let kind = classify(&method, req.uri());
        let after = bound(kind, layer);
        let again = (layer == "outer" && kind == Kind::Read).then(|| copy_request(&req)).flatten();
        let fut = self.inner.call(req);
        if kind == Kind::Watch {
            return Box::pin(async move { fut.await.map_err(Into::into) });
        }
        if layer == "inner" {
            return Box::pin(async move { attempt(fut, after, layer, &method, &path).await });
        }
        let spare = self.inner.clone();
        Box::pin(async move {
            let start = Instant::now();
            let flight = Flight::start();
            let dials = Dials::now();
            let mut out = attempt(fut, after, layer, &method, &path).await;
            // Retrying drops the stalled future first. hyper-util's `Pooled` only returns a
            // connection to the pool when `is_open` (= its HTTP/1 sender is ready, i.e. the
            // dispatcher is idle and wanting), so a connection wedged mid-request is discarded and
            // the second attempt checks out another idle one or dials; a stall in checkout or
            // connect starts a fresh race; a stall in the auth filter re-enters it.
            let fired = out.as_ref().err().and_then(|e| e.downcast_ref::<KubeTimeout>()).map(|t| t.layer);
            if let (Some(fired), Some(req)) = (fired, again) {
                tracing::warn!(verb = %method, resource = %path, first_attempt_ms = start.elapsed().as_millis() as u64, layer = fired, "kube.retry");
                let ready = spare.ready_oneshot().await.map_err(Into::<tower::BoxError>::into);
                out = match ready {
                    Ok(mut svc) => attempt(svc.call(req), after, layer, &method, &path).await,
                    Err(e) => Err(e),
                };
            }
            let (dials, dials_ok) = dials.since();
            let inflight = flight.finish();
            let ms = start.elapsed().as_millis();
            if out.is_ok() && ms >= KUBE_SLOW_MS {
                tracing::warn!(%method, %path, ms, inflight, dials, dials_ok, "kube.slow");
            }
            out
        })
    }
}

// Client-wide, not per client: a timeout is only readable against everything else the process
// had in flight. hyper's legacy pool never says whether a request got a pooled or a fresh
// connection, so the dial counters are the nearest honest proxy: a call that timed out with
// `dials == 0` rode a connection that was already open (the dead-pooled-socket case), and one with
// `dials > dials_ok` was waiting on a connect that never finished.
static INFLIGHT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static DIALS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static DIALS_OK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct Flight {
    done: bool,
}

impl Flight {
    fn start() -> Self {
        INFLIGHT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Flight { done: false }
    }

    /// Inflight including this call.
    fn finish(mut self) -> u64 {
        self.done = true;
        INFLIGHT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed)
    }
}

struct Dials(u64, u64);

impl Dials {
    fn now() -> Self {
        use std::sync::atomic::Ordering::Relaxed;
        Dials(DIALS.load(Relaxed), DIALS_OK.load(Relaxed))
    }

    /// `(dials started since, dials that connected since)`.
    fn since(&self) -> (u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (DIALS.load(Relaxed) - self.0, DIALS_OK.load(Relaxed) - self.1)
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        // A caller that dropped the future mid-call must not leave the gauge counting it forever.
        if !self.done {
            INFLIGHT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Counts dials under the TLS connector; the connection itself passes through untouched.
#[derive(Clone)]
pub struct Counted<C>(C);

impl<C> tower::Service<http::Uri> for Counted<C>
where
    C: tower::Service<http::Uri>,
    C::Future: Send + 'static,
{
    type Response = C::Response;
    type Error = C::Error;
    type Future = std::pin::Pin<Box<dyn std::future::Future<Output = Result<C::Response, C::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, uri: http::Uri) -> Self::Future {
        use std::sync::atomic::Ordering::Relaxed;
        DIALS.fetch_add(1, Relaxed);
        let fut = self.0.call(uri);
        Box::pin(async move {
            let out = fut.await;
            if out.is_ok() {
                DIALS_OK.fetch_add(1, Relaxed);
            }
            out
        })
    }
}

/// Idle connections are dropped from the pool well before any NAT or conntrack table on the path
/// could forget them, so a request is never written onto a connection only one end still believes
/// in.
pub const KUBE_POOL_IDLE: Duration = Duration::from_secs(30);

/// The kube client the api tier and the agent both use: kube's own default stack — base URI,
/// retry, auth, extra headers — rebuilt here for one reason, the connector.
///
/// kube builds its `HttpConnector` inside a private function and sets no keepalive on it, and its
/// connection pool is HTTP/1.1. So when an API connection dies without a RST — the region's control
/// plane closing an idle socket, a NAT forgetting the flow — the pool still hands it out, the
/// request is written into nothing, and the caller waits out the full call bound. On 2026-09-12
/// that was about five `did not answer in 30s` an hour across the agents, 23 of 30 of them a
/// 46 KB `GET /api/v1/nodes` the API server itself answers in 0.2 s, with the server logging
/// `use of closed network connection` for the same peers. It failed `ws.push.p95`,
/// `env.attach` and `request.approve` on the SLO probe.
///
/// The fix is the one the peer path already carries (`kloudlite_core::peer::bound_dead_peer`):
/// TCP keepalive so a dead connection is noticed in about thirty seconds of idle, a user timeout
/// so unacknowledged writes fail on the same clock, and a pool idle timeout so a connection is
/// retired before a middlebox can strand it. None of them caps a live watch.
///
/// Dropped from kube's stack on purpose: gzip decompression and the HTTP proxy tunnel (neither
/// feature is enabled in this workspace), the debug-level trace spans, and an exec-plugin
/// credential's expiry (no process of ours authenticates through an exec plugin).
pub fn bounded_client(config: kube::Config) -> kube::Result<kube::Client> {
    use hyper_util::{client::legacy::connect::HttpConnector, rt::TokioExecutor};
    use kube::client::ConfigExt as _;
    use kloudlite_core::peer::{KEEPALIVE_IDLE, KEEPALIVE_INTERVAL, KEEPALIVE_RETRIES};

    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_keepalive(Some(KEEPALIVE_IDLE));
    http.set_keepalive_interval(Some(KEEPALIVE_INTERVAL));
    http.set_keepalive_retries(Some(KEEPALIVE_RETRIES));
    // Linux only: the fleet is Linux, and the Mac editor's checker must not paint a real option red.
    #[cfg(target_os = "linux")]
    http.set_tcp_user_timeout(Some(kloudlite_core::peer::USER_TIMEOUT));

    let https = config.rustls_https_connector_with_connector(http)?;
    let mut connector = hyper_timeout::TimeoutConnector::new(Counted(https));
    connector.set_connect_timeout(Some(KUBE_CONNECT_TIMEOUT));
    connector.set_read_timeout(config.read_timeout);
    connector.set_write_timeout(config.write_timeout);

    let hyper = hyper_util::client::legacy::Builder::new(TokioExecutor::new())
        .pool_idle_timeout(KUBE_POOL_IDLE)
        .build::<_, kube::client::Body>(connector);

    let service = tower::ServiceBuilder::new()
        .layer(config.base_uri_layer())
        .option_layer(config.default_retry.then(|| {
            tower::retry::RetryLayer::new(kube::client::retry::RetryPolicy::server_retry())
        }))
        .option_layer(config.auth_layer()?)
        .layer(config.extra_headers_layer()?)
        .map_err(tower::BoxError::from)
        .layer(BoundLayer::INNER)
        .service(hyper);

    Ok(kube::client::ClientBuilder::new(service, config.default_namespace.clone())
        .with_layer(&BoundLayer::OUTER)
        .build())
}

/// `kube::Client::try_default()`, bounded. In-cluster config when the pod has a ServiceAccount,
/// else the operator's kubeconfig — the same inference, only the client differs.
pub async fn try_default() -> kube::Result<kube::Client> {
    let config = kube::Config::infer().await.map_err(kube::Error::InferConfig)?;
    bounded_client(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one classification the whole layer turns on: a watch must never be bounded, and an
    /// ordinary call must never be mistaken for one (a resource actually NAMED `watch=true`
    /// cannot exist, but a field selector mentioning the word can).
    #[test]
    fn only_a_real_watch_parameter_is_a_watch() {
        let w = |s: &str| is_watch(&s.parse::<http::Uri>().unwrap());
        assert!(w("/apis/kloudlite.io/v1alpha1/workspaces?watch=true&timeoutSeconds=290"));
        assert!(w("/api/v1/nodes?allowWatchBookmarks=true&watch=true"));
        assert!(w("/api/v1/nodes?watch=1"));
        assert!(!w("/apis/kloudlite.io/v1alpha1/workspaces"));
        assert!(!w("/apis/kloudlite.io/v1alpha1/requests/req-1/status"));
        assert!(!w("/api/v1/nodes?fieldSelector=metadata.name%3Dwatch%3Dtrue"));
        assert!(!w("/api/v1/nodes?watch=false"));
    }

    #[test]
    fn reads_writes_and_watches_are_classified() {
        let c = |m: http::Method, u: &str| classify(&m, &u.parse::<http::Uri>().unwrap());
        assert_eq!(c(http::Method::GET, "/api/v1/nodes"), Kind::Read);
        assert_eq!(c(http::Method::GET, "/api/v1/nodes/n1"), Kind::Read);
        assert_eq!(c(http::Method::GET, "/api/v1/nodes?watch=true"), Kind::Watch);
        assert_eq!(c(http::Method::PATCH, "/api/v1/nodes/n1"), Kind::Write);
        assert_eq!(c(http::Method::POST, "/api/v1/namespaces"), Kind::Write);
        assert_eq!(c(http::Method::DELETE, "/api/v1/namespaces/x"), Kind::Write);
    }

    /// Hangs its first `hang` calls forever, answers after that; counts every call.
    #[derive(Clone)]
    struct Mock {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        hang: usize,
    }

    impl tower::Service<http::Request<Body>> for Mock {
        type Response = http::Response<()>;
        type Error = tower::BoxError;
        type Future = BoxFut<http::Response<()>>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, _: http::Request<Body>) -> Self::Future {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let hang = n < self.hang;
            Box::pin(async move {
                if hang {
                    std::future::pending::<()>().await;
                }
                Ok(http::Response::new(()))
            })
        }
    }

    fn mock(hang: usize) -> (Mock, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (Mock { calls: calls.clone(), hang }, calls)
    }

    fn req(m: http::Method, u: &str) -> http::Request<Body> {
        let mut r = http::Request::new(Body::empty());
        *r.method_mut() = m;
        *r.uri_mut() = u.parse().unwrap();
        r
    }

    fn calls(c: &std::sync::Arc<std::sync::atomic::AtomicUsize>) -> usize {
        c.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_read_is_retried_once_and_answers() {
        let (m, c) = mock(1);
        let t = tokio::time::Instant::now();
        let svc = tower::Layer::layer(&BoundLayer::OUTER, m);
        svc.oneshot(req(http::Method::GET, "/api/v1/nodes")).await.expect("second attempt answers");
        assert_eq!(calls(&c), 2);
        assert_eq!(t.elapsed().as_secs(), 5);
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_is_retried_only_once() {
        let (m, c) = mock(usize::MAX);
        let svc = tower::Layer::layer(&BoundLayer::OUTER, m);
        let e = svc.oneshot(req(http::Method::GET, "/api/v1/nodes")).await.unwrap_err();
        assert_eq!(e.downcast_ref::<KubeTimeout>().unwrap().after, KUBE_READ_TIMEOUT);
        assert_eq!(calls(&c), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_write_keeps_thirty_seconds_and_is_never_retried() {
        let (m, c) = mock(1);
        let svc = tower::Layer::layer(&BoundLayer::OUTER, m);
        let e = svc.oneshot(req(http::Method::PATCH, "/api/v1/nodes/n1")).await.unwrap_err();
        assert_eq!(e.downcast_ref::<KubeTimeout>().unwrap().after, KUBE_CALL_TIMEOUT);
        assert_eq!(calls(&c), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hyper_stall_is_attributed_to_the_inner_layer() {
        let (m, c) = mock(usize::MAX);
        let svc = tower::Layer::layer(&BoundLayer::OUTER, tower::Layer::layer(&BoundLayer::INNER, m));
        let e = svc.oneshot(req(http::Method::GET, "/api/v1/nodes")).await.unwrap_err();
        let t = e.downcast_ref::<KubeTimeout>().unwrap();
        assert_eq!((t.layer, t.after), ("inner", KUBE_READ_TIMEOUT - KUBE_INNER_MARGIN));
        assert_eq!(calls(&c), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stall_above_hyper_is_attributed_to_the_outer_layer() {
        let (m, _) = mock(usize::MAX);
        let svc = tower::Layer::layer(&BoundLayer::OUTER, m);
        let e = svc.oneshot(req(http::Method::GET, "/api/v1/nodes")).await.unwrap_err();
        assert_eq!(e.downcast_ref::<KubeTimeout>().unwrap().layer, "outer");
    }

    #[tokio::test(start_paused = true)]
    async fn a_watch_is_never_bounded() {
        let (m, c) = mock(usize::MAX);
        let svc = tower::Layer::layer(&BoundLayer::OUTER, tower::Layer::layer(&BoundLayer::INNER, m));
        let call = svc.oneshot(req(http::Method::GET, "/api/v1/nodes?watch=true"));
        assert!(tokio::time::timeout(Duration::from_secs(600), call).await.is_err());
        assert_eq!(calls(&c), 1);
    }
}
