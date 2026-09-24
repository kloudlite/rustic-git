//! An in-memory tracing subscriber for propagation tests in other crates.

use std::sync::OnceLock;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SimpleSpanProcessor};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::Layer as _;

pub fn subscriber() -> (tracing::Dispatch, InMemorySpanExporter) {
    // tracing-core 0.1.36 (`callsite.rs`, `has_just_one`): while at most one dispatcher is
    // registered, a callsite's interest comes from the CALLING thread's default alone. A parallel
    // test with no subscriber that first hits a shared callsite (`http::server_span`) then caches
    // `never` for everyone, and a traced test sees no spans (ide `server::tests`, 13 of 15 full-lib
    // runs, 2026-09-24). A second dispatcher held for the whole process keeps the count above one,
    // so interest is the union of every live dispatcher.
    static PIN: OnceLock<tracing::Dispatch> = OnceLock::new();
    PIN.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));
    let out = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_sampler(crate::Sampler::default())
        .with_span_processor(crate::Promote::new(SimpleSpanProcessor::new(out.clone())))
        .build();
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    let sub = tracing_subscriber::registry().with(
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("test"))
            // The production filter, so a test of an untraced path exercises what ships.
            .with_filter(tracing_subscriber::filter::filter_fn(crate::exported)),
    );
    (tracing::Dispatch::new(sub), out)
}
