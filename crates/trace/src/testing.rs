//! An in-memory tracing subscriber for propagation tests in other crates.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SimpleSpanProcessor};
use tracing_subscriber::layer::SubscriberExt as _;

pub fn subscriber() -> (tracing::Dispatch, InMemorySpanExporter) {
    let out = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_sampler(crate::Sampler::default())
        .with_span_processor(crate::Promote::new(SimpleSpanProcessor::new(out.clone())))
        .build();
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    let sub = tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
    (tracing::Dispatch::new(sub), out)
}
