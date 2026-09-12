//! One bound on every request this agent makes to the API server.
//!
//! kube's own `read_timeout` is sized for watches (295 s by default, 120 s here), so an ordinary
//! GET the server queues has effectively no deadline: on 2026-09-12 a `keys` step sat 59.9 s in
//! `converge_owner`'s first `get_opt`, and every other object on that node reported
//! `event.late age_ms ≈ 59500` in the same minute. A stalled request is answered as an error the
//! reconciler can log and requeue on, not waited out.
//!
//! Per REQUEST, not per client, because a watch is legitimately minutes long: anything whose query
//! carries `watch=true` keeps the connection-level bound and nothing else. Everything over
//! `SLOW` is logged with its method and path, so the next stall is readable rather than inferred.
//!
//! ponytail: a local copy of what `crates/workspaces` is growing as `k8s::client::bounded_client`
//! (batch 4.2, branch `fix/4`) — same constants, same `kube.slow` name. Delete this module and
//! call that one when the two branches meet.

// `axum::http`, not a fresh `http` dependency: axum, kube and hyper all resolve to the one http
// 1.x in the lock file, and re-declaring it is a second pin that can drift from theirs.
use axum::http::{Request, Response};
use std::task::{Context, Poll};
use tower::{BoxError, Layer, Service};

/// The deadline on one non-watch request. Well above any healthy API server answer and well below
/// the connection-level `read_timeout`, so this is what fires first on a queued request.
const BOUND: std::time::Duration = std::time::Duration::from_secs(30);
/// Anything slower than this is logged. A healthy request is single-digit milliseconds.
const SLOW: u128 = 2_000;

#[derive(Clone)]
pub(crate) struct BoundedLayer;

impl<S> Layer<S> for BoundedLayer {
    type Service = Bounded<S>;

    fn layer(&self, inner: S) -> Bounded<S> {
        Bounded { inner }
    }
}

pub(crate) struct Bounded<S> {
    inner: S,
}

impl<S, B> Service<Request<kube::client::Body>> for Bounded<S>
where
    S: Service<Request<kube::client::Body>, Response = Response<B>>,
    S::Future: Send + 'static,
    S::Error: Into<BoxError>,
{
    type Response = S::Response;
    type Error = BoxError;
    type Future = futures::future::BoxFuture<'static, Result<Self::Response, BoxError>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: Request<kube::client::Body>) -> Self::Future {
        let watch = req.uri().query().is_some_and(|q| q.contains("watch=true"));
        let (method, path) = (req.method().clone(), req.uri().path().to_string());
        // Called on THIS service, never a clone: `poll_ready` above reserved capacity on the one
        // instance, and kube's default stack is a `BoxService`, which is not `Clone` anyway.
        let fut = self.inner.call(req);
        Box::pin(async move {
            let started = std::time::Instant::now();
            let out: Result<Response<B>, BoxError> = if watch {
                fut.await.map_err(Into::into)
            } else {
                match tokio::time::timeout(BOUND, fut).await {
                    Ok(r) => r.map_err(Into::into),
                    Err(_) => Err(BoxError::from(format!("{method} {path}: no answer in {}s", BOUND.as_secs()))),
                }
            };
            let ms = started.elapsed().as_millis();
            if ms > SLOW && !watch {
                tracing::warn!(method = %method, path = %path, ms, "kube.slow");
            }
            out
        })
    }
}
