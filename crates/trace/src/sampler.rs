//! Parent-based head sampling whose root ratio is a Live setting — and which never DROPS.
//!
//! A span that is not sampled is still recorded (`RecordOnly`) so `Promote` can keep the whole
//! trace when its local root turns out to have failed or been slow; a `Drop` here would make
//! "always keep errors" impossible to honour, because the decision is taken before the outcome
//! exists. The cost of that is paid in Task 8's measurement, not assumed away.
//!
//! The ratio is read through a closure bound by each binary next to its `LiveSettings`
//! (`bind_ratio`), so a settings save reaches the sampler on the reader's next beat and there is
//! no second copy of the value to hand-sync. Before a binary binds one, roots use `DEFAULT_RATIO`.
//!
//! Whether a REMOTE parent's sampled flag is obeyed is the one trust decision for Rust services.
//! The trace id is always continued (a trace stays whole); only keep/record is at stake. A remote
//! flag is obeyed only when the parent context carries `Probe` — which `http::server_span` adds
//! for a request `http::is_probe` accepts, and `http::inject` passes on to the next hop — or when a
//! process sets `trust_remote_sampled(true)`. Default `false`, because `/v1` reaches api without
//! passing ingress, so any outside caller could otherwise force every hop to sample.

use opentelemetry::trace::{Link, SpanKind, TraceContextExt, TraceId};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{SamplingDecision, SamplingResult, ShouldSample};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

pub const DEFAULT_RATIO: f64 = 0.1;

type Ratio = Box<dyn Fn() -> f64 + Send + Sync>;
static RATIO: OnceLock<Ratio> = OnceLock::new();
static TRUST_REMOTE: AtomicBool = AtomicBool::new(false);

/// Context value: this request is probe traffic, so its remote sampled flag is obeyed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Probe;

/// Bind the root ratio to a live settings handle. First call wins; a binary binds exactly once.
pub fn bind_ratio(f: impl Fn() -> f64 + Send + Sync + 'static) {
    let _ = RATIO.set(Box::new(f));
}

/// Obey (`true`) or ignore (`false`, the default) a remote caller's sampled flag. See the module doc.
pub fn trust_remote_sampled(yes: bool) {
    TRUST_REMOTE.store(yes, Ordering::Relaxed);
}

fn ratio() -> f64 {
    RATIO.get().map_or(DEFAULT_RATIO, |f| f()).clamp(0.0, 1.0)
}

#[derive(Clone, Debug, Default)]
pub struct Sampler;

pub(crate) fn decide_with(parent: Option<&Context>, trace_id: TraceId, ratio: f64, trust_remote: bool) -> SamplingDecision {
    let probe = parent.is_some_and(|c| c.get::<Probe>().is_some());
    let parent = parent.map(|c| c.span().span_context().clone()).filter(|sc| sc.is_valid());
    if let Some(sc) = parent.filter(|sc| trust_remote || probe || !sc.is_remote()) {
        return if sc.is_sampled() { SamplingDecision::RecordAndSample } else { SamplingDecision::RecordOnly };
    }
    let root = opentelemetry_sdk::trace::Sampler::TraceIdRatioBased(ratio);
    match root.should_sample(None, trace_id, "", &SpanKind::Internal, &[], &[]).decision {
        SamplingDecision::RecordAndSample => SamplingDecision::RecordAndSample,
        _ => SamplingDecision::RecordOnly,
    }
}

impl ShouldSample for Sampler {
    fn should_sample(&self, parent: Option<&Context>, trace_id: TraceId, _: &str, _: &SpanKind, _: &[KeyValue], _: &[Link]) -> SamplingResult {
        SamplingResult {
            decision: decide_with(parent, trace_id, ratio(), TRUST_REMOTE.load(Ordering::Relaxed)),
            attributes: Vec::new(),
            trace_state: parent.map(|c| c.span().span_context().trace_state().clone()).unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{SpanContext, SpanId, TraceFlags, TraceState};

    fn decide(parent: Option<&Context>, ratio: f64, trace: u128) -> SamplingDecision {
        decide_with(parent, TraceId::from(trace), ratio, true)
    }

    fn remote(flags: TraceFlags) -> Context {
        Context::new().with_remote_span_context(SpanContext::new(TraceId::from(7u128), SpanId::from(9u64), flags, true, TraceState::default()))
    }

    #[test]
    fn a_sampled_parent_keeps_whatever_the_ratio() {
        assert_eq!(decide(Some(&remote(TraceFlags::SAMPLED)), 0.0, 1), SamplingDecision::RecordAndSample);
        let local = Context::new().with_remote_span_context(SpanContext::new(TraceId::from(7u128), SpanId::from(9u64), TraceFlags::SAMPLED, false, TraceState::default()));
        assert_eq!(decide_with(Some(&local), TraceId::from(u128::MAX), 0.0, false), SamplingDecision::RecordAndSample);
    }

    #[test]
    fn an_unsampled_parent_records_but_never_samples() {
        assert_eq!(decide(Some(&remote(TraceFlags::default())), 1.0, 1), SamplingDecision::RecordOnly);
    }

    #[test]
    fn a_root_follows_the_ratio_and_is_never_dropped() {
        assert_eq!(decide(None, 1.0, 1), SamplingDecision::RecordAndSample);
        assert_eq!(decide(None, 0.0, u128::MAX), SamplingDecision::RecordOnly);
    }

    #[test]
    fn an_empty_context_is_a_root() {
        assert_eq!(decide(Some(&Context::new()), 0.0, u128::MAX), SamplingDecision::RecordOnly);
    }

    #[test]
    fn untrusted_is_the_default() {
        assert!(!TRUST_REMOTE.load(Ordering::Relaxed));
    }

    #[test]
    fn a_probe_marked_remote_flag_is_obeyed_untrusted() {
        let p = remote(TraceFlags::SAMPLED).with_value(Probe);
        assert_eq!(decide_with(Some(&p), TraceId::from(u128::MAX), 0.0, false), SamplingDecision::RecordAndSample);
    }

    #[test]
    fn an_untrusted_remote_flag_is_replaced_by_the_ratio() {
        let p = remote(TraceFlags::SAMPLED);
        assert_eq!(decide_with(Some(&p), TraceId::from(u128::MAX), 0.0, false), SamplingDecision::RecordOnly);
        let p = remote(TraceFlags::default());
        assert_eq!(decide_with(Some(&p), TraceId::from(1u128), 1.0, false), SamplingDecision::RecordAndSample);
    }
}
