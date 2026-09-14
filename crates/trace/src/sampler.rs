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
//!
//! The marker is forgeable, so its effect is BOUNDED rather than secret: every probe-honoured
//! sampled decision spends a token from a per-process bucket (`Sampler::new(rate, burst)`,
//! default 20 traces/s, burst 100). Past the cap the ratio decides, exactly as for an outside
//! caller — a flood costs at most the bucket, and the real probe (a few req/s) never touches it.
//! Only a remote probe root is charged; its children have local parents and are free.

use opentelemetry::trace::{Link, SpanKind, TraceContextExt, TraceId};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{SamplingDecision, SamplingResult, ShouldSample};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

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

pub const PROBE_RATE: f64 = 20.0;
pub const PROBE_BURST: f64 = 100.0;

/// Token bucket for probe-honoured samples. A mutex, not atomics: it is taken only for a remote
/// probe root, a few times a second, never on an ordinary span.
#[derive(Debug)]
pub(crate) struct Bucket {
    rate: f64,
    burst: f64,
    state: Mutex<(f64, Instant)>,
}

impl Bucket {
    pub(crate) fn new(rate: f64, burst: f64) -> Self {
        Self { rate, burst, state: Mutex::new((burst, Instant::now())) }
    }

    pub(crate) fn take(&self) -> bool {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        st.0 = (st.0 + now.duration_since(st.1).as_secs_f64() * self.rate).min(self.burst);
        st.1 = now;
        if st.0 >= 1.0 {
            st.0 -= 1.0;
            true
        } else {
            false
        }
    }
}

#[derive(Clone, Debug)]
pub struct Sampler {
    probes: Arc<Bucket>,
}

impl Sampler {
    /// `rate` probe-honoured traces per second with a `burst` allowance; beyond it the ratio decides.
    pub fn new(rate: f64, burst: f64) -> Self {
        Self { probes: Arc::new(Bucket::new(rate, burst)) }
    }
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new(PROBE_RATE, PROBE_BURST)
    }
}

/// A remote parent that is neither probe-marked nor trusted falls through to the ratio — including
/// our OWN internal hops (api -> srv, agent -> agent). That still keeps a trace whole because
/// `TraceIdRatioBased` is a pure function of the trace id: every process with the same ratio
/// re-rolls the same answer. It diverges only while a ratio change is mid-rollout, or when an
/// upstream kept the trace by promotion (the downstream then promotes only its own failure or
/// stall). Task 2 should trust the sampled flag on the authenticated peer listener
/// (`WS_PEER_SECRET` / peer auth). `trust_remote_sampled` is process-wide, so that needs a
/// per-listener context mark set after peer auth passes, the way `Probe` is — which removes both
/// gaps for internal traffic without trusting the public listener.
pub(crate) fn decide_with(parent: Option<&Context>, trace_id: TraceId, ratio: f64, trust_remote: bool, probes: &Bucket) -> SamplingDecision {
    let probe = parent.is_some_and(|c| c.get::<Probe>().is_some());
    let parent = parent.map(|c| c.span().span_context().clone()).filter(|sc| sc.is_valid());
    if let Some(sc) = parent {
        let obeyed = !sc.is_remote() || trust_remote || (probe && (!sc.is_sampled() || probes.take()));
        if obeyed {
            return if sc.is_sampled() { SamplingDecision::RecordAndSample } else { SamplingDecision::RecordOnly };
        }
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
            decision: decide_with(parent, trace_id, ratio(), TRUST_REMOTE.load(Ordering::Relaxed), &self.probes),
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
        decide_with(parent, TraceId::from(trace), ratio, true, &open())
    }

    fn open() -> Bucket {
        Bucket::new(PROBE_RATE, PROBE_BURST)
    }

    fn remote(flags: TraceFlags) -> Context {
        Context::new().with_remote_span_context(SpanContext::new(TraceId::from(7u128), SpanId::from(9u64), flags, true, TraceState::default()))
    }

    #[test]
    fn a_sampled_parent_keeps_whatever_the_ratio() {
        assert_eq!(decide(Some(&remote(TraceFlags::SAMPLED)), 0.0, 1), SamplingDecision::RecordAndSample);
        let local = Context::new().with_remote_span_context(SpanContext::new(TraceId::from(7u128), SpanId::from(9u64), TraceFlags::SAMPLED, false, TraceState::default()));
        assert_eq!(decide_with(Some(&local), TraceId::from(u128::MAX), 0.0, false, &open()), SamplingDecision::RecordAndSample);
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
        assert_eq!(decide_with(Some(&p), TraceId::from(u128::MAX), 0.0, false, &open()), SamplingDecision::RecordAndSample);
    }

    #[test]
    fn a_probe_flood_keeps_at_most_the_bucket() {
        let bucket = open();
        let p = remote(TraceFlags::SAMPLED).with_value(Probe);
        let started = Instant::now();
        let kept = (0..10_000).filter(|_| decide_with(Some(&p), TraceId::from(u128::MAX), 0.0, false, &bucket) == SamplingDecision::RecordAndSample).count();
        let bound = PROBE_BURST + PROBE_RATE * started.elapsed().as_secs_f64();
        assert!(kept as f64 <= bound, "kept {kept} > {bound}");
        assert!(kept >= PROBE_BURST as usize, "under the cap every probe is kept, got {kept}");
    }

    #[test]
    fn past_the_cap_a_probe_falls_back_to_the_ratio() {
        let empty = Bucket::new(0.0, 0.0);
        let p = remote(TraceFlags::SAMPLED).with_value(Probe);
        assert_eq!(decide_with(Some(&p), TraceId::from(u128::MAX), 0.0, false, &empty), SamplingDecision::RecordOnly);
        assert_eq!(decide_with(Some(&p), TraceId::from(1u128), 1.0, false, &empty), SamplingDecision::RecordAndSample);
    }

    #[test]
    fn an_untrusted_remote_flag_is_replaced_by_the_ratio() {
        let p = remote(TraceFlags::SAMPLED);
        assert_eq!(decide_with(Some(&p), TraceId::from(u128::MAX), 0.0, false, &open()), SamplingDecision::RecordOnly);
        let p = remote(TraceFlags::default());
        assert_eq!(decide_with(Some(&p), TraceId::from(1u128), 1.0, false, &open()), SamplingDecision::RecordAndSample);
    }
}
