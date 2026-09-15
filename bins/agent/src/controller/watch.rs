//! The supervisor every bare watch in the agent runs under.
//!
//! kube-runtime already backs off on errors, never ends a `watcher` on its own, and rebuilds a
//! connection that goes quiet past `timeout + 5 s`. A 410 delivered as a watch event relists. What
//! it never relists on is everything else: a watch that fails to START (its status retried from the
//! same resourceVersion forever), a non-410 error event, or a stream that ends or idles and resumes
//! from the version it held. On 2026-09-15 07:05–10:22 IST the k3s API server's watch cache froze,
//! and after k3s was restarted the agents' Snapshot watch saw no new event until the three agents
//! were restarted by hand — what the restart did that the retries never did is a fresh LIST. So
//! this layer rebuilds the watcher from nothing (a relist) when it has gone quiet after an error,
//! soon, or quiet at all, much later; and rebuilds a stream that ends, paced.
//!
//! Bookmarks and server-side timeouts are consumed inside kube-runtime and never reach this layer,
//! so "no event" alone cannot tell a healthy quiet kind from a stuck one — the error is the tell.
//! The reflector keeps its old store until the relist's `InitDone` swaps it, so a reader never sees
//! it empty; the cost is one LIST and one pass per object for every subscriber.

use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use kube::runtime::watcher::{Error, Event};
use std::time::Duration;
use tokio::time::Instant;

/// Quiet since an error is the stuck shape — start failures or error events retried against a
/// version the server will not serve — so relist after ten minutes.
pub(crate) const STALE_AFTER_ERROR: Duration = Duration::from_secs(600);
/// Quiet with no error is almost always a healthy kind with nothing happening, and the Volume
/// watch fans out to four controllers; this relist is only the safety net for a hang that errors
/// nowhere.
pub(crate) const STALE_QUIET: Duration = Duration::from_secs(3600);
/// Between a stream ending and its rebuild, so one that ends at once cannot spin.
const REBUILD_PAUSE: Duration = Duration::from_secs(5);

type Events<K> = BoxStream<'static, Result<Event<K>, Error>>;

pub(crate) fn resilient<K, S, F>(kind: &'static str, make: F) -> impl Stream<Item = Result<Event<K>, Error>> + Send
where
    K: Send + 'static,
    S: Stream<Item = Result<Event<K>, Error>> + Send + 'static,
    F: FnMut() -> S + Send + 'static,
{
    with_windows(kind, STALE_AFTER_ERROR, STALE_QUIET, make)
}

fn with_windows<K, S, F>(kind: &'static str, after_error: Duration, quiet: Duration, mut make: F) -> impl Stream<Item = Result<Event<K>, Error>> + Send
where
    K: Send + 'static,
    S: Stream<Item = Result<Event<K>, Error>> + Send + 'static,
    F: FnMut() -> S + Send + 'static,
{
    let first: Events<K> = make().boxed();
    // (stream, last event, an error since it, builder)
    futures::stream::unfold((first, Instant::now(), false, make), move |(mut events, mut last, mut failed, mut make)| async move {
        loop {
            let window = if failed { after_error } else { quiet };
            match tokio::time::timeout_at(last + window, events.next()).await {
                Ok(Some(Ok(ev))) => return Some((Ok(ev), (events, Instant::now(), false, make))),
                Ok(Some(Err(e))) => return Some((Err(e), (events, last, true, make))),
                Ok(None) => {
                    tracing::warn!(kind, "watch.ended");
                    tokio::time::sleep(REBUILD_PAUSE).await;
                }
                Err(_) => tracing::warn!(kind, silent_secs = last.elapsed().as_secs(), errored = failed, "watch.stale"),
            }
            events = make().boxed();
            (last, failed) = (Instant::now(), false);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::ConfigMap;
    use kube::runtime::WatchStreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    type Ev = Result<Event<ConfigMap>, Error>;
    const SHORT: Duration = Duration::from_secs(120);

    fn counted<S: Stream<Item = Ev> + Send + 'static>(f: impl Fn(usize) -> S + Send + 'static) -> (Arc<AtomicUsize>, impl FnMut() -> S + Send + 'static) {
        let n = Arc::new(AtomicUsize::new(0));
        let m = n.clone();
        (n, move || f(m.fetch_add(1, Ordering::SeqCst)))
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_that_ends_is_rebuilt() {
        let (made, make) = counted(|_| futures::stream::iter([Ok(Event::InitDone)]));
        let mut s = Box::pin(resilient("ConfigMap", make));
        for _ in 0..3 {
            assert!(matches!(s.next().await, Some(Ok(Event::InitDone))));
        }
        assert_eq!(made.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn quiet_after_an_error_relists_at_the_short_window_and_plain_quiet_at_the_long_one() {
        let (made, make) = counted(|_| {
            futures::stream::iter([Ok(Event::InitDone)]).chain(futures::stream::repeat_with(|| Err(Error::NoResourceVersion)).then(|e| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                e
            }))
        });
        let mut s = Box::pin(with_windows("ConfigMap", SHORT, STALE_QUIET, make));
        assert!(matches!(s.next().await, Some(Ok(Event::InitDone))));
        let started = Instant::now();
        let mut errors = 0;
        while let Some(Err(_)) = s.next().await {
            errors += 1;
        }
        assert!(errors >= 3, "errors pass through: {errors}");
        let took = started.elapsed();
        assert!(took >= SHORT && took < STALE_QUIET, "{took:?}");
        assert_eq!(made.load(Ordering::SeqCst), 2);

        let (made, make) = counted(|_| futures::stream::pending::<Ev>());
        let mut s = Box::pin(with_windows("ConfigMap", SHORT, STALE_QUIET, make));
        assert!(tokio::time::timeout(Duration::from_secs(3500), s.next()).await.is_err(), "a quiet watch is left alone");
        assert_eq!(made.load(Ordering::SeqCst), 1);
        assert!(tokio::time::timeout(Duration::from_secs(200), s.next()).await.is_err());
        assert_eq!(made.load(Ordering::SeqCst), 2, "and relisted once the safety net passes");
    }

    #[tokio::test(start_paused = true)]
    async fn a_rebuild_keeps_the_store_populated() {
        let cm = |n: &str| ConfigMap { metadata: kube::api::ObjectMeta { name: Some(n.into()), namespace: Some("ns".into()), ..Default::default() }, ..Default::default() };
        let (reader, writer) = kube::runtime::reflector::store::<ConfigMap>();
        // The first build lists `a`, errors once, then hangs; the rebuild's relist has not reached
        // InitDone yet when the store is read.
        let (_, make) = counted(move |i| {
            let head: Vec<Ev> = if i == 0 {
                vec![Ok(Event::Init), Ok(Event::InitApply(cm("a"))), Ok(Event::InitDone), Err(Error::NoResourceVersion)]
            } else {
                vec![Ok(Event::Init)]
            };
            futures::stream::iter(head).chain(futures::stream::pending())
        });
        let mut s = Box::pin(with_windows("ConfigMap", SHORT, STALE_QUIET, make).reflect(writer));
        for _ in 0..4 {
            s.next().await;
        }
        reader.wait_until_ready().await.unwrap();
        assert!(matches!(s.next().await, Some(Ok(Event::Init))), "the rebuild's relist began");
        assert_eq!(reader.state().len(), 1, "the old store stands until the relist's InitDone");
        assert!(reader.get(&kube::runtime::reflector::ObjectRef::new("a").within("ns")).is_some());
    }
}
