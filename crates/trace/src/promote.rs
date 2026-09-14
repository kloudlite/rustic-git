//! Keep an unsampled trace when its LOCAL root failed or was slow.
//!
//! Every non-sampled span waits here, keyed by trace id, until the span that is this process's
//! root for the trace ends (no parent, or a remote parent). If that root has ERROR status or took
//! longer than `SLOW`, every waiting span of the trace is re-flagged sampled and handed to the
//! batch processor; otherwise they are all dropped. Across nodes this keeps a trace whole in the
//! cases that matter: a downstream failure or stall makes the downstream local root fail or stall
//! too, so each process promotes its own part. A fast upstream wrapped around a slow downstream
//! cannot happen by construction.
//!
//! Bounded by the TOTAL number of waiting spans (`MAX_PENDING`), evicting the trace whose last
//! child arrived longest ago first. A still-running slow request keeps adding children, so it
//! stays at the back of that order; what goes first is a trace whose root never ended (a
//! cancelled future) or has been quiet the longest. One trace holds at most `MAX_SPANS`; children
//! past that are counted and stamped as `dropped_children` on the promoted root rather than lost
//! silently. Every drop is logged as `trace.promote.dropped`, at most once per `LOG_EVERY`.
//!
//! ponytail: eviction order is an append-only queue with lazy deletion (an entry is live only if
//! its sequence number is the trace's latest), compacted when it doubles the cap — O(1) per span
//! with a rare O(n) compaction. A real LRU is the upgrade if the compaction ever shows in profiles.

use crate::SLOW;
use opentelemetry::trace::{SpanContext, SpanId, Status, TraceId};
use opentelemetry::KeyValue;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{Span, SpanData, SpanProcessor};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_PENDING: usize = 16_384;
const MAX_SPANS: usize = 512;
const LOG_EVERY: Duration = Duration::from_secs(10);

#[derive(Debug, Default)]
struct Waiting {
    spans: Vec<SpanData>,
    last: u64,
    dropped: u64,
}

#[derive(Debug, Default)]
pub(crate) struct Pending {
    traces: HashMap<TraceId, Waiting>,
    order: VecDeque<(TraceId, u64)>,
    seq: u64,
    pub(crate) total: usize,
}

#[derive(Debug)]
pub struct Promote<P> {
    inner: P,
    max_pending: usize,
    max_spans: usize,
    pub(crate) pending: Mutex<Pending>,
    /// Drops not yet logged: [evicted for capacity, past the per-trace cap].
    unlogged: [AtomicU64; 2],
    last_log: Mutex<Option<Instant>>,
}

impl<P> Promote<P> {
    pub fn new(inner: P) -> Self {
        Self::with_limits(inner, MAX_PENDING, MAX_SPANS)
    }

    pub(crate) fn with_limits(inner: P, max_pending: usize, max_spans: usize) -> Self {
        Self { inner, max_pending, max_spans, pending: Mutex::default(), unlogged: Default::default(), last_log: Mutex::default() }
    }

    /// Called with the lock released: the log line is an event, and must not run under `pending`.
    fn report(&self, capacity: u64, per_trace: u64) {
        self.unlogged[0].fetch_add(capacity, Ordering::Relaxed);
        self.unlogged[1].fetch_add(per_trace, Ordering::Relaxed);
        let mut last = self.last_log.lock().unwrap_or_else(|p| p.into_inner());
        if last.is_some_and(|t| t.elapsed() < LOG_EVERY) {
            return;
        }
        *last = Some(Instant::now());
        drop(last);
        for (n, reason) in self.unlogged.iter().zip(["capacity", "per_trace"]) {
            let count = n.swap(0, Ordering::Relaxed);
            if count > 0 {
                tracing::warn!(count, reason, "trace.promote.dropped");
            }
        }
    }
}

fn interesting(s: &SpanData) -> bool {
    matches!(s.status, Status::Error { .. }) || s.end_time.duration_since(s.start_time).is_ok_and(|d| d > SLOW)
}

fn sampled(mut s: SpanData) -> SpanData {
    let c = &s.span_context;
    s.span_context = SpanContext::new(c.trace_id(), c.span_id(), c.trace_flags().with_sampled(true), c.is_remote(), c.trace_state().clone());
    s
}

impl<P: SpanProcessor> SpanProcessor for Promote<P> {
    fn on_start(&self, span: &mut Span, cx: &opentelemetry::Context) {
        self.inner.on_start(span, cx);
    }

    fn on_end(&self, mut span: SpanData) {
        if span.span_context.is_sampled() {
            return self.inner.on_end(span);
        }
        let id = span.span_context.trace_id();
        let mut guard = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        let p = &mut *guard;
        if !(span.parent_span_is_remote || span.parent_span_id == SpanId::INVALID) {
            p.seq += 1;
            let w = p.traces.entry(id).or_default();
            w.last = p.seq;
            let mut per_trace = 0;
            if w.spans.len() < self.max_spans {
                w.spans.push(span);
                p.total += 1;
            } else {
                w.dropped += 1;
                per_trace = 1;
            }
            p.order.push_back((id, p.seq));
            let mut evicted = 0;
            while p.total > self.max_pending {
                let Some((old, seq)) = p.order.pop_front() else { break };
                if p.traces.get(&old).is_some_and(|w| w.last == seq) {
                    let n = p.traces.remove(&old).map_or(0, |w| w.spans.len());
                    p.total -= n;
                    evicted += n as u64;
                }
            }
            if p.order.len() > 2 * self.max_pending {
                let traces = &p.traces;
                p.order.retain(|(id, seq)| traces.get(id).is_some_and(|w| w.last == *seq));
            }
            drop(guard);
            if evicted + per_trace > 0 {
                self.report(evicted, per_trace);
            }
            return;
        }
        let w = p.traces.remove(&id).unwrap_or_default();
        p.total -= w.spans.len();
        drop(guard);
        if interesting(&span) {
            if w.dropped > 0 {
                span.attributes.push(KeyValue::new("dropped_children", w.dropped as i64));
            }
            for s in w.spans.into_iter().chain(std::iter::once(span)) {
                self.inner.on_end(sampled(s));
            }
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{SpanKind, TraceFlags, TraceState};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SimpleSpanProcessor, SpanEvents, SpanLinks};
    use std::time::SystemTime;

    fn span(trace: u128, id: u64, parent: u64, remote_parent: bool, took: Duration, status: Status, sampled: bool) -> SpanData {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        SpanData {
            span_context: SpanContext::new(TraceId::from(trace), SpanId::from(id), TraceFlags::default().with_sampled(sampled), false, TraceState::default()),
            parent_span_id: SpanId::from(parent),
            parent_span_is_remote: remote_parent,
            span_kind: SpanKind::Internal,
            name: "s".into(),
            start_time: start,
            end_time: start + took,
            attributes: vec![],
            dropped_attributes_count: 0,
            events: SpanEvents::default(),
            links: SpanLinks::default(),
            status,
            instrumentation_scope: Default::default(),
        }
    }

    type Mem = Promote<SimpleSpanProcessor<InMemorySpanExporter>>;

    fn promote_with(max_pending: usize, max_spans: usize) -> (Mem, InMemorySpanExporter) {
        let out = InMemorySpanExporter::default();
        (Promote::with_limits(SimpleSpanProcessor::new(out.clone()), max_pending, max_spans), out)
    }

    fn promote() -> (Mem, InMemorySpanExporter) {
        promote_with(MAX_PENDING, MAX_SPANS)
    }

    fn child(trace: u128, id: u64) -> SpanData {
        span(trace, id, 1, false, Duration::ZERO, Status::Unset, false)
    }

    fn waiting(p: &Mem, trace: u128) -> usize {
        p.pending.lock().unwrap().traces.get(&TraceId::from(trace)).map_or(0, |w| w.spans.len())
    }

    #[test]
    fn a_sampled_span_passes_straight_through() {
        let (p, out) = promote();
        p.on_end(span(1, 2, 0, false, Duration::from_millis(5), Status::Unset, true));
        assert_eq!(out.get_finished_spans().unwrap().len(), 1);
    }

    #[test]
    fn a_fast_ok_unsampled_trace_is_dropped_whole() {
        let (p, out) = promote();
        p.on_end(span(1, 3, 2, false, Duration::from_millis(5), Status::Unset, false));
        p.on_end(span(1, 2, 0, false, Duration::from_millis(9), Status::Unset, false));
        assert!(out.get_finished_spans().unwrap().is_empty());
        assert_eq!(p.pending.lock().unwrap().total, 0);
    }

    #[test]
    fn an_errored_local_root_keeps_its_children_sampled() {
        let (p, out) = promote();
        p.on_end(span(1, 3, 2, false, Duration::from_millis(5), Status::Unset, false));
        p.on_end(span(1, 2, 99, true, Duration::from_millis(9), Status::error("boom"), false));
        let got = out.get_finished_spans().unwrap();
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|s| s.span_context.is_sampled()));
    }

    #[test]
    fn a_slow_local_root_is_kept() {
        let (p, out) = promote();
        p.on_end(span(1, 2, 0, false, SLOW + Duration::from_millis(1), Status::Unset, false));
        assert_eq!(out.get_finished_spans().unwrap().len(), 1);
    }

    #[test]
    fn pending_is_bounded_by_total_spans() {
        let (p, _) = promote_with(100, 8);
        for t in 0..10_000u128 {
            p.on_end(child(t % 300 + 1, t as u64 + 2));
        }
        let pending = p.pending.lock().unwrap();
        assert!(pending.total <= 100);
        assert!(pending.order.len() <= 200);
    }

    #[test]
    fn overflow_evicts_the_oldest_trace_first() {
        let (p, _) = promote_with(4, 8);
        for t in 1..=5u128 {
            p.on_end(child(t, 2));
        }
        assert_eq!(waiting(&p, 1), 0, "the oldest trace went");
        assert!((2..=5).all(|t| waiting(&p, t) == 1), "the newer ones stayed");
    }

    #[test]
    fn a_still_running_slow_root_keeps_its_children() {
        let (p, out) = promote_with(8, 64);
        p.on_end(child(1, 2));
        for (i, t) in (100..112u128).enumerate() {
            p.on_end(child(t, 2));
            if i % 4 == 3 {
                p.on_end(child(1, 10 + i as u64));
            }
        }
        assert_eq!(waiting(&p, 1), 4);
        p.on_end(span(1, 1, 0, false, SLOW + Duration::from_millis(1), Status::Unset, false));
        assert_eq!(out.get_finished_spans().unwrap().len(), 5);
    }

    #[test]
    fn children_past_the_per_trace_cap_are_counted_on_the_root() {
        let (p, out) = promote_with(100, 2);
        for id in 2..5 {
            p.on_end(child(1, id));
        }
        p.on_end(span(1, 1, 0, false, Duration::ZERO, Status::error("boom"), false));
        let got = out.get_finished_spans().unwrap();
        assert_eq!(got.len(), 3);
        let root = got.iter().find(|s| s.parent_span_id == SpanId::INVALID).unwrap();
        assert!(root.attributes.contains(&KeyValue::new("dropped_children", 1i64)));
    }

    #[test]
    fn a_failed_request_exports_its_whole_tree() {
        let (d, out) = crate::testing::subscriber();
        let mut h = http::HeaderMap::new();
        // An unsampled remote parent: head sampling says RecordOnly, so only promotion can keep it.
        h.insert("traceparent", http::HeaderValue::from_static("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-00"));
        tracing::dispatcher::with_default(&d, || {
            let span = crate::server_span(&http::Method::GET, "/v1/x", &h, "r", "/v1/x");
            span.in_scope(|| drop(tracing::info_span!("child")));
            crate::finish(&span, 500);
        });
        let got = out.get_finished_spans().unwrap();
        assert_eq!(got.len(), 2, "server span and its child");
        assert!(got.iter().all(|s| s.span_context.is_sampled() && s.span_context.trace_id() == TraceId::from_hex("0af7651916cd43dd8448eb211c80319c").unwrap()));
        assert!(got.iter().any(|s| matches!(s.status, Status::Error { .. })));
    }
}
