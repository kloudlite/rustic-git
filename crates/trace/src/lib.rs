//! Distributed tracing for every Rust binary: one OpenTelemetry layer on the one subscriber
//! `kloudlite_core::log` builds, exported OTLP/HTTP to the node-local collector.
//!
//! Off by default: without `KLOUDLITE_OTLP_URL` `layer()` is `None` and every binary runs exactly
//! as before — the `KLOUDLITE_CLICKHOUSE_URL` pattern. With it, spans leave through a
//! `BatchSpanProcessor` on its own thread with a bounded queue: a full queue drops (`try_send`),
//! a dead collector drops, and neither is visible to a request. `tests/exporter_down.rs` holds that.
//!
//! Module map: `sampler` (who is kept at the head, and whether a remote flag is trusted),
//! `promote` (who is kept at the tail: errored or slow local roots, in-process), `http` (the
//! in/out seams), `testing` (an in-memory subscriber for other crates' tests).
//!
//! ponytail: no flush at shutdown — up to one `SCHEDULED_DELAY` of spans is lost on a pod stop.
//! Calling `SdkTracerProvider::shutdown` from each binary's signal path is the upgrade if a
//! drill's trace is ever needed across a roll.

mod http;
mod promote;
mod sampler;
#[cfg(feature = "testing")]
pub mod testing;

pub use http::{client_span, finish, inject, inject_reqwest, server_span, stamp, traced, untraced, UNTRACED};
pub use promote::Promote;
pub use sampler::{bind_ratio, trust_remote_sampled, Sampler, DEFAULT_RATIO};

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{ExporterBuildError, WithExportConfig as _};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{BatchConfigBuilder, BatchSpanProcessor, SdkTracerProvider};
use opentelemetry_sdk::Resource;
use std::time::Duration;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// The same floor the tiers' `http.slow` line uses.
pub const SLOW: Duration = Duration::from_secs(1);
const QUEUE: usize = 2048;
const BATCH: usize = 512;
const SCHEDULED_DELAY: Duration = Duration::from_secs(5);
const EXPORT_TIMEOUT: Duration = Duration::from_secs(5);

pub fn provider(url: &str, service: &str) -> Result<SdkTracerProvider, ExporterBuildError> {
    // The workspace's reqwest is `rustls-no-provider`, and `layer()` runs from `log::init` before
    // a binary's own install: without this the exporter's blocking client panics at build.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        // Used verbatim by the builder: the signal path is ours to append.
        .with_endpoint(format!("{}/v1/traces", url.trim_end_matches('/')))
        .with_timeout(EXPORT_TIMEOUT)
        .build()?;
    let batch = BatchSpanProcessor::builder(exporter)
        .with_batch_config(
            BatchConfigBuilder::default()
                .with_max_queue_size(QUEUE)
                .with_max_export_batch_size(BATCH)
                .with_scheduled_delay(SCHEDULED_DELAY)
                .build(),
        )
        .build();
    Ok(SdkTracerProvider::builder()
        .with_sampler(Sampler)
        .with_span_processor(Promote::new(batch))
        .with_resource(Resource::builder().with_service_name(service.to_string()).build())
        .build())
}

pub fn layer_with<S>(url: &str, service: &str) -> Result<impl Layer<S> + Send + Sync, ExporterBuildError>
where
    // `OpenTelemetryLayer` holds `PhantomData<S>`, so `Send + Sync` on the layer needs it on `S`.
    S: tracing::Subscriber + for<'a> LookupSpan<'a> + Send + Sync,
{
    let provider = provider(url, service)?;
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    // `SdkTracer` owns a clone of the provider, so the batch thread lives as long as the layer.
    let tracer = provider.tracer("kloudlite");
    Ok(tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_filter(filter_fn(|meta| meta.target() != UNTRACED)))
}

pub fn layer<S>() -> Option<impl Layer<S> + Send + Sync>
where
    // `OpenTelemetryLayer` holds `PhantomData<S>`, so `Send + Sync` on the layer needs it on `S`.
    S: tracing::Subscriber + for<'a> LookupSpan<'a> + Send + Sync,
{
    let url = std::env::var("KLOUDLITE_OTLP_URL").ok().filter(|u| !u.is_empty())?;
    let service = std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "kloudlite".into());
    match layer_with(&url, &service) {
        Ok(l) => Some(l),
        Err(e) => {
            // Before the subscriber exists: stderr is the only place this can go.
            eprintln!("trace.init.failed: {e}");
            None
        }
    }
}
