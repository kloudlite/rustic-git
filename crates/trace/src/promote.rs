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
//! ponytail: a trace whose root never ends (a cancelled future) parks its children until the map
//! reaches `MAX_TRACES`, at which point the whole map is cleared — those traces are lost, not
//! leaked. A time-ordered eviction is the upgrade if Task 8 shows promoted traces going missing.

use crate::SLOW;
use opentelemetry::trace::{SpanContext, SpanId, Status, TraceId};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{Span, SpanData, SpanProcessor};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

pub(crate) const MAX_TRACES: usize = 4096;
const MAX_SPANS: usize = 512;

#[derive(Debug)]
pub struct Promote<P> {
    inner: P,
    pub(crate) pending: Mutex<HashMap<TraceId, Vec<SpanData>>>,
}

impl<P> Promote<P> {
    pub fn new(inner: P) -> Self {
        Self { inner, pending: Mutex::new(HashMap::new()) }
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

    fn on_end(&self, span: SpanData) {
        if span.span_context.is_sampled() {
            return self.inner.on_end(span);
        }
        let id = span.span_context.trace_id();
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        if !(span.parent_span_is_remote || span.parent_span_id == SpanId::INVALID) {
            if pending.len() >= MAX_TRACES && !pending.contains_key(&id) {
                pending.clear();
            }
            let waiting = pending.entry(id).or_default();
            if waiting.len() < MAX_SPANS {
                waiting.push(span);
            }
            return;
        }
        let children = pending.remove(&id).unwrap_or_default();
        drop(pending);
        if interesting(&span) {
            for s in children.into_iter().chain(std::iter::once(span)) {
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

    fn promote() -> (Promote<SimpleSpanProcessor<InMemorySpanExporter>>, InMemorySpanExporter) {
        let out = InMemorySpanExporter::default();
        (Promote::new(SimpleSpanProcessor::new(out.clone())), out)
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
    fn pending_is_bounded() {
        let (p, _) = promote();
        for t in 0..(MAX_TRACES as u128 + 10) {
            p.on_end(span(t + 1, 3, 2, false, Duration::ZERO, Status::Unset, false));
        }
        assert!(p.pending.lock().unwrap().len() <= MAX_TRACES);
    }
}
