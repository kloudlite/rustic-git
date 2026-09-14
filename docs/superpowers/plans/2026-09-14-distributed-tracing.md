# Distributed Tracing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One W3C trace per request across probe → ingress-nginx → web/api → srv, api→srv, agent→agent, builder-gate → api and bench → workspace tool server, visible in HyperDX's trace waterfall and service map, with logs linked by `trace_id`.

**Architecture:** A new `kloudlite-trace` crate owns the Rust side: a `tracing-opentelemetry` layer added to the one subscriber `kloudlite_core::log` already builds, a parent-based sampler whose root ratio is a Live setting, a `Promote` span processor that keeps unsampled traces whose local root errored or ran over 1 s, and two HTTP helpers (`server_span` for inbound, `inject` for outbound). Node (web, bench) mirrors the same three pieces with the OpenTelemetry JS SDK. Everything exports OTLP/HTTP to the node-local `kloudlite-otel-agent` collector, which gets a traces pipeline; ClickStack's gateway writes `default.otel_traces`, which HyperDX's Traces source already reads.

**Tech Stack:** Rust: `opentelemetry` 0.32.0, `opentelemetry_sdk` 0.32.1, `opentelemetry-otlp` 0.32.0, `opentelemetry-http` 0.32.0, `tracing-opentelemetry` 0.33.0 (fits the lock's `tracing` 0.1.44, `tracing-subscriber` 0.3.23, `reqwest` 0.13.4, `axum` 0.8.9, `http` 1.x). Node: `@opentelemetry/api` 1.9.1, `sdk-trace-node`/`sdk-trace-base`/`resources` 2.11.0, `exporter-trace-otlp-proto`/`instrumentation`/`instrumentation-http` 0.222.0, `instrumentation-undici` 0.32.0. Next.js 16.3.4 (built-in spans through the global `@opentelemetry/api` provider). ingress-nginx's built-in OpenTelemetry module.

**Spec:** Owner decisions in the request of 2026-09-14 (copied into Global Constraints below); `CLAUDE.md` "History and telemetry" and "Live settings".

**Vendoring note.** None of the OpenTelemetry crates or npm packages are in `Cargo.lock`, `~/.cargo/registry` or any `node_modules` today. Every signature this plan uses was read from the published sources of the exact versions above (crate tarballs from static.crates.io, `npm pack` tarballs): `ShouldSample::should_sample`, `SamplingResult`, `SpanProcessor` (`on_start`/`on_end`/`force_flush`/`shutdown_with_timeout`), `BatchSpanProcessor::builder(..).with_batch_config(..)`, `BatchConfigBuilder::default().with_max_queue_size/with_max_export_batch_size/with_scheduled_delay`, `SdkTracerProvider::builder().with_sampler/with_span_processor/with_resource`, `Resource::builder().with_service_name`, `SpanExporter::builder().with_http()` + `WithExportConfig::{with_endpoint, with_timeout}` (a programmatic endpoint is used verbatim — `/v1/traces` must be appended), `OpenTelemetrySpanExt::{set_parent -> Result<(), SetParentError>, context}`, the `otel.name`/`otel.kind`/`otel.status_code` fields (honoured on `on_record` after the span is built too), `HeaderInjector`/`HeaderExtractor`, `TraceContextPropagator`; JS: `TracerConfig.{sampler, spanProcessors}`, `ReadableSpan.{parentSpanContext, status, duration}`, `SpanProcessor.{onStart,onEnd,forceFlush,shutdown}`, `BatchSpanProcessor` (drops spans whose flags lack SAMPLED), `IgnoreIncomingRequestFunction(request)`, `OTLPTraceExporter({url, timeoutMillis})`. Task 1 Step 1 re-checks them in the registry once `cargo fetch` has vendored them; if any moved, stop and report rather than adapt silently.

## Global Constraints

- Propagation is W3C `traceparent`/`tracestate` only (`TraceContextPropagator` / the JS SDK default). No B3, no vendor headers.
- One trace spans: probe → ingress-nginx → web (Node) or api → srv; api → srv peer hop; agent → agent peer (`/peer/v1/snapshot`, `/peer/v1/wake`); builder-gate → api `/v1/internal/builders/*`; bench (Node, `harness/bench`) → workspace tool server (`kl ide serve`, `crates/ide`). Kubernetes API calls are CLIENT spans from `crates/workspaces/src/k8s/client.rs` carrying `kube.timeout_layer = inner|outer` when a bound fires.
- `x-request-id` stays exactly as it is. Every span we open declares `trace_id` and records it, so a JSON log line inside it carries `span.trace_id`, which the collector's `trace_parser` turns into the log record's TraceId.
- Export is OTLP/HTTP protobuf, batched on the SDK's own thread (Rust) / timer (Node), never on the request path. Queue 2048 spans, batch 512, delay 5 s, export timeout 5 s. A full queue drops; a collector outage drops; neither may block or fail a request.
- `KLOUDLITE_OTLP_URL` is optional everywhere (the `KLOUDLITE_CLICKHOUSE_URL` pattern): unset means no tracing layer and exactly today's behaviour. `OTEL_SERVICE_NAME` names the service. These are boot wiring, not tunables.
- Sampling is parent-based so a trace is kept or dropped whole: a sampled parent keeps, an unsampled parent records-but-does-not-sample. A root samples at the ratio; everything not sampled is still RECORDED so `Promote` can keep it when its local root ends with ERROR status or after more than 1 s. Probe traffic is marked by the probe sending `traceparent` with the sampled flag (`-01`), which parent-based sampling keeps at every hop.
- The root ratio is `traceSampleRatio`, default `0.1`, range `0.0..=1.0`: in the central `cluster/settings` document for server/api/worker/gateway/builder-gate, and in `ClusterSettings/default` for the agent. Read only through `LiveSettings` (`kloudlite_trace::bind_ratio(move || settings.load().trace_sample_ratio)`), never `std::env::var`. Node roots (rare — nginx or a Rust tier is almost always the parent) use a constant 0.1.
- No per-chunk or per-object spans: git pack, registry blob streams and `btrfs send` bodies get the one request span only. No span per watch event — one `reconcile` span per pass. Watches (`k8s::client::is_watch`) get no client span. `/healthz`, `/readyz`, `/livez`, `/metrics`, `/api/health` are never traced (their span uses target `kloudlite_trace::untraced`, which the OpenTelemetry layer filters out).
- Node uses only `@opentelemetry/instrumentation-http` and `@opentelemetry/instrumentation-undici` (fetch) plus Next.js's built-in spans. No `@opentelemetry/sdk-node`, no auto-instrumentations bundle. Web registers from `web/apps/web/src/instrumentation-node.ts`; bench from `harness/bench/src/main.ts`'s server path.
- ingress-nginx tracing is enabled only through keys in `deploy/ingress-nginx-config.yaml`.
- Traces retention in ClickHouse is 7 days: `ALTER TABLE default.otel_traces MODIFY TTL toDate(Timestamp) + INTERVAL 7 DAY`, documented in `deploy/clickstack/README.md`. The exporter's single `ttl: 720h` in `deploy/clickstack/clickstack-values.yaml` applies to every table it creates and only at `CREATE … IF NOT EXISTS`, so it cannot express a per-table TTL and does not undo the ALTER.
- Merge gate (Task 8): no probe p95 regression above 5 % and no CPU/memory regression above 5 % on srv/api/agent/web under the fixed load; otherwise lower `traceSampleRatio` or remove spans until it holds.
- House style: module `//!` docs carry the why, files stay under ~800 lines, `// ponytail:` marks deliberate ceilings, commit subjects imperative sentence case, no attribution lines. Build, test and ship from the dev pod per `CLAUDE.md` "Dev loop".

---

## File Structure

| Path | Responsibility |
|---|---|
| `crates/trace/Cargo.toml` (new) | `kloudlite-trace` crate: OpenTelemetry deps, `testing` feature |
| `crates/trace/src/lib.rs` (new) | module doc, `layer()`/`layer_with()`, provider + exporter build, constants |
| `crates/trace/src/sampler.rs` (new) | `Sampler` (parent-based, Live root ratio, never drops), `bind_ratio` |
| `crates/trace/src/promote.rs` (new) | `Promote<P>`: keep unsampled traces whose local root errored or was slow |
| `crates/trace/src/http.rs` (new) | `server_span`, `finish`, `client_span`, `stamp`, `inject`, `inject_reqwest`, `traced` middleware, `untraced` |
| `crates/trace/src/testing.rs` (new, feature `testing`) | in-memory subscriber for other crates' propagation tests |
| `crates/trace/tests/exporter_down.rs` (new) | a dead collector never fails or slows a request |
| `crates/core/src/log.rs` | add the trace layer to the one subscriber |
| `crates/core/src/metrics.rs` | `http_metrics` opens the server span through `server_span` |
| `crates/core/src/settings.rs` | `trace_sample_ratio` on `CentralSettings` + stored twin + history + meta + range |
| `crates/api/src/forward.rs`, `images.rs`, `repos.rs`, `signatures.rs` | api → srv `traceparent` |
| `bins/{server,api,worker,gateway,builder-gate}/src/main.rs`, `crates/api/src/lib.rs` | `bind_ratio` next to each central `LiveSettings` |
| `bins/slo/src/stages/mod.rs` | the probe mints a sampled `traceparent` per request |
| `crates/workspaces/src/crd/settings.rs`, `crates/workspaces/src/settings.rs`, `crates/workspaces/src/api/admin/settings.rs` | `traceSampleRatio` in `ClusterSettings` |
| `bins/agent/src/lib.rs`, `peer/mod.rs`, `peer/pull.rs`, `peer/wake.rs`, `controller/run.rs` | agent ratio binding, peer server/client propagation, reconcile span |
| `crates/workspaces/src/k8s/client.rs` | kube client spans with the timeout layer |
| `bins/builder-gate/src/lib.rs` | gate → api propagation, gate server span |
| `crates/ide/src/server.rs`, `bins/kl/src/main.rs` | tool server span + exporter |
| `crates/workspaces/src/k8s/policies.rs`, `k8s/workspace.rs`, `k8s/bench.rs` | tenant pods may reach the node collector's OTLP port; OTLP env |
| `web/apps/web/src/lib/tracing.ts` (new), `src/instrumentation-node.ts`, `test-node/tracing.test.ts` (new) | Node SDK for web |
| `harness/bench/src/tracing.ts` (new), `src/main.ts`, `src/rpc-child.ts`, `harness/pi/workspace-tools.ts`, `bench/test/tracing.test.ts` (new) | Node SDK for bench; tool-call `traceparent` |
| `deploy/k3s/otel-agent.yaml`, `deploy/kloudlite.yaml` | OTLP receiver, traces pipeline, `trace_parser`, node-local Service, service env |
| `deploy/ingress-nginx-config.yaml` | nginx OpenTelemetry keys |
| `deploy/clickstack/README.md` | the 7-day traces TTL step |
| `deploy/dev/trace-perf.sh` (new) | Task 8's before/after measurement |

---

### Task 1: The `kloudlite-trace` crate

**Files:**
- Create: `crates/trace/Cargo.toml`, `crates/trace/src/lib.rs`, `crates/trace/src/sampler.rs`, `crates/trace/src/promote.rs`, `crates/trace/src/http.rs`, `crates/trace/src/testing.rs`, `crates/trace/tests/exporter_down.rs`
- Modify: `Cargo.toml` (workspace `members` and `[workspace.dependencies]`)

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces:
  - `kloudlite_trace::layer<S>() -> Option<impl tracing_subscriber::Layer<S> + Send + Sync>` (reads `KLOUDLITE_OTLP_URL`, `OTEL_SERVICE_NAME`)
  - `kloudlite_trace::layer_with<S>(url: &str, service: &str) -> Result<impl Layer<S> + Send + Sync, opentelemetry_otlp::ExporterBuildError>`
  - `kloudlite_trace::bind_ratio(f: impl Fn() -> f64 + Send + Sync + 'static)`; `kloudlite_trace::DEFAULT_RATIO: f64 = 0.1`
  - `kloudlite_trace::server_span(method: &http::Method, path: &str, headers: &http::HeaderMap, req_id: &str, route: &str) -> tracing::Span`
  - `kloudlite_trace::finish(span: &tracing::Span, status: u16)`
  - `kloudlite_trace::client_span(name: &'static str, method: &str, path: &str) -> tracing::Span`
  - `kloudlite_trace::stamp(span: &tracing::Span)`
  - `kloudlite_trace::inject(headers: &mut http::HeaderMap)`; `kloudlite_trace::inject_reqwest(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder`
  - `kloudlite_trace::traced(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response`
  - `kloudlite_trace::UNTRACED: &str`, `kloudlite_trace::untraced(path: &str) -> bool`, `kloudlite_trace::SLOW: Duration`
  - feature `testing`: `kloudlite_trace::testing::subscriber() -> (tracing::Dispatch, opentelemetry_sdk::trace::InMemorySpanExporter)`

- [ ] **Step 1: Add the crate and vendor its dependencies, then check the cited signatures**

Workspace `Cargo.toml`: add `"crates/trace"` to `members`, and under `[workspace.dependencies]`:

```toml
# Tracing export. Versions move together: tracing-opentelemetry 0.33 is built against
# opentelemetry 0.32, and opentelemetry-otlp 0.32's reqwest feature is reqwest 0.13 — the one
# already in the lock, so no second HTTP client version is pulled in.
opentelemetry = { version = "0.32", default-features = false, features = ["trace"] }
opentelemetry_sdk = { version = "0.32", default-features = false, features = ["trace"] }
opentelemetry-otlp = { version = "0.32", default-features = false, features = ["trace", "http-proto", "reqwest-blocking-client"] }
opentelemetry-http = { version = "0.32", default-features = false }
tracing-opentelemetry = { version = "0.33", default-features = false }
kloudlite-trace = { path = "crates/trace" }
```

`crates/trace/Cargo.toml`:

```toml
[package]
name = "kloudlite-trace"
version = "0.1.0"
edition = "2021"
license = "SSPL-1.0"

[dependencies]
axum = { workspace = true }
reqwest = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
opentelemetry = { workspace = true }
opentelemetry_sdk = { workspace = true }
opentelemetry-otlp = { workspace = true }
opentelemetry-http = { workspace = true }
tracing-opentelemetry = { workspace = true }

[features]
testing = ["opentelemetry_sdk/testing"]

[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt", "net", "time"] }
opentelemetry_sdk = { workspace = true, features = ["testing"] }
```

Run: `cargo fetch && R=~/.cargo/registry/src/index.crates.io-*; grep -n 'fn should_sample' $R/opentelemetry_sdk-0.32.*/src/trace/sampler.rs; grep -n 'fn with_max_queue_size\|fn with_scheduled_delay' $R/opentelemetry_sdk-0.32.*/src/trace/span_processor.rs; grep -n 'fn set_parent' $R/tracing-opentelemetry-0.33.*/src/span_ext.rs; grep -n 'std::thread::spawn' $R/opentelemetry-otlp-0.32.*/src/exporter/http/mod.rs`
Expected: each grep prints a line. The last proves the blocking reqwest client is built on its own thread, so building the exporter inside a tokio runtime does not panic.

- [ ] **Step 2: Write the failing sampler and promotion tests**

`crates/trace/src/sampler.rs` (tests at the bottom of the file you create in Step 4; write them first):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{SpanContext, SpanId, TraceContextExt, TraceFlags, TraceState};

    fn decide(parent: Option<&Context>, ratio: f64, trace: u128) -> SamplingDecision {
        decide_with(parent, TraceId::from(trace), ratio)
    }

    fn remote(flags: TraceFlags) -> Context {
        Context::new().with_remote_span_context(SpanContext::new(TraceId::from(7u128), SpanId::from(9u64), flags, true, TraceState::default()))
    }

    #[test]
    fn a_sampled_parent_keeps_whatever_the_ratio() {
        assert_eq!(decide(Some(&remote(TraceFlags::SAMPLED)), 0.0, 1), SamplingDecision::RecordAndSample);
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
}
```

`crates/trace/src/promote.rs` tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{SpanKind, TraceState};
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
```

If `SpanData` has fields beyond those listed (check `grep -n 'pub ' $R/opentelemetry_sdk-0.32.*/src/trace/export.rs` between `pub struct SpanData` and its closing brace), add them with their `Default` value; the test helper must name every field.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p kloudlite-trace`
Expected: FAIL to compile — `decide_with`, `Promote`, `MAX_TRACES` not found.

- [ ] **Step 4: Write `sampler.rs`**

```rust
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

use opentelemetry::trace::{Link, SamplingDecision, SpanKind, TraceContextExt, TraceId};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{SamplingResult, ShouldSample};
use std::sync::OnceLock;

pub const DEFAULT_RATIO: f64 = 0.1;

type Ratio = Box<dyn Fn() -> f64 + Send + Sync>;
static RATIO: OnceLock<Ratio> = OnceLock::new();

/// Bind the root ratio to a live settings handle. First call wins; a binary binds exactly once.
pub fn bind_ratio(f: impl Fn() -> f64 + Send + Sync + 'static) {
    let _ = RATIO.set(Box::new(f));
}

fn ratio() -> f64 {
    RATIO.get().map_or(DEFAULT_RATIO, |f| f()).clamp(0.0, 1.0)
}

#[derive(Clone, Debug, Default)]
pub struct Sampler;

pub(crate) fn decide_with(parent: Option<&Context>, trace_id: TraceId, ratio: f64) -> SamplingDecision {
    if let Some(sc) = parent.map(|c| c.span().span_context().clone()).filter(|sc| sc.is_valid()) {
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
            decision: decide_with(parent, trace_id, ratio()),
            attributes: Vec::new(),
            trace_state: parent.map(|c| c.span().span_context().trace_state().clone()).unwrap_or_default(),
        }
    }
}
```

- [ ] **Step 5: Write `promote.rs`**

```rust
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
use opentelemetry::trace::{SpanContext, SpanId, Status, TraceFlags, TraceId};
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
```

Verify `TraceFlags` needs no import beyond `with_sampled` (it is a method on the value); remove the unused `TraceFlags` import if clippy flags it.

- [ ] **Step 6: Write `http.rs`**

```rust
//! The two seams every hop uses: `server_span` on the way in, `inject` on the way out.
//!
//! Inbound: the caller's `traceparent` becomes the parent of one server span per request, named by
//! the route CLASS (bounded cardinality), never the raw path. Health and metrics paths get a span
//! under `UNTRACED`, which the OpenTelemetry layer filters out, so `req_id` still lands on their
//! log lines but they never reach a trace.
//!
//! Outbound: `inject` writes the CURRENT span's context. There is no client span for a plain
//! hop — the callee's server span is the child of the caller's server span, which is what the
//! waterfall and the service map need; a client span per hop would double the span count for a
//! row that says "the network took 1 ms".

use opentelemetry::global;
use opentelemetry::trace::TraceContextExt;
use opentelemetry_http::{HeaderExtractor, HeaderInjector};
use tracing_opentelemetry::OpenTelemetrySpanExt;

pub const UNTRACED: &str = "kloudlite_trace::untraced";

pub fn untraced(path: &str) -> bool {
    matches!(path, "/healthz" | "/readyz" | "/livez" | "/metrics" | "/api/health")
}

/// Record this span's trace id as the `trace_id` field, so JSON log lines inside it carry
/// `span.trace_id`. A no-op when no tracing layer is installed (the context is invalid).
pub fn stamp(span: &tracing::Span) {
    let sc = span.context().span().span_context().clone();
    if sc.is_valid() {
        span.record("trace_id", sc.trace_id().to_string());
    }
}

pub fn server_span(method: &http::Method, path: &str, headers: &http::HeaderMap, req_id: &str, route: &str) -> tracing::Span {
    if untraced(path) {
        return tracing::info_span!(target: UNTRACED, "http", req_id = %req_id);
    }
    let span = tracing::info_span!(
        "http",
        otel.name = %format!("{method} {route}"),
        otel.kind = "server",
        otel.status_code = tracing::field::Empty,
        http.request.method = %method,
        url.path = %path,
        http.response.status_code = tracing::field::Empty,
        req_id = %req_id,
        trace_id = tracing::field::Empty,
    );
    let parent = global::get_text_map_propagator(|p| p.extract(&HeaderExtractor(headers)));
    // Err only when no OpenTelemetry layer is installed — exactly the case with nothing to do.
    let _ = span.set_parent(parent);
    stamp(&span);
    span
}

pub fn finish(span: &tracing::Span, status: u16) {
    span.record("http.response.status_code", status);
    if status >= 500 {
        span.record("otel.status_code", "ERROR");
    }
}

pub fn client_span(name: &'static str, method: &str, path: &str) -> tracing::Span {
    let span = tracing::info_span!(
        "client",
        otel.name = name,
        otel.kind = "client",
        otel.status_code = tracing::field::Empty,
        http.request.method = %method,
        url.path = %path,
        kube.timeout_layer = tracing::field::Empty,
        trace_id = tracing::field::Empty,
    );
    stamp(&span);
    span
}

pub fn inject(headers: &mut http::HeaderMap) {
    let cx = tracing::Span::current().context();
    global::get_text_map_propagator(|p| p.inject_context(&cx, &mut HeaderInjector(headers)));
}

pub fn inject_reqwest(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let mut h = http::HeaderMap::new();
    inject(&mut h);
    req.headers(h)
}

/// For listeners that do not mount `kloudlite_core::metrics::http_metrics` (agent peer, tool
/// server, builder gate): the same server span, no metrics.
pub async fn traced(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    use tracing::Instrument;
    let (method, path) = (req.method().clone(), req.uri().path().to_string());
    let span = server_span(&method, &path, req.headers(), "", &path);
    let res = next.run(req).instrument(span.clone()).await;
    finish(&span, res.status().as_u16());
    res
}
```

Add `http = "1"` to `crates/trace/Cargo.toml` `[dependencies]` (already in the lock through axum). `traced` names spans by path because those three routers have a handful of fixed routes each, with ids only in `/peer/v1/snapshot/{volume}/{name}` and `/stream/*/{id}`; replace ids there: pass `req.extensions().get::<axum::extract::MatchedPath>().map(|m| m.as_str().to_string()).unwrap_or(path.clone())` as `route` instead of `&path`.

- [ ] **Step 7: Write `lib.rs` and `testing.rs`**

`lib.rs`:

```rust
//! Distributed tracing for every Rust binary: one OpenTelemetry layer on the one subscriber
//! `kloudlite_core::log` builds, exported OTLP/HTTP to the node-local collector.
//!
//! Off by default: without `KLOUDLITE_OTLP_URL` `layer()` is `None` and every binary runs exactly
//! as before — the `KLOUDLITE_CLICKHOUSE_URL` pattern. With it, spans leave through a
//! `BatchSpanProcessor` on its own thread with a bounded queue: a full queue drops (`try_send`),
//! a dead collector drops, and neither is visible to a request. `tests/exporter_down.rs` holds that.
//!
//! Module map: `sampler` (who is kept at the head), `promote` (who is kept at the tail),
//! `http` (the in/out seams), `testing` (an in-memory subscriber for other crates' tests).
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
pub use sampler::{bind_ratio, Sampler, DEFAULT_RATIO};

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
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    let provider = provider(url, service)?;
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    let tracer = provider.tracer("kloudlite");
    Ok(tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_filter(filter_fn(|meta| meta.target() != UNTRACED)))
}

pub fn layer<S>() -> Option<impl Layer<S> + Send + Sync>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
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
```

The provider is moved into the tracer: `SdkTracer` holds the provider's inner `Arc`, so the batch thread lives as long as the layer. Confirm with `grep -n 'provider' $R/opentelemetry_sdk-0.32.*/src/trace/tracer.rs | head` that `SdkTracer` stores the provider; if it holds only a `Weak`, add `static PROVIDER: OnceLock<SdkTracerProvider>` in `layer_with` and `let _ = PROVIDER.set(provider.clone());`.

`testing.rs`:

```rust
//! An in-memory tracing subscriber for propagation tests in other crates.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SimpleSpanProcessor};
use tracing_subscriber::layer::SubscriberExt as _;

pub fn subscriber() -> (tracing::Dispatch, InMemorySpanExporter) {
    let out = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_sampler(crate::Sampler)
        .with_span_processor(crate::Promote::new(SimpleSpanProcessor::new(out.clone())))
        .build();
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    let sub = tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
    (tracing::Dispatch::new(sub), out)
}
```

- [ ] **Step 8: Write the exporter-down test**

`crates/trace/tests/exporter_down.rs`:

```rust
//! A collector that is not there must cost a request nothing: no error, no stall.

use std::time::{Duration, Instant};
use tracing_subscriber::layer::SubscriberExt as _;

#[tokio::test(flavor = "current_thread")]
async fn a_dead_collector_never_fails_or_slows_a_request() {
    // Port 9 (discard) on loopback: nothing listens, every export fails fast or times out.
    let layer = kloudlite_trace::layer_with("http://127.0.0.1:9", "test").expect("exporter builds without connecting");
    let _g = tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));
    kloudlite_trace::bind_ratio(|| 1.0);

    let app = axum::Router::new()
        .route("/x", axum::routing::get(|| async { "ok" }))
        .layer(axum::middleware::from_fn(kloudlite_trace::traced));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let http = reqwest::Client::new();
    let started = Instant::now();
    // More requests than the queue holds, so the full-queue drop path runs too.
    for _ in 0..3_000 {
        let r = http.get(format!("http://{addr}/x")).header("traceparent", "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01").send().await.unwrap();
        assert_eq!(r.status(), 200);
    }
    assert!(started.elapsed() < Duration::from_secs(10), "3000 loopback requests took {:?}", started.elapsed());
}
```

Add to `crates/trace/Cargo.toml` `[dev-dependencies]`: `reqwest = { workspace = true }`, `tracing-subscriber = { workspace = true }`.

- [ ] **Step 9: Run the tests**

Run: `cargo test -p kloudlite-trace --features testing && cargo clippy -p kloudlite-trace --all-targets --features testing -- -D warnings`
Expected: PASS, no warnings. For a baseline on the 10 s bound, run the same test once with the `.layer(...)` line removed and note the time in the commit body.

- [ ] **Step 10: Commit**

```bash
git add Cargo.toml Cargo.lock crates/trace
git commit -m "Add the kloudlite-trace crate with a Live-ratio sampler and tail promotion"
```

---

### Task 2: Trace the central tier (srv, api, gateway, worker, probe) end to end

**Files:**
- Modify: `crates/core/Cargo.toml`, `crates/core/src/log.rs`, `crates/core/src/metrics.rs:156-200`, `crates/core/src/settings.rs` (`CentralSettings`, `built_in_defaults`, `merged_with`, `StoredCentralSettings`, `StoredCentralSettingsSnapshot`, its `From`, `push_history`, `CENTRAL_SETTING_META`, `validate_stored`)
- Modify: `crates/api/Cargo.toml`, `crates/api/src/forward.rs:110-127`, `crates/api/src/images.rs:47-53`, `crates/api/src/repos.rs` (sends at ~204, ~440, ~967), `crates/api/src/signatures.rs` (sends at ~76, ~123), `crates/api/src/lib.rs:139`
- Modify: `bins/server/src/main.rs:~100`, `bins/worker/src/main.rs:78`, `bins/gateway/src/main.rs:~60`, `bins/slo/src/stages/mod.rs:~182`
- Modify: `deploy/k3s/otel-agent.yaml`, `deploy/kloudlite.yaml` (collector config + DaemonSet ports + a node-local Service + service env), `deploy/clickstack/README.md`
- Test: `crates/core/src/metrics.rs` tests, `crates/core/src/log.rs` tests, `crates/core/src/settings.rs` tests, `crates/api/src/forward.rs` tests

**Interfaces:**
- Consumes: Task 1's `layer`, `server_span`, `finish`, `inject`, `inject_reqwest`, `bind_ratio`, `testing::subscriber`.
- Produces: `CentralSettings.trace_sample_ratio: f64`; `StoredCentralSettings.trace_sample_ratio: Option<f64>` (wire `traceSampleRatio`); the collector Service `kloudlite-otel-agent-otlp` (port 4318 HTTP, 4317 gRPC, `internalTrafficPolicy: Local`) in `kube-system` (k3s) and `kloudlite` (AKS).

- [ ] **Step 1: Write the failing tests**

`crates/core/src/log.rs`, add inside `mod tests`:

```rust
    #[test]
    fn json_lines_carry_the_enclosing_span_trace_id() {
        let buf = Buf::default();
        tracing::subscriber::with_default(super::subscriber(true, buf.clone()), || {
            let s = tracing::info_span!("http", trace_id = "4bf92f3577b34da6a3ce929d0e0e4736");
            let _e = s.enter();
            tracing::warn!("inside");
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let v: serde_json::Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
        assert_eq!(v["span"]["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736");
    }
```

`crates/core/src/metrics.rs`, add a test module (or extend the existing one) — add `kloudlite-trace = { workspace = true, features = ["testing"] }` under `[dev-dependencies]` of `crates/core/Cargo.toml`:

```rust
#[cfg(test)]
mod trace_tests {
    use axum::http::Request;
    use tower::ServiceExt as _;

    const IN: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[tokio::test(flavor = "current_thread")]
    async fn http_metrics_continues_the_callers_trace_and_passes_it_on() {
        let (dispatch, spans) = kloudlite_trace::testing::subscriber();
        let _g = tracing::dispatcher::set_default(&dispatch);
        let app = axum::Router::new()
            .route("/v1/x", axum::routing::get(|| async {
                let mut h = axum::http::HeaderMap::new();
                kloudlite_trace::inject(&mut h);
                h.get("traceparent").unwrap().to_str().unwrap().to_string()
            }))
            .layer(axum::middleware::from_fn_with_state("api", super::http_metrics));
        let res = app.oneshot(Request::get("/v1/x").header("traceparent", IN).body(axum::body::Body::empty()).unwrap()).await.unwrap();
        let body = axum::body::to_bytes(res.into_body(), 1 << 16).await.unwrap();
        let out = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(&out[3..35], &IN[3..35], "same trace id out as in");
        assert_ne!(&out[36..52], &IN[36..52], "a new span id: ours, not the caller's");
        assert!(out.ends_with("-01"), "sampled stays sampled");
        let got = spans.get_finished_spans().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "GET api");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn healthz_is_never_a_span() {
        let (dispatch, spans) = kloudlite_trace::testing::subscriber();
        let _g = tracing::dispatcher::set_default(&dispatch);
        let app = axum::Router::new()
            .route("/healthz", axum::routing::get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state("api", super::http_metrics));
        app.oneshot(Request::get("/healthz").header("traceparent", IN).body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert!(spans.get_finished_spans().unwrap().is_empty());
    }
}
```

The `testing::subscriber()` layer has no `UNTRACED` filter; add `.with_filter(tracing_subscriber::filter::filter_fn(|m| m.target() != crate::UNTRACED))` to it in `crates/trace/src/testing.rs` (and `use tracing_subscriber::Layer as _;`) so this test exercises the same filter production uses. The expected span name is `"{method} {route}"` where route is `route_class(path)`; read `route_class("/v1/x")`'s result in `metrics.rs` and put that in place of `api` in the assertion if it differs.

`crates/core/src/settings.rs` tests:

```rust
    #[test]
    fn trace_sample_ratio_defaults_merges_and_is_range_checked() {
        assert_eq!(CentralSettings::built_in_defaults().trace_sample_ratio, 0.1);
        let stored = StoredCentralSettings { trace_sample_ratio: Some(0.25), ..Default::default() };
        assert_eq!(CentralSettings::built_in_defaults().merged_with(&stored).trace_sample_ratio, 0.25);
        assert!(validate_stored(&StoredCentralSettings { trace_sample_ratio: Some(1.5), ..Default::default() }).unwrap_err().starts_with("trace_sample_ratio must be between 0 and 1"));
        assert!(CENTRAL_SETTING_META.iter().any(|(f, _)| *f == "traceSampleRatio"));
    }
```

`crates/api/src/forward.rs` tests (add `kloudlite-trace` with `testing` to `crates/api` dev-dependencies):

```rust
    #[tokio::test(flavor = "current_thread")]
    async fn a_peer_call_carries_the_callers_trace() {
        let (dispatch, _spans) = kloudlite_trace::testing::subscriber();
        let _g = tracing::dispatcher::set_default(&dispatch);
        let seen = Arc::new(std::sync::Mutex::new(String::new()));
        let s = seen.clone();
        let app = axum::Router::new().route("/p", axum::routing::get(move |h: axum::http::HeaderMap| {
            let s = s.clone();
            async move { *s.lock().unwrap() = h.get("traceparent").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default(); }
        }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let span = kloudlite_trace::server_span(&axum::http::Method::GET, "/v1/x", &{
            let mut h = axum::http::HeaderMap::new();
            h.insert("traceparent", "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".parse().unwrap());
            h
        }, "r", "v1");
        tracing::Instrument::instrument(send_retrying(reqwest::Client::new().get(format!("http://{addr}/p"))), span).await.unwrap();
        assert_eq!(&seen.lock().unwrap()[3..35], "4bf92f3577b34da6a3ce929d0e0e4736");
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p kloudlite-core -p kloudlite-api trace`
Expected: FAIL — `trace_sample_ratio` missing; `http_metrics` test fails (no `traceparent` injected, trace id mismatch); forward test gets an empty header.

- [ ] **Step 3: Wire the layer into the one subscriber**

`crates/core/Cargo.toml` `[dependencies]`: `kloudlite-trace = { workspace = true }`.

`crates/core/src/log.rs` — replace the body of `subscriber` and extend the module doc with one paragraph: "With `KLOUDLITE_OTLP_URL` set, `kloudlite_trace::layer()` joins the stack (Task 1's crate); every span we open records `trace_id`, so a JSON line carries `span.trace_id` and the collector's `trace_parser` links it to its trace."

```rust
pub fn subscriber<W>(json: bool, w: W) -> Box<dyn Subscriber + Send + Sync>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    use tracing_subscriber::layer::SubscriberExt as _;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let base = tracing_subscriber::registry().with(filter).with(kloudlite_trace::layer());
    if json {
        // `flatten_event`: `message` and the call-site fields land at the top level next to
        // `level`/`target`, which is what a pipeline query like `fields.repo == x` wants.
        Box::new(base.with(fmt::layer().json().flatten_event(true).with_writer(w)))
    } else {
        Box::new(base.with(fmt::layer().with_writer(w)))
    }
}
```

- [ ] **Step 4: Open the server span in `http_metrics`**

In `crates/core/src/metrics.rs`, replace `let span = tracing::info_span!("http", req_id = %req_id);` with:

```rust
    // The server span of the distributed trace (kloudlite_trace::server_span): the caller's
    // `traceparent` is its parent, `req_id` stays on it exactly as before.
    let span = kloudlite_trace::server_span(&method, &path, req.headers(), &req_id, class);
```

and change `let mut res = tracing::Instrument::instrument(next.run(req), span).await;` to `...instrument(next.run(req), span.clone()).await;`, then directly after `let status = res.status().as_u16();` add `kloudlite_trace::finish(&span, status);`.

- [ ] **Step 5: Add `trace_sample_ratio` to the central settings**

In `crates/core/src/settings.rs`:

```rust
    // CentralSettings, after builder_start_secs:
    /// Root sampling ratio for distributed traces. 0.0..=1.0. Errors, slow requests and probe
    /// traffic are kept regardless (`kloudlite_trace`); this only thins the rest.
    pub trace_sample_ratio: f64,
```

`built_in_defaults`: `trace_sample_ratio: kloudlite_trace::DEFAULT_RATIO,`. `merged_with`: `over!(trace_sample_ratio);`. `StoredCentralSettings` and `StoredCentralSettingsSnapshot`: `#[serde(skip_serializing_if = "Option::is_none")] pub trace_sample_ratio: Option<f64>,`. `push_history`: `trace_sample_ratio: old.trace_sample_ratio,`. The `From<&StoredCentralSettingsSnapshot> for StoredCentralSettings` impl: `trace_sample_ratio: s.trace_sample_ratio,` (use that impl's own binding name). `CENTRAL_SETTING_META`: `("traceSampleRatio", Mark::Live),`. `validate_stored`: `range!(trace_sample_ratio, 0.0f64, 1.0f64);` (the macro's `contains` works on `RangeInclusive<f64>`; `range_err` prints `0` and `1`). `CentralSettings` derives `JsonSchema` and `Serialize`; `f64` is fine for both. If any `#[derive(Eq)]` on these structs fails to compile with `f64`, drop `Eq` there (keep `PartialEq`).

Then `grep -rn 'builder_start_secs' --include='*.rs' --include='*.ts' --include='*.tsx' crates bins web/apps/web` and add the new field at every hit that enumerates fields (the admin schema route, fixtures, the web settings form), mirroring `builder_start_secs`.

- [ ] **Step 6: Bind the ratio in every central binary**

Directly after each `LiveSettings::new(` of `CentralSettings` (`crates/api/src/lib.rs:139` binding `central`; `bins/worker/src/main.rs:78` binding `central`; `bins/server/src/main.rs` where `app.central` exists; `bins/gateway/src/main.rs` where `gw.central` exists):

```rust
    kloudlite_trace::bind_ratio({
        let central = central.clone();
        move || central.load().trace_sample_ratio
    });
```

(For server use `let central = app.central.clone();`, for gateway `let central = gw.central.clone();`.) Add `kloudlite-trace = { workspace = true }` to each of those crates' `Cargo.toml`.

- [ ] **Step 7: Propagate api → srv**

`crates/api/src/forward.rs` `send_retrying` — inject once, before the first send, so the retry clone carries it too:

```rust
pub(crate) async fn send_retrying(req: reqwest::RequestBuilder) -> reqwest::Result<reqwest::Response> {
    let req = kloudlite_trace::inject_reqwest(req);
```

(keep the rest of the body unchanged). `crates/api/src/images.rs` `request_id`: before `out`, add `kloudlite_trace::inject(&mut out);` and rename nothing. For each direct `.send()` in `crates/api/src/repos.rs` and `crates/api/src/signatures.rs` whose URL targets the peer listener (they carry `PEER_HEADER`), wrap the builder: `kloudlite_trace::inject_reqwest(api.client.get(url))…` at the point the builder is created.

- [ ] **Step 8: Make the probe's requests sampled roots**

In `bins/slo/src/stages/mod.rs`, next to `request_id`:

```rust
/// A sampled W3C `traceparent` per request: the sampled flag is how a probe request marks
/// itself, and parent-based sampling keeps it at every hop (ingress, web, api, srv, agent).
pub(crate) fn traceparent() -> String {
    use rand::Rng as _;
    let mut r = rand::rng();
    format!("00-{:032x}-{:016x}-01", r.random::<u128>() | 1, r.random::<u64>() | 1)
}
```

Check the lock's `rand` major (`grep -A1 'name = "rand"' Cargo.lock`): on 0.8 the calls are `rand::thread_rng()` and `r.gen::<u128>()`. Wherever the probe sets `REQUEST_ID` on an outgoing request, also set `.header("traceparent", traceparent())`, and add `trace_id = %&tp[3..35]` to the `slo.http.done`/`slo.http.slow` lines so a probe sample links to its trace. Test in the same file:

```rust
    #[test]
    fn traceparent_is_w3c_and_sampled() {
        let t = super::traceparent();
        assert_eq!(t.len(), 55);
        assert!(t.starts_with("00-") && t.ends_with("-01"));
        assert_ne!(&t[3..35], "00000000000000000000000000000000");
    }
```

- [ ] **Step 9: Run the tests**

Run: `cargo test -p kloudlite-core -p kloudlite-api -p kloudlite-slo && cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 10: Collector traces pipeline, log linking, service env**

In `deploy/k3s/otel-agent.yaml`'s `kloudlite-otel-agent` ConfigMap, under `receivers:` add:

```yaml
      # Traces from our own services and ingress-nginx on THIS node, through the node-local
      # Service below. nginx speaks gRPC only; the Rust and Node SDKs speak HTTP.
      otlp:
        protocols:
          grpc: { endpoint: "0.0.0.0:4317" }
          http: { endpoint: "0.0.0.0:4318" }
```

In `filelog`'s `operators`, directly after the `json_parser` with `id: event`:

```yaml
          # A line inside a traced span carries `span.trace_id` (kloudlite_core::log); lifting it
          # into the record's TraceId is what makes HyperDX's log→trace link work.
          - type: trace_parser
            if: 'attributes.span != nil and attributes.span.trace_id != nil'
            trace_id:
              parse_from: attributes.span.trace_id
```

Under `service.pipelines` add:

```yaml
        traces:
          receivers: [otlp]
          processors: [k8sattributes, resource, batch]
          exporters: [otlphttp]
```

`transform/identity` is deliberately not on this pipeline: it overwrites `service.name` from the pod name, and the SDK's `OTEL_SERVICE_NAME` is the name the service map should show. In the DaemonSet's container add `ports: [{ containerPort: 4317, name: otlp-grpc }, { containerPort: 4318, name: otlp-http }]`, and append:

```yaml
---
# Node-local OTLP door: `internalTrafficPolicy: Local` sends a pod's spans to the collector on its
# own node, so export never crosses nodes and a node's collector outage costs only that node.
apiVersion: v1
kind: Service
metadata:
  name: kloudlite-otel-agent-otlp
  namespace: kube-system
spec:
  selector: { app: kloudlite-otel-agent }
  internalTrafficPolicy: Local
  ports:
    - { name: otlp-grpc, port: 4317, targetPort: 4317 }
    - { name: otlp-http, port: 4318, targetPort: 4318 }
```

Apply the same three edits to the AKS copy in `deploy/kloudlite.yaml` (collector ConfigMap near line 1277, DaemonSet near 1577), namespace `kloudlite`. Keep the "copied with three changes and no others" note in `CLAUDE.md` true by making identical edits.

For every container of `kloudlite-srv`, `kloudlite-api` (both roles), `kloudlite-worker`, `kloudlite-gateway`, `kloudlite-builder-gate` and the SLO CronJobs in `deploy/kloudlite.yaml`, add:

```yaml
            - name: KLOUDLITE_OTLP_URL
              value: http://kloudlite-otel-agent-otlp.kloudlite.svc:4318
            - name: OTEL_SERVICE_NAME
              value: kloudlite-srv   # kloudlite-api, kloudlite-admin, kloudlite-worker, kloudlite-gateway, kloudlite-builder-gate, kloudlite-slo
```

- [ ] **Step 11: Bound traces retention**

In `deploy/clickstack/README.md`, add a section after "The one manual step: the ingestion API key":

````markdown
## Traces retention: 7 days

The exporter's `ttl` (`clickstack-values.yaml`, `720h`) is one value for every table it creates,
applied only at `CREATE TABLE IF NOT EXISTS` — it cannot give traces their own window, and it never
rewrites a table that exists. Traces are the largest per-request signal and are only read for
recent incidents, so they get a week, set once on the table itself:

```sh
kubectl -n clickstack exec -it chi-clickstack-clickhouse-0-0-0 -- clickhouse-client -q \
  "ALTER TABLE default.otel_traces MODIFY TTL toDate(Timestamp) + INTERVAL 7 DAY"
kubectl -n clickstack exec -it chi-clickstack-clickhouse-0-0-0 -- clickhouse-client -q \
  "SHOW CREATE TABLE default.otel_traces" | grep TTL
```

A chart upgrade does not undo it. A table dropped and recreated by the exporter comes back at 30
days; re-run this. The pod name is the operator's; `kubectl -n clickstack get pods` if it differs.
````

Before applying, confirm in HyperDX (ClickStack MCP `clickstack_list_sources`) that a Traces source reads `default.otel_traces`.

- [ ] **Step 12: Exercise on the fleet**

Ship per `deploy/dev/README.md`, apply the collector yaml, run the TTL ALTER, then from the dev pod:

```sh
TP=00-$(openssl rand -hex 16)-$(openssl rand -hex 8)-01
curl -s -o /dev/null -H "traceparent: $TP" https://<app host>/v1/regions -H "authorization: Bearer $TOKEN"
echo ${TP:3:32}
```

Expected: `clickstack_trace_waterfall` for that trace id shows `kloudlite-api` `GET v1` (and `kloudlite-srv` under it for a repo read); `clickstack_search` on the Logs source for that TraceId returns the api's `http.*` line.

- [ ] **Step 13: Commit**

```bash
git add crates/core crates/api bins/server bins/worker bins/gateway bins/slo deploy/k3s/otel-agent.yaml deploy/kloudlite.yaml deploy/clickstack/README.md Cargo.lock web/apps/web
git commit -m "Trace the central tier end to end and link logs to traces"
```

---

### Task 3: Trace the agent — peer hops, reconcile passes, kube calls

**Files:**
- Modify: `crates/workspaces/src/crd/settings.rs` (`ClusterSettingsSpec`, `CLUSTER_SETTING_META`), `crates/workspaces/src/settings.rs` (`AgentSettings`, `from_env`, `merged_with`), `crates/workspaces/src/api/admin/settings.rs` (`validate_cluster_patch`)
- Modify: `bins/agent/src/lib.rs:320`, `bins/agent/src/peer/mod.rs:108-114`, `bins/agent/src/peer/pull.rs:487`, `bins/agent/src/peer/wake.rs:~36`, `bins/agent/src/controller/run.rs:~176-183`
- Modify: `crates/workspaces/src/k8s/client.rs` (`Bounded::call`, `attempt`)
- Modify: `crates/workspaces/Cargo.toml`, `bins/agent/Cargo.toml`, `deploy/k3s/agent-daemonset` env (the agent DaemonSet in `deploy/k3s/`; `grep -rln 'kloudlite-agent' deploy/k3s/*.yaml`)
- Test: `crates/workspaces/src/settings.rs` tests, `crates/workspaces/src/k8s/client.rs` tests, `bins/agent/src/peer/tests.rs`

**Interfaces:**
- Consumes: Task 1 (`traced`, `inject_reqwest`, `client_span`, `stamp`, `bind_ratio`, `testing::subscriber`).
- Produces: `ClusterSettingsSpec.trace_sample_ratio: Option<f64>` (wire `traceSampleRatio`), `AgentSettings.trace_sample_ratio: f64`.

- [ ] **Step 1: Write the failing tests**

`crates/workspaces/src/settings.rs` tests:

```rust
    #[test]
    fn trace_sample_ratio_merges_from_cluster_settings() {
        let spec = ClusterSettingsSpec { trace_sample_ratio: Some(0.5), ..Default::default() };
        assert_eq!(AgentSettings::from_env().merged_with(&spec).trace_sample_ratio, 0.5);
        assert_eq!(AgentSettings::from_env().trace_sample_ratio, kloudlite_trace::DEFAULT_RATIO);
    }
```

(If `ClusterSettingsSpec` has no `Default`, build it the way the existing test at `crd/settings.rs:180` does.)

`crates/workspaces/src/k8s/client.rs` tests (add `kloudlite-trace` with `testing` to `crates/workspaces` dev-dependencies):

```rust
    #[tokio::test(flavor = "current_thread")]
    async fn a_timed_out_kube_call_is_an_error_client_span_naming_the_layer() {
        let (dispatch, spans) = kloudlite_trace::testing::subscriber();
        let _g = tracing::dispatcher::set_default(&dispatch);
        let never = tower::service_fn(|_req: http::Request<kube::client::Body>| std::future::pending::<Result<http::Response<kube::client::Body>, tower::BoxError>>());
        let mut svc = tower::Layer::layer(&BoundLayer::OUTER, never);
        tokio::time::pause();
        let req = http::Request::post("/api/v1/namespaces/x/pods").body(kube::client::Body::empty()).unwrap();
        let err = tower::ServiceExt::oneshot(&mut svc, req).await.unwrap_err();
        assert!(err.downcast_ref::<KubeTimeout>().is_some());
        let got = spans.get_finished_spans().unwrap();
        let s = got.iter().find(|s| s.name == "kube POST").expect("one client span");
        assert!(matches!(s.status, opentelemetry::trace::Status::Error { .. }));
        assert!(s.attributes.iter().any(|kv| kv.key.as_str() == "kube.timeout_layer" && kv.value.as_str() == "outer"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_watch_opens_no_span() {
        let (dispatch, spans) = kloudlite_trace::testing::subscriber();
        let _g = tracing::dispatcher::set_default(&dispatch);
        let ok = tower::service_fn(|_req: http::Request<kube::client::Body>| async { Ok::<_, tower::BoxError>(http::Response::new(kube::client::Body::empty())) });
        let svc = tower::Layer::layer(&BoundLayer::OUTER, ok);
        let req = http::Request::get("/api/v1/pods?watch=true").body(kube::client::Body::empty()).unwrap();
        tower::ServiceExt::oneshot(svc, req).await.unwrap();
        assert!(spans.get_finished_spans().unwrap().is_empty());
    }
```

Match the service error type to what `Bounded<S>`'s `Service` impl requires (read its `where` clause at `client.rs:~150-165`), and use the existing test module's request-building helper if it has one. The test relies on the timeout span being a root that ends with ERROR, which `Promote` keeps even though the ratio drops it.

`bins/agent/src/peer/tests.rs`:

```rust
#[tokio::test(flavor = "current_thread")]
async fn a_peer_request_continues_the_senders_trace() {
    let (dispatch, spans) = kloudlite_trace::testing::subscriber();
    let _g = tracing::dispatcher::set_default(&dispatch);
    let app = axum::Router::new()
        .route("/peer/v1/wake", axum::routing::post(|| async { axum::http::StatusCode::NO_CONTENT }))
        .layer(axum::middleware::from_fn(kloudlite_trace::traced));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let parent = kloudlite_trace::client_span("sender", "POST", "/peer/v1/wake");
    let trace = { use opentelemetry::trace::TraceContextExt as _; use tracing_opentelemetry::OpenTelemetrySpanExt as _; parent.context().span().span_context().trace_id() };
    tracing::Instrument::instrument(kloudlite_trace::inject_reqwest(reqwest::Client::new().post(format!("http://{addr}/peer/v1/wake"))).send(), parent).await.unwrap();
    let got = spans.get_finished_spans().unwrap();
    assert!(got.iter().any(|s| s.name == "POST /peer/v1/wake" && s.span_context.trace_id() == trace && s.parent_span_is_remote));
}
```

A ratio-0.1 root may be unsampled; wrap the test body in `kloudlite_trace::bind_ratio(|| 1.0);` at the top (the test binary is its own process for `bins/agent/tests` but `peer/tests.rs` is a unit module sharing a binary — `bind_ratio` is first-wins, and every agent test that cares wants 1.0, so bind it there). Add `opentelemetry`, `tracing-opentelemetry` (workspace) and `kloudlite-trace` with `testing` to `bins/agent` dev-dependencies.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p kloudlite-workspaces trace -- --nocapture; cargo test -p kloudlite-agent a_peer_request_continues`
Expected: FAIL — missing field, no span named `kube POST`, no server span on the peer router.

- [ ] **Step 3: Add the cluster setting**

`crates/workspaces/src/crd/settings.rs` `ClusterSettingsSpec`:

```rust
    /// Root sampling ratio for this region's agent traces. 0.0..=1.0. Errors, slow passes and
    /// sampled parents are kept regardless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_sample_ratio: Option<f64>,
```

`CLUSTER_SETTING_META`: `("traceSampleRatio", kloudlite_core::settings::Mark::Live, &[]),`. `AgentSettings`: `pub trace_sample_ratio: f64,`; `from_env`: `trace_sample_ratio: kloudlite_trace::DEFAULT_RATIO,`; `merged_with`: `over!(trace_sample_ratio);`. `validate_cluster_patch`: `range!(trace_sample_ratio, 0.0f64, 1.0f64);`. If `AgentSettings`' `#[derive(PartialEq)]` stays valid with `f64` (it does; `Eq` would not), nothing else changes. Grep `quota_gb_ceiling` across `crates/workspaces`, `bins/agent`, `web/apps/web` and add the field wherever fields are enumerated (fixtures, the region settings tab).

- [ ] **Step 4: Bind the agent's ratio**

`bins/agent/src/lib.rs`, right after `let settings = LiveSettings::new(initial_settings(&client).await);`:

```rust
    kloudlite_trace::bind_ratio({
        let settings = settings.clone();
        move || settings.load().trace_sample_ratio
    });
```

- [ ] **Step 5: Peer server and clients**

`bins/agent/src/peer/mod.rs` `router`: add `.layer(axum::middleware::from_fn(kloudlite_trace::traced))` before `.with_state(...)`. The snapshot handler's span ends when the streamed response is returned — the byte stream itself gets no span, by design.

`bins/agent/src/peer/pull.rs:487`: `let resp = kloudlite_trace::inject_reqwest(http.get(&url)).header("x-peer-secret", secret)…` (rest unchanged). `bins/agent/src/peer/wake.rs`: `match kloudlite_trace::inject_reqwest(http.post(&url)).header("x-peer-secret", *secret)…`.

Both callers run inside a reconcile pass or a beat; the pull runs from the replication beat, so wrap the beat's per-volume pull call in `tracing::info_span!("replicate.pull", otel.kind = "internal", volume = %id, trace_id = tracing::field::Empty)` with a `kloudlite_trace::stamp(&span)` after creation, instrumenting the pull future — find the call with `grep -n 'pull::' bins/agent/src/*.rs bins/agent/src/peer/*.rs`.

- [ ] **Step 6: One span per reconcile pass**

In `bins/agent/src/controller/run.rs`, replace `let r = fut.await;` with:

```rust
    // One span per pass, never per watch event: the pass is the unit that decides and writes.
    let span = tracing::info_span!("reconcile", otel.name = %format!("reconcile {kind}"), otel.status_code = tracing::field::Empty, kind, %name, trace_id = tracing::field::Empty);
    kloudlite_trace::stamp(&span);
    let r = tracing::Instrument::instrument(fut, span.clone()).await;
    if r.is_err() {
        span.record("otel.status_code", "ERROR");
    }
```

- [ ] **Step 7: kube client spans**

In `crates/workspaces/src/k8s/client.rs` `Bounded::call`, in the `layer == "outer"` branch only, and only when `kind` is not a watch (`classify` returns the watch kind for those — use the exact variant name from `enum Kind`):

```rust
        // One CLIENT span per non-watch call, opened in the outer layer so both bounds, the
        // retry and hyper sit inside it. A timeout marks it ERROR and names the layer that fired.
        let span = kloudlite_trace::client_span("kube", method.as_str(), &path);
        span.record("otel.name", format!("kube {method}"));
```

and instrument the boxed future it returns with `tracing::Instrument::instrument(…, span)`. In `attempt`, in the `Err(_)` timeout arm, before returning:

```rust
            let cur = tracing::Span::current();
            cur.record("kube.timeout_layer", layer);
            cur.record("otel.status_code", "ERROR");
```

The inner layer's `attempt` runs inside the outer span's future, so `Span::current()` is the kube span in both cases; if inner fires first the attribute says `inner`, and an outer retry that then succeeds leaves `inner` recorded with ERROR — which is the truth about that call. Watches never reach this branch. Leave the existing `kube.timeout`/`kube.retry` log lines unchanged; they now carry `span.trace_id`.

- [ ] **Step 8: Run the tests**

Run: `cargo test -p kloudlite-workspaces && cargo test -p kloudlite-agent && cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 9: Agent env and fleet exercise**

In the agent DaemonSet (k3s) add `KLOUDLITE_OTLP_URL=http://kloudlite-otel-agent-otlp.kube-system.svc:4318` and `OTEL_SERVICE_NAME=kloudlite-agent`. The agent runs `hostNetwork`? Check `grep -n hostNetwork` in its yaml: a hostNetwork pod still resolves cluster DNS only with `dnsPolicy: ClusterFirstWithHostNet`; if the DaemonSet lacks it, use `http://127.0.0.1:4318` instead and give the collector DaemonSet `hostPort: 4318` on that port.

Exercise: set `traceSampleRatio: 1.0` on the test region's `ClusterSettings`, stop a workspace (a stop cuts a sync point and wakes peers), then `clickstack_search` Traces for `ServiceName = 'kloudlite-agent' AND SpanName = 'POST /peer/v1/wake'` in the last 5 min; open its waterfall and confirm the parent `reconcile Workspace` span is on the other node. Restore the ratio.

- [ ] **Step 10: Commit**

```bash
git add crates/workspaces bins/agent deploy/k3s Cargo.lock web/apps/web
git commit -m "Trace agent peer hops, reconcile passes and kube calls"
```

---

### Task 4: Trace builder-gate → api

**Files:**
- Modify: `bins/builder-gate/Cargo.toml`, `bins/builder-gate/src/lib.rs:56-64` (`call`), `:104` (router), its `LiveSettings::new` site (`:293` and the `main.rs` equivalent)
- Test: `bins/builder-gate/tests/gate.rs`

**Interfaces:**
- Consumes: Task 1 (`inject_reqwest`, `traced`, `bind_ratio`, `testing::subscriber`); Task 2 (`CentralSettings.trace_sample_ratio`, api `http_metrics` server span).
- Produces: nothing new.

- [ ] **Step 1: Write the failing test**

In `bins/builder-gate/tests/gate.rs`, beside the existing fake api (the test already stands one up for `/v1/internal/builders/*`), add:

```rust
#[tokio::test(flavor = "current_thread")]
async fn a_gate_call_to_the_api_carries_the_trace() {
    let (dispatch, _spans) = kloudlite_trace::testing::subscriber();
    let _g = tracing::dispatcher::set_default(&dispatch);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let s = seen.clone();
    let api = axum::Router::new().route("/v1/internal/builders/{slug}/start", axum::routing::post(move |h: axum::http::HeaderMap| {
        let s = s.clone();
        async move { *s.lock().unwrap() = h.get("traceparent").map(|v| v.to_str().unwrap().into()).unwrap_or_default(); axum::Json(serde_json::json!({"ready": true})) }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, api).await.unwrap() });
    let parent = kloudlite_trace::client_span("gate", "POST", "/start");
    let want = { use opentelemetry::trace::TraceContextExt as _; use tracing_opentelemetry::OpenTelemetrySpanExt as _; parent.context().span().span_context().trace_id().to_string() };
    tracing::Instrument::instrument(start_via(&base, "alice"), parent).await;
    assert_eq!(&seen.lock().unwrap()[3..35], want);
}
```

`start_via(base, slug)` stands for however the existing tests drive the gate's api client (read `gate.rs` for its constructor and the `start` method name, e.g. `ApiClient::new(base, "s".into()).start("alice").await`), and inline that call here.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p kloudlite-builder-gate a_gate_call_to_the_api_carries_the_trace`
Expected: FAIL — empty `traceparent`.

- [ ] **Step 3: Implement**

`call`:

```rust
    async fn call(&self, method: reqwest::Method, path: &str) -> Result<reqwest::Response, String> {
        kloudlite_trace::inject_reqwest(self.http.request(method, format!("{}{path}", self.base)))
            .bearer_auth(&self.secret)
```

(rest unchanged). The gate's HTTP router at `:104`: `.layer(axum::middleware::from_fn(kloudlite_trace::traced))`. The gate's spliced TCP connections get one span per accepted connection: where the gate accepts a buildkit connection, wrap the splice future in `tracing::info_span!("gate.connection", otel.kind = "server", slug = %slug, trace_id = tracing::field::Empty)` + `kloudlite_trace::stamp`, so the api `start` call made while the connection waits is its child. Bind the ratio next to its `central` LiveSettings exactly as in Task 2 Step 6. Add `kloudlite-trace` (+ `testing` in dev-dependencies, plus `opentelemetry`/`tracing-opentelemetry` dev-deps) to `bins/builder-gate/Cargo.toml`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p kloudlite-builder-gate && cargo clippy -p kloudlite-builder-gate --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add bins/builder-gate Cargo.lock
git commit -m "Trace builder gate calls into the api"
```

---

### Task 5: Trace the web tier (Next.js)

**Files:**
- Create: `web/apps/web/src/lib/tracing.ts`, `web/apps/web/test-node/tracing.test.ts`
- Modify: `web/apps/web/package.json`, `web/apps/web/src/instrumentation-node.ts`, `deploy/kloudlite-web.yaml` (env)

**Interfaces:**
- Consumes: the Rust tiers' server spans (Task 2) as downstream children.
- Produces: `startTracing(service: string, url: string): void`; `KlSampler`; `Promote` (JS), all exported from `src/lib/tracing.ts`.

- [ ] **Step 1: Add the packages**

Run: `cd web && bun add --cwd apps/web @opentelemetry/api@1.9.1 @opentelemetry/sdk-trace-node@2.11.0 @opentelemetry/sdk-trace-base@2.11.0 @opentelemetry/resources@2.11.0 @opentelemetry/exporter-trace-otlp-proto@0.222.0 @opentelemetry/instrumentation@0.222.0 @opentelemetry/instrumentation-http@0.222.0 @opentelemetry/instrumentation-undici@0.32.0`
Then: `grep -n 'parentSpanContext' node_modules/@opentelemetry/sdk-trace/build/src/export/ReadableSpan.d.ts; grep -n 'ignoreIncomingRequestHook' node_modules/@opentelemetry/instrumentation-http/build/src/types.d.ts` (from `web/`; if hoisted under `apps/web/node_modules`, look there).
Expected: both print a line.

- [ ] **Step 2: Write the failing test**

`web/apps/web/test-node/tracing.test.ts` — runs under Node (`node --test`), not bun: `instrumentation-http` and `-undici` patch Node's modules, which bun does not load.

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { context, ROOT_CONTEXT, SamplingDecision, SpanKind, SpanStatusCode, trace, TraceFlags } from "@opentelemetry/api";
import { InMemorySpanExporter, SimpleSpanProcessor } from "@opentelemetry/sdk-trace-base";
import { KlSampler, Promote, startTracing } from "../src/lib/tracing.ts";

const remote = (flags: number) => trace.setSpanContext(ROOT_CONTEXT, { traceId: "4bf92f3577b34da6a3ce929d0e0e4736", spanId: "00f067aa0ba902b7", traceFlags: flags, isRemote: true });

test("sampler: parent decides, roots follow the ratio, nothing is dropped", () => {
  const s = new KlSampler(0);
  assert.equal(s.shouldSample(remote(TraceFlags.SAMPLED), "x", "n", SpanKind.SERVER, {}, []).decision, SamplingDecision.RECORD_AND_SAMPLED);
  assert.equal(s.shouldSample(remote(TraceFlags.NONE), "x", "n", SpanKind.SERVER, {}, []).decision, SamplingDecision.RECORD);
  assert.equal(s.shouldSample(ROOT_CONTEXT, "ffffffffffffffffffffffffffffffff", "n", SpanKind.SERVER, {}, []).decision, SamplingDecision.RECORD);
  assert.equal(new KlSampler(1).shouldSample(ROOT_CONTEXT, "00000000000000000000000000000001", "n", SpanKind.SERVER, {}, []).decision, SamplingDecision.RECORD_AND_SAMPLED);
});

test("promote: an errored local root keeps its unsampled trace, a fast ok one does not", () => {
  const out = new InMemorySpanExporter();
  const p = new Promote(new SimpleSpanProcessor(out));
  const mk = (id: string, parent: string | undefined, code: SpanStatusCode, ms: number) =>
    ({ spanContext: () => ({ traceId: "4bf92f3577b34da6a3ce929d0e0e4736", spanId: id, traceFlags: TraceFlags.NONE }), parentSpanContext: parent ? { traceId: "4bf92f3577b34da6a3ce929d0e0e4736", spanId: parent, traceFlags: 0, isRemote: false } : undefined, status: { code }, duration: [0, ms * 1e6] }) as never;
  p.onEnd(mk("0000000000000002", "0000000000000001", SpanStatusCode.UNSET, 1));
  p.onEnd(mk("0000000000000001", undefined, SpanStatusCode.UNSET, 1));
  assert.equal(out.getFinishedSpans().length, 0);
  p.onEnd(mk("0000000000000003", "0000000000000004", SpanStatusCode.UNSET, 1));
  p.onEnd(mk("0000000000000004", undefined, SpanStatusCode.ERROR, 1));
  assert.equal(out.getFinishedSpans().length, 2);
});

test("round trip: an incoming traceparent reaches the downstream fetch with the same trace id", async () => {
  startTracing("web-test", "http://127.0.0.1:9");
  let seen = "";
  const down = http.createServer((req, res) => { seen = String(req.headers.traceparent ?? ""); res.end("ok"); }).listen(0);
  const downPort = (down.address() as { port: number }).port;
  const up = http.createServer(async (_req, res) => { await fetch(`http://127.0.0.1:${downPort}/`); res.end("ok"); }).listen(0);
  const upPort = (up.address() as { port: number }).port;
  await fetch(`http://127.0.0.1:${upPort}/`, { headers: { traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01" } });
  up.close(); down.close();
  assert.equal(seen.slice(3, 35), "4bf92f3577b34da6a3ce929d0e0e4736");
});

test("a dead collector never fails a request", async () => {
  const srv = http.createServer((_q, r) => r.end("ok")).listen(0);
  const port = (srv.address() as { port: number }).port;
  for (let i = 0; i < 500; i++) assert.equal((await fetch(`http://127.0.0.1:${port}/`)).status, 200);
  srv.close();
});
```

The round-trip test's outer `fetch` is itself instrumented (it runs after `startTracing`), which is fine: it becomes the parent, but the incoming header it would inject is replaced by ours only if no span is active — so instead issue the outer request with `http.request` from `node:http` inside `context.with(ROOT_CONTEXT, …)` if the assertion shows a different trace id; the test must prove the header IN is continued OUT. Add to `web/apps/web/package.json` scripts: `"test:node": "node --test test-node/*.test.ts"`.

- [ ] **Step 3: Run it to verify it fails**

Run: `cd web/apps/web && bun run test:node`
Expected: FAIL — `../src/lib/tracing.ts` not found.

- [ ] **Step 4: Write `src/lib/tracing.ts`**

```ts
/** Distributed tracing for the Node tiers: the JS twin of `crates/trace`.
 *
 *  Parent-based sampling that never drops (an unsampled span is RECORDED so `Promote` can keep a
 *  trace whose local root errored or ran over a second), a bounded batch exporter that drops on a
 *  full queue or a dead collector, and only two instrumentations — incoming/outgoing `node:http`
 *  and `fetch` (undici). Next.js's own spans ride the same global provider.
 *
 *  No relative or `@/` imports: `harness/bench/src/tracing.ts` is a copy of this file, and this
 *  one must also load under plain `node --test`.
 *  ponytail: two copies because web (bun workspace) and harness (npm) share no package root;
 *  move to one package when they do. */
import { SamplingDecision, SpanStatusCode, trace, TraceFlags, type Attributes, type Context, type Link, type Sampler, type SamplingResult, type SpanKind } from "@opentelemetry/api";
import { BatchSpanProcessor, TraceIdRatioBasedSampler, type ReadableSpan, type Span, type SpanProcessor } from "@opentelemetry/sdk-trace-base";
import { NodeTracerProvider } from "@opentelemetry/sdk-trace-node";
import { OTLPTraceExporter } from "@opentelemetry/exporter-trace-otlp-proto";
import { registerInstrumentations } from "@opentelemetry/instrumentation";
import { HttpInstrumentation } from "@opentelemetry/instrumentation-http";
import { UndiciInstrumentation } from "@opentelemetry/instrumentation-undici";
import { resourceFromAttributes } from "@opentelemetry/resources";

const SLOW_MS = 1_000;
const ROOT_RATIO = 0.1;
const MAX_TRACES = 4096;
const MAX_SPANS = 512;
const UNTRACED = new Set(["/healthz", "/readyz", "/livez", "/metrics", "/api/health"]);

export class KlSampler implements Sampler {
  private root: TraceIdRatioBasedSampler;
  constructor(ratio = ROOT_RATIO) {
    this.root = new TraceIdRatioBasedSampler(ratio);
  }
  shouldSample(cx: Context, traceId: string, name: string, kind: SpanKind, attrs: Attributes, links: Link[]): SamplingResult {
    const parent = trace.getSpanContext(cx);
    if (parent && trace.isSpanContextValid(parent)) {
      return { decision: parent.traceFlags & TraceFlags.SAMPLED ? SamplingDecision.RECORD_AND_SAMPLED : SamplingDecision.RECORD };
    }
    const d = this.root.shouldSample(cx, traceId).decision;
    return { decision: d === SamplingDecision.RECORD_AND_SAMPLED ? d : SamplingDecision.RECORD };
  }
  toString() {
    return "KlSampler";
  }
}

export class Promote implements SpanProcessor {
  private pending = new Map<string, ReadableSpan[]>();
  constructor(private inner: SpanProcessor) {}
  onStart(span: Span, cx: Context) {
    this.inner.onStart(span, cx);
  }
  onEnd(span: ReadableSpan) {
    const sc = span.spanContext();
    if (sc.traceFlags & TraceFlags.SAMPLED) return this.inner.onEnd(span);
    const localRoot = !span.parentSpanContext || span.parentSpanContext.isRemote;
    if (!localRoot) {
      if (this.pending.size >= MAX_TRACES && !this.pending.has(sc.traceId)) this.pending.clear();
      const waiting = this.pending.get(sc.traceId) ?? [];
      if (waiting.length < MAX_SPANS) waiting.push(span);
      this.pending.set(sc.traceId, waiting);
      return;
    }
    const children = this.pending.get(sc.traceId) ?? [];
    this.pending.delete(sc.traceId);
    const ms = span.duration[0] * 1e3 + span.duration[1] / 1e6;
    if (span.status.code !== SpanStatusCode.ERROR && ms <= SLOW_MS) return;
    for (const s of [...children, span]) {
      const c = s.spanContext();
      // Same span, flags re-marked: the batch processor exports only SAMPLED spans.
      this.inner.onEnd(Object.create(s, { spanContext: { value: () => ({ ...c, traceFlags: c.traceFlags | TraceFlags.SAMPLED }) } }));
    }
  }
  forceFlush() {
    return this.inner.forceFlush();
  }
  shutdown() {
    return this.inner.shutdown();
  }
}

let started = false;

export function startTracing(service: string, url: string) {
  if (started) return;
  started = true;
  const exporter = new OTLPTraceExporter({ url: `${url.replace(/\/$/, "")}/v1/traces`, timeoutMillis: 5_000 });
  const provider = new NodeTracerProvider({
    resource: resourceFromAttributes({ "service.name": service }),
    sampler: new KlSampler(),
    spanProcessors: [new Promote(new BatchSpanProcessor(exporter, { maxQueueSize: 2048, maxExportBatchSize: 512, scheduledDelayMillis: 5_000, exportTimeoutMillis: 5_000 }))],
  });
  // W3C trace-context propagator and AsyncLocalStorage context manager: the SDK's defaults.
  provider.register();
  registerInstrumentations({
    tracerProvider: provider,
    instrumentations: [
      new HttpInstrumentation({ ignoreIncomingRequestHook: (req) => UNTRACED.has(new URL(req.url ?? "/", "http://x").pathname) }),
      new UndiciInstrumentation(),
    ],
  });
}
```

Check `resource` is a `TracerConfig` field (`grep -n 'resource?' node_modules/@opentelemetry/sdk-trace/build/src/types.d.ts`) and `exportTimeoutMillis` a `BufferConfig` field; remove whichever is absent.

- [ ] **Step 5: Register it in the web process**

`web/apps/web/src/instrumentation-node.ts`, at the top of `registerNode()` before the metrics patch:

```ts
  // Tracing first, so its http instrumentation wraps the server before the metrics patch below.
  const otlp = process.env.KLOUDLITE_OTLP_URL;
  if (otlp) {
    const { startTracing } = await import("./lib/tracing");
    startTracing(process.env.OTEL_SERVICE_NAME ?? "kloudlite-web", otlp);
    logger.info("web.tracing.installed");
  }
```

Update the `ponytail:` note in that file's doc comment: the OpenTelemetry registration now exists beside the metrics patch; the patch stays because the metrics are not spans.

`deploy/kloudlite-web.yaml`, web container env: `KLOUDLITE_OTLP_URL=http://kloudlite-otel-agent-otlp.kloudlite.svc:4318`, `OTEL_SERVICE_NAME=kloudlite-web`.

Every `lib/api/client.ts` call to the api is a `fetch`, so undici carries `traceparent` to `kloudlite-api` without a code change there.

- [ ] **Step 6: Run the tests and the web gates**

Run: `cd web/apps/web && bun run test:node && cd ../.. && bun run lint && bun run typecheck && bun run build && bun run test`
Expected: PASS. If `next build` warns that it cannot bundle an `@opentelemetry/*` module, add those package names to `serverExternalPackages` in `web/apps/web/next.config.ts` and rebuild.

- [ ] **Step 7: Exercise and commit**

After deploy: `curl -s -o /dev/null -H "traceparent: $TP" https://<app host>/` and confirm in `clickstack_trace_waterfall` that `kloudlite-web` spans (Next's `GET /` and `render route (app) /`) parent `kloudlite-api` spans.

```bash
git add web/apps/web deploy/kloudlite-web.yaml
git commit -m "Trace the web tier with the OpenTelemetry Node SDK"
```

---

### Task 6: Trace bench → workspace tool server

**Files:**
- Create: `harness/bench/src/tracing.ts` (copy of Task 5's `web/apps/web/src/lib/tracing.ts`, same content), `harness/bench/test/tracing.test.ts`
- Modify: `harness/package.json`, `harness/bench/src/main.ts` (server path), `harness/bench/src/rpc-child.ts:51`, `harness/pi/workspace-tools.ts:100`
- Modify: `crates/ide/Cargo.toml`, `crates/ide/src/server.rs:39-51`, `bins/kl/Cargo.toml`, `bins/kl/src/main.rs:151-155`
- Modify: `crates/workspaces/src/k8s/policies.rs` (`default_policies`), `crates/workspaces/src/k8s/workspace.rs:~22`, `crates/workspaces/src/k8s/bench.rs:~57`
- Test: `crates/ide` server test, `crates/workspaces/src/k8s/tests/` policy test, `harness/bench/test/tracing.test.ts`

**Interfaces:**
- Consumes: Task 1 (`traced`, `layer`, `testing::subscriber`); Task 5's `tracing.ts` content.
- Produces: `traceparent(): string | undefined` exported from `harness/bench/src/tracing.ts`; `KL_TRACEPARENT` child env; `allow-otlp` NetworkPolicy.

- [ ] **Step 1: Write the failing tests**

`crates/ide` — in the crate's existing server tests (`grep -rn 'fn .*healthz' crates/ide/src crates/ide/tests`), add `kloudlite-trace` with `testing` to dev-dependencies and:

```rust
#[tokio::test(flavor = "current_thread")]
async fn a_tool_call_continues_the_bench_trace() {
    let (dispatch, spans) = kloudlite_trace::testing::subscriber();
    let _g = tracing::dispatcher::set_default(&dispatch);
    let app = test_router(); // the helper the existing tests use to build `server::router`
    let req = axum::http::Request::get("/tools").header("traceparent", "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").body(axum::body::Body::empty()).unwrap();
    tower::ServiceExt::oneshot(app, req).await.unwrap();
    let got = spans.get_finished_spans().unwrap();
    assert!(got.iter().any(|s| s.name == "GET /tools" && s.span_context.trace_id().to_string() == "4bf92f3577b34da6a3ce929d0e0e4736"));
}
```

Replace `test_router()` with the existing tests' construction of the router.

`crates/workspaces/src/k8s/tests/` — in the file that tests `default_policies` (`grep -rln default_policies crates/workspaces/src/k8s/tests`):

```rust
#[test]
fn tenants_may_reach_only_the_node_collectors_otlp_http_port() {
    let or = OwnerReference::default();
    let p = default_policies("ws-alice", "alice", &or).into_iter().find(|p| p.metadata.name.as_deref() == Some("allow-otlp")).expect("allow-otlp");
    let v = serde_json::to_value(p.spec.unwrap()).unwrap();
    assert_eq!(v["egress"][0]["ports"], serde_json::json!([{ "protocol": "TCP", "port": 4318 }]));
    assert_eq!(v["egress"][0]["to"][0]["podSelector"]["matchLabels"]["app"], "kloudlite-otel-agent");
}
```

`harness/bench/test/tracing.test.ts`:

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { context, trace } from "@opentelemetry/api";
import { startTracing, traceparent } from "../src/tracing.ts";

test("the child's traceparent is the active span's trace", async () => {
  startTracing("bench-test", "http://127.0.0.1:9");
  const tracer = trace.getTracer("t");
  await tracer.startActiveSpan("session", async (span) => {
    const tp = traceparent();
    assert.ok(tp);
    assert.equal(tp!.slice(3, 35), span.spanContext().traceId);
    span.end();
  });
});

test("a tool call's fetch carries KL_TRACEPARENT's trace id", async () => {
  let seen = "";
  const srv = http.createServer((req, res) => { seen = String(req.headers.traceparent ?? ""); res.end("{}"); }).listen(0);
  const port = (srv.address() as { port: number }).port;
  const tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
  await fetch(`http://127.0.0.1:${port}/tools/read`, { method: "POST", headers: { "content-type": "application/json", traceparent: tp } });
  srv.close();
  assert.equal(seen.slice(3, 35), "4bf92f3577b34da6a3ce929d0e0e4736");
  void context;
});
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p kloudlite-ide a_tool_call_continues && cargo test -p kloudlite-workspaces tenants_may_reach && (cd harness && npm run bench:test)`
Expected: FAIL — no span, no `allow-otlp`, `../src/tracing.ts` not found.

- [ ] **Step 3: Tool server span and exporter**

`crates/ide/Cargo.toml`: `kloudlite-trace = { path = "../trace" }`. `crates/ide/src/server.rs` router: after the last `.route(...)` add `.layer(axum::middleware::from_fn(kloudlite_trace::traced))` (it names spans by `MatchedPath`, so `/stream/process/{id}` is one name; the WebSocket stream itself is one span that ends at upgrade).

`bins/kl/src/main.rs` `serve_ide`, replace the subscriber block:

```rust
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .with(kloudlite_trace::layer())
        .with(tracing_subscriber::fmt::layer().json().with_writer(std::io::stderr))
        .init();
```

`bins/kl/Cargo.toml`: `kloudlite-trace = { path = "../../crates/trace" }`. Build the musl target once in the pod (`deploy/dev/ship.sh` builds `kl`) to confirm `opentelemetry-otlp`'s blocking reqwest compiles there with `rustls-no-provider` — the collector URL is plain `http://`, so no TLS provider is ever needed.

- [ ] **Step 4: Let tenant pods reach the collector, and give them the URL**

`crates/workspaces/src/k8s/policies.rs` `default_policies`, add after `allow-dns`:

```rust
        policy(
            "allow-otlp",
            ns,
            owner,
            owner_ref,
            // The tool server and the bench export spans to the collector on their own node.
            // `allow_internet_egress` excludes the cluster, so without this the export fails
            // (harmlessly — it drops) and the bench → tool server hop never reaches HyperDX.
            // One peer, both selectors: the namespace alone would admit all of kube-system.
            // HTTP only: nothing in a tenant pod speaks gRPC to it.
            json!({
                "podSelector": {},
                "policyTypes": ["Egress"],
                "egress": [{
                    "to": [{
                        "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": "kube-system" } },
                        "podSelector": { "matchLabels": { "app": "kloudlite-otel-agent" } },
                    }],
                    "ports": [{ "protocol": "TCP", "port": 4318 }],
                }],
            }),
        ),
```

Update the module `//!` header's list to name "the collector's OTLP port". In `k8s/workspace.rs` next to `var("KL_WORKSPACE", …)` and in `k8s/bench.rs` next to `var("KL_BENCH_IDLE_SECS", …)`:

```rust
        var("KLOUDLITE_OTLP_URL", "http://kloudlite-otel-agent-otlp.kube-system.svc:4318".to_string()),
        var("OTEL_SERVICE_NAME", "kl-ide".to_string()),   // "harness-bench" in bench.rs
```

Match `var`'s argument types to its signature there. If a snapshot test pins the pod's env list, update it.

- [ ] **Step 5: Bench tracing and the tool-call header**

Copy `web/apps/web/src/lib/tracing.ts` to `harness/bench/src/tracing.ts` verbatim, then append:

```ts
import { propagation, context as otelContext } from "@opentelemetry/api";

/** The active context as a W3C header, for a child process that makes the next hop itself. */
export function traceparent(): string | undefined {
  const carrier: Record<string, string> = {};
  propagation.inject(otelContext.active(), carrier);
  return carrier.traceparent;
}
```

(move that import to the top with the others). `harness/package.json` `dependencies`: the same eight `@opentelemetry/*` packages and versions as Task 5 Step 1, then `cd harness && npm install`.

`harness/bench/src/main.ts`, on the server path right after the lock is taken (before `import("./server.ts")`):

```ts
if (process.env.KLOUDLITE_OTLP_URL) {
  const { startTracing } = await import("./tracing.ts");
  startTracing(process.env.OTEL_SERVICE_NAME ?? "harness-bench", process.env.KLOUDLITE_OTLP_URL);
}
```

It must stay after the `--ping` branch: the readiness probe has a 1 s budget and must not load the SDK.

`harness/bench/src/rpc-child.ts:51`, the spawn env:

```ts
    const { traceparent } = await import("./tracing.ts");
    const tp = traceparent();
    const env = { ...process.env, ...(o.tools ? { KL_TOOLS_WORKSPACE: o.tools } : {}), ...(tp ? { KL_TRACEPARENT: tp } : {}) };
    const child = spawn(bin, args, { stdio: ["pipe", "pipe", "pipe"], env, cwd: o.cwd ?? process.env.HOME });
```

If the enclosing function is not `async`, import `traceparent` statically at the top of `rpc-child.ts` instead. `harness/pi/workspace-tools.ts:100`:

```ts
        // The bench's trace, handed down at spawn: pi's RPC protocol has no field for it per call.
        const headers: Record<string, string> = { "content-type": "application/json" };
        if (process.env.KL_TRACEPARENT) headers.traceparent = process.env.KL_TRACEPARENT;
        const r = await fetch(`http://${at}/tools/${c.tool}`, { method: "POST", headers, body: JSON.stringify(c.args), signal });
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p kloudlite-ide -p kloudlite-workspaces && cargo clippy --workspace --all-targets -- -D warnings && (cd harness && npm run bench:test && npm run typecheck)`
Expected: PASS.

- [ ] **Step 7: Exercise and commit**

On a test region: open a bench thread on a workspace, run one tool, then `clickstack_search` Traces `ServiceName = 'kl-ide' AND SpanName = 'POST /tools/{name}'` and open the waterfall; its parent must be a `harness-bench` span. On AKS (no NetworkPolicy engine) this passes without Step 4; on a k3s region it proves `allow-otlp`.

```bash
git add harness crates/ide bins/kl crates/workspaces Cargo.lock
git commit -m "Trace bench tool calls into the workspace tool server"
```

---

### Task 7: ingress-nginx spans

**Files:**
- Modify: `deploy/ingress-nginx-config.yaml`, `deploy/kloudlite.yaml` (nothing else — the node-local Service from Task 2 is the target)

**Interfaces:**
- Consumes: Task 2's `kloudlite-otel-agent-otlp` Service (gRPC 4317).
- Produces: `ingress-nginx` spans as the parent of web/api spans.

- [ ] **Step 1: Confirm the controller supports the keys**

Run: `kubectl -n ingress-nginx get deploy ingress-nginx-controller -o jsonpath='{.spec.template.spec.containers[0].image}'; kubectl -n ingress-nginx exec deploy/ingress-nginx-controller -- ls /etc/nginx/modules/ | grep -i otel`
Expected: a controller image ≥ v1.10 and an `otel_ngx_module.so`. If the module is absent, stop: this task needs the controller's `opentelemetry` init container (Helm `controller.opentelemetry.enabled=true`), which is a Helm change the owner must approve.

- [ ] **Step 2: Add the keys**

Append to `data:` in `deploy/ingress-nginx-config.yaml`, and add a paragraph to its header comment:

```yaml
  # OpenTelemetry (the controller's built-in module). Spans go to the collector on the
  # controller's own node over gRPC — the only protocol this module speaks. `trust-incoming-span`
  # continues a caller's `traceparent`, which is how the probe's sampled flag survives the edge
  # (anyone can send one; the per-IP rate limits above bound what that costs). Parent-based with a
  # 10 % root ratio, matching `traceSampleRatio`'s default; it is static here because nginx reads
  # no Live setting — a changed ratio needs this file changed too.
  enable-opentelemetry: "true"
  opentelemetry-trust-incoming-span: "true"
  opentelemetry-operation-name: "HTTP $request_method $service_name"
  otlp-collector-host: "kloudlite-otel-agent-otlp.kloudlite.svc"
  otlp-collector-port: "4317"
  otel-service-name: "ingress-nginx"
  otel-sampler: "TraceIdRatioBased"
  otel-sampler-ratio: "0.1"
  otel-sampler-parent-based: "true"
  otel-max-queuesize: "2048"
  otel-schedule-delay-millis: "5000"
  otel-max-export-batch-size: "512"
```

Also add `"trace_id":"$opentelemetry_trace_id",` to `log-format-upstream` right after `"req_id":"$req_id",`, and extend the collector's `trace_parser` `if` in both yaml copies to also fire on `attributes.trace_id != nil` with a second operator:

```yaml
          - type: trace_parser
            if: 'attributes.trace_id != nil and attributes.trace_id != ""'
            trace_id:
              parse_from: attributes.trace_id
```

- [ ] **Step 3: Apply and exercise**

Run: `kubectl apply --server-side -f deploy/ingress-nginx-config.yaml && kubectl -n ingress-nginx rollout restart deploy/ingress-nginx-controller && kubectl -n ingress-nginx rollout status deploy/ingress-nginx-controller`
Then send the Task 2 Step 12 curl again. Expected: the waterfall's root is `ingress-nginx`, with `kloudlite-api` under it, and the `ingress.access` log line for that request carries the same TraceId. Health checks through the ingress (`/api/health`) appear as nginx spans only at 10 % and never below it.

- [ ] **Step 4: Commit**

```bash
git add deploy/ingress-nginx-config.yaml deploy/k3s/otel-agent.yaml deploy/kloudlite.yaml
git commit -m "Emit ingress-nginx spans into the trace"
```

---

### Task 8: Prove tracing costs under 5 %

**Files:**
- Create: `deploy/dev/trace-perf.sh`
- Modify: `deploy/dev/README.md` (one line naming the script)

**Interfaces:**
- Consumes: Tasks 2–7 deployed; `deploy/dev/run-suite.sh`; the ClickStack MCP / `clickstack_sql`.
- Produces: the merge gate verdict, recorded in the merge commit body.

- [ ] **Step 1: Write the load script**

`deploy/dev/trace-perf.sh`:

```bash
#!/usr/bin/env bash
# Fixed in-cluster load against srv, api, agent and web, for the tracing before/after comparison.
#   deploy/dev/trace-perf.sh <label>        e.g. off-1, on-1
# Runs `oha` in a throwaway pod in the cluster (laptop RTT hides the real numbers), 50 rps for
# 300 s per target, and prints each target's p50/p95/p99. CPU and memory are read afterwards from
# ClickStack over the same window (see the plan's Task 8). Needs $TOKEN (a probe owner's token).
set -euo pipefail
LABEL=${1:?label}
: "${TOKEN:?export a probe owner token}"
NS=kloudlite
targets=(
  "srv|http://kloudlite-srv.kloudlite.svc:8081/api/slo-probe/perf/branches"
  "api|http://kloudlite-api.kloudlite.svc:8080/v1/regions"
  "web|http://kloudlite-web.kloudlite.svc:3000/"
  "agent|http://kloudlite-api.kloudlite.svc:8080/v1/workspaces"
)
echo "start $(date -u +%FT%TZ) $LABEL"
for t in "${targets[@]}"; do
  name=${t%%|*}; url=${t#*|}
  kubectl -n "$NS" run "perf-$name-$RANDOM" --rm -i --restart=Never --image=ghcr.io/hatoo/oha:1.4.6 -- \
    -z 300s -q 50 --no-tui --json -H "authorization: Bearer $TOKEN" -H "x-kloudlite-peer: ${PEER:-}" "$url" \
    | python3 -c 'import json,sys; d=json.load(sys.stdin)["latencyPercentiles"]; print("'"$name"'", "p50", d["p50"], "p95", d["p95"], "p99", d["p99"])'
done
echo "end $(date -u +%FT%TZ) $LABEL"
```

Before first use, replace each URL with a real read on that tier: confirm ports with `kubectl -n kloudlite get svc`, pick an existing probe-owned repo for srv (a browse GET on the peer listener needs `x-kloudlite-peer`, so export `PEER` from the `kloudlite-peer` Secret in the pod, never on the laptop), and for "agent" use a `/v1` read that makes the agent reconcile nothing but exercises kube reads (`/v1/workspaces`) — the agent's own load comes from the probe suites in Step 3. Pin `oha`'s image to a tag that exists (`crane ls ghcr.io/hatoo/oha` or the project's releases) rather than the one above if it is absent. `chmod +x`.

- [ ] **Step 2: Baseline with tracing off**

Set `KLOUDLITE_OTLP_URL` to empty on srv/api/worker/gateway/agent/web (one `kubectl set env … KLOUDLITE_OTLP_URL-` per workload) and wait for the rollouts. Run `deploy/dev/trace-perf.sh off-1`, `off-2`, `off-3`, recording each start/end. Run `deploy/dev/run-suite.sh fast` three times and `hourly` once.

- [ ] **Step 3: Measure with tracing on**

Restore the env (re-apply `deploy/kloudlite.yaml` and the agent DaemonSet), `traceSampleRatio` at its default 0.1. Run `trace-perf.sh on-1..on-3` and the same suite runs.

- [ ] **Step 4: Compare**

Probe p95s per SLO id, before vs after (the admin process stores every run):

```sql
SELECT id, quantile(0.95)(ms) AS p95, count() FROM kloudlite.events FINAL
WHERE kind = 'slo.step' AND ts BETWEEN {off_start} AND {off_end} GROUP BY id ORDER BY id
```

Run the same for the `on` window. Read the real table and column names first (`clickstack_sql`: `SHOW CREATE TABLE kloudlite.events`, and `crates/workspaces/src/slo/` for where step durations are written) and adjust the query; the comparison is per step id.

CPU and memory per workload over each load window — find the metric names first with `clickstack_list_metrics` filtered to `k8s.pod.cpu` and `k8s.pod.memory` (kubeletstats emits `k8s.pod.cpu.usage` or `k8s.pod.cpu.utilization` depending on collector version; `k8s.pod.memory.working_set`), then:

```sql
SELECT ResourceAttributes['service.name'] AS svc, avg(Value) AS v
FROM default.otel_metrics_gauge
WHERE MetricName = 'k8s.pod.cpu.usage' AND TimeUnix BETWEEN {start} AND {end}
  AND ResourceAttributes['service.name'] IN ('kloudlite-srv','kloudlite-api','kloudlite-agent','kloudlite-web')
GROUP BY svc
```

Also read the SDK's dropped-span evidence: the batch processor logs once when it first drops (`grep -i 'dropped' ` in `clickstack_search` Logs for each service over the `on` windows).

- [ ] **Step 5: Apply the gate**

Pass: every probe step's p95 (median of three runs) and every tier's load p95, CPU and memory are within +5 % of baseline. Fail: set `traceSampleRatio` to 0.02 in the central document and `ClusterSettings`, re-run Step 3; if still failing, remove the span whose volume dominates (`clickstack_table` Traces grouped by `SpanName` count over the `on` window — the likely candidates are `kube` client spans and `reconcile` spans), and re-run. Record the table of numbers and the final ratio in the commit body.

- [ ] **Step 6: Commit**

```bash
git add deploy/dev/trace-perf.sh deploy/dev/README.md
git commit -m "Add the in-cluster tracing cost measurement"
```

---

## Self-review

- Coverage: W3C everywhere (T1 propagator, T5/T6 SDK default, T7 nginx); every named hop — probe (T2 S8), nginx (T7), web (T5), api→srv (T2 S7), agent peer (T3 S5), builder-gate→api (T4), bench→tool server (T6), kube client spans with layer (T3 S7); `x-request-id` unchanged and `trace_id` on logs (T2 S3–4, S10; T7 S2); batched/bounded/drop export (T1 S7–8, T5 S4); sampling rules and Live ratio in both stores (T1 S4–5, T2 S5–6, T3 S3–4); no per-chunk/per-event spans, health never traced (T1 S6, T2 S1, T3 S5–7); Node with http+undici only (T5, T6); nginx via ConfigMap (T7); collector + 7-day TTL (T2 S10–11); perf proof and gate (T8); tests per hop, sampler, exporter-down (T1, T2, T3, T4, T5, T6).
- Names used across tasks match Task 1's Produces block: `layer`, `layer_with`, `bind_ratio`, `DEFAULT_RATIO`, `server_span`, `finish`, `client_span`, `stamp`, `inject`, `inject_reqwest`, `traced`, `UNTRACED`, `untraced`, `SLOW`, `testing::subscriber`, `trace_sample_ratio`/`traceSampleRatio`, `kloudlite-otel-agent-otlp`.
