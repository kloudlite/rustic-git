//! The supervisor every bare watch in the agent runs under.
//!
//! kube-runtime already backs off on errors, never ends a `watcher` on its own, and rebuilds a
//! connection that goes quiet past `timeout + 5 s`. None of that relists: every retry resumes from
//! the resourceVersion the watcher last held. On 2026-09-15 07:05–10:22 IST the k3s API server's
//! watch cache froze, and after k3s was restarted the agents' Snapshot watch saw no new event
//! until the three agents were restarted by hand — what the restart did that the retries never
//! did is a fresh LIST. So a watch that has delivered no event for `WATCH_STALE` (errors do not
//! count: a watch that only errors is exactly the stuck one) logs `watch.stale` and is rebuilt
//! from nothing, and a stream that ends is rebuilt too, paced.
//!
//! Bookmarks are consumed inside kube-runtime and never reach this layer, so a healthy but quiet
//! kind is relisted once per window as well. The reflector swaps its store atomically on the
//! relist's `InitDone`, so a reader never sees it empty; the cost is one LIST and, for a
//! controller's shared stream, one no-op pass per object.

use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use kube::runtime::watcher::{Error, Event};
use std::time::Duration;
use tokio::time::Instant;

/// ponytail: one window for every kind. A per-kind window (or reading bookmarks off the raw
/// `WatchEvent` stream) would spare quiet kinds their relist if the LIST cost is ever felt.
pub(crate) const WATCH_STALE: Duration = Duration::from_secs(600);
/// Between a stream ending and its rebuild, so one that ends at once cannot spin.
const REBUILD_PAUSE: Duration = Duration::from_secs(5);

type Events<K> = BoxStream<'static, Result<Event<K>, Error>>;

pub(crate) fn resilient<K, S, F>(kind: &'static str, stale: Duration, mut make: F) -> impl Stream<Item = Result<Event<K>, Error>> + Send
where
    K: Send + 'static,
    S: Stream<Item = Result<Event<K>, Error>> + Send + 'static,
    F: FnMut() -> S + Send + 'static,
{
    let first: Events<K> = make().boxed();
    futures::stream::unfold((first, Instant::now(), make), move |(mut events, mut last, mut make)| async move {
        loop {
            match tokio::time::timeout_at(last + stale, events.next()).await {
                Ok(Some(Ok(ev))) => return Some((Ok(ev), (events, Instant::now(), make))),
                Ok(Some(Err(e))) => return Some((Err(e), (events, last, make))),
                Ok(None) => {
                    tracing::warn!(kind, "watch.ended");
                    tokio::time::sleep(REBUILD_PAUSE).await;
                }
                Err(_) => tracing::warn!(kind, silent_secs = last.elapsed().as_secs(), "watch.stale"),
            }
            events = make().boxed();
            last = Instant::now();
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::ConfigMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn counted<S: Stream<Item = Result<Event<ConfigMap>, Error>> + Send + 'static>(
        f: impl Fn() -> S + Send + 'static,
    ) -> (Arc<AtomicUsize>, impl FnMut() -> S + Send + 'static) {
        let n = Arc::new(AtomicUsize::new(0));
        let m = n.clone();
        (n, move || {
            m.fetch_add(1, Ordering::SeqCst);
            f()
        })
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_that_ends_is_rebuilt() {
        let (made, make) = counted(|| futures::stream::iter([Ok(Event::InitDone)]));
        let mut s = Box::pin(resilient("ConfigMap", WATCH_STALE, make));
        for _ in 0..3 {
            assert!(matches!(s.next().await, Some(Ok(Event::InitDone))));
        }
        assert_eq!(made.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_or_only_failing_stream_is_rebuilt_after_the_window() {
        // Every build answers one event, then only errors forever: the errors pass through, but
        // the clock runs from the event, so the window still rebuilds it.
        let (made, make) = counted(|| {
            futures::stream::iter([Ok(Event::InitDone)])
                .chain(futures::stream::repeat_with(|| Err(Error::NoResourceVersion)).then(|e| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    e
                }))
        });
        let mut s = Box::pin(resilient("ConfigMap", Duration::from_secs(120), make));
        assert!(matches!(s.next().await, Some(Ok(Event::InitDone))));
        let started = Instant::now();
        let mut errors = 0;
        loop {
            match s.next().await {
                Some(Err(_)) => errors += 1,
                Some(Ok(Event::InitDone)) => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(errors >= 3, "errors pass through: {errors}");
        assert!(started.elapsed() >= Duration::from_secs(120), "rebuilt only once the window passed");
        assert_eq!(made.load(Ordering::SeqCst), 2);
        // And one that says nothing at all is rebuilt the same way.
        let (made, make) = counted(futures::stream::pending::<Result<Event<ConfigMap>, Error>>);
        let mut s = Box::pin(resilient("ConfigMap", Duration::from_secs(120), make));
        assert!(tokio::time::timeout(Duration::from_secs(250), s.next()).await.is_err());
        assert_eq!(made.load(Ordering::SeqCst), 3);
    }
}
