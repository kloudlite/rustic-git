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

use std::task::{Context, Poll};
use std::time::{Duration, Instant};

/// Every ordinary kube call this tier makes is a get/list/patch against an API server. None of
/// them has any business taking half a minute, and one that does is better as an error a handler
/// can answer with than as a request nobody ever hears back from.
pub const KUBE_CALL_TIMEOUT: Duration = Duration::from_secs(30);

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

#[derive(Clone, Copy, Default)]
pub struct BoundLayer;

impl<S> tower::Layer<S> for BoundLayer {
    type Service = Bounded<S>;
    fn layer(&self, inner: S) -> Bounded<S> {
        Bounded { inner }
    }
}

#[derive(Clone)]
pub struct Bounded<S> {
    inner: S,
}

impl<S, B> tower::Service<http::Request<B>> for Bounded<S>
where
    S: tower::Service<http::Request<B>, Error = tower::BoxError> + Send + 'static,
    S::Future: Send + 'static,
    S::Response: Send + 'static,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = tower::BoxError;
    type Future = std::pin::Pin<Box<dyn std::future::Future<Output = Result<S::Response, tower::BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let watch = is_watch(req.uri());
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        let fut = self.inner.call(req);
        Box::pin(async move {
            if watch {
                return fut.await;
            }
            let start = Instant::now();
            let out = match tokio::time::timeout(KUBE_CALL_TIMEOUT, fut).await {
                Ok(r) => r,
                Err(_) => {
                    tracing::warn!(%method, %path, secs = KUBE_CALL_TIMEOUT.as_secs(), "kube.timeout");
                    return Err(format!("kubernetes {method} {path} did not answer in {:?}", KUBE_CALL_TIMEOUT).into());
                }
            };
            let ms = start.elapsed().as_millis();
            if ms >= KUBE_SLOW_MS {
                tracing::warn!(%method, %path, ms, "kube.slow");
            }
            out
        })
    }
}

/// `kube::Client::try_from(config)`, with the bound above. The one constructor every process of
/// ours should use, so no client is built without it by accident — the api tier and the agent
/// both, which is why nothing in this module knows anything about either.
pub fn bounded_client(mut config: kube::Config) -> kube::Result<kube::Client> {
    config.connect_timeout = Some(KUBE_CONNECT_TIMEOUT);
    Ok(kube::client::ClientBuilder::try_from(config)?.with_layer(&BoundLayer).build())
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
}
