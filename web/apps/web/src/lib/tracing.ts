/** Distributed tracing for the Node tiers: the JS twin of `crates/trace`.
 *
 *  Same rules as the Rust side, value for value:
 *  - `KlSampler`: parent-based and never DROPS. A LOCAL parent decides; a REMOTE parent's sampled
 *    flag is obeyed only when the request carried `x-kloudlite-probe: 1`, and each obeyed sampled
 *    probe root spends a token (20/s, burst 100) — the marker is forgeable, so its effect is
 *    bounded rather than secret. Everything else is a root at the trace-id ratio (0.1), which is a
 *    pure function of the trace id, so every tier with the same ratio re-rolls the same answer.
 *  - `Promote`: an unsampled span is still RECORDED and waits here until this process's local
 *    root ends; a root with ERROR status or over one second (`SLOW_MS`, the Rust `SLOW`) is
 *    exported with its children, capped at 2/s burst 20. The Node SDK has no keep-after-the-fact,
 *    but it needs none: `BatchSpanProcessor.onEnd` only checks `spanContext().traceFlags &
 *    SAMPLED` (sdk-trace `BatchSpanProcessorBase.js`), so a view of the span with the flag set is
 *    all a promotion is.
 *  - `KlPropagator`: W3C trace context plus the probe marker, carried as a context value so every
 *    outgoing `fetch` of a probe request re-sends the header, as `http::inject` does.
 *  - Export view: only a fixed allow-list of attributes leaves the process, a span is named by its
 *    route TEMPLATE (`http.route`, which Next.js stamps on the http server span), and a name
 *    holding a URL or query is cut before it. A raw path carries owner and repo names; a private
 *    repository's name must not land in trace storage. Exception events keep only their type.
 *  - Export is OTLP/HTTP protobuf through a bounded batch processor (queue 2048, batch 512, 5 s
 *    delay and timeout): a full queue or a dead collector drops, and nothing waits on it.
 *  - Only `node:http` incoming (outgoing off, so the exporter's own requests are not traced) and
 *    `fetch` (undici). Health and metrics paths are never traced.
 *
 *  No relative or `@/` imports: this file must load under plain `node --test` too.
 *  ponytail: the web tier has no LiveSettings and `/v1/settings/central` does not serve the
 *  trace fields, so ratio and buckets come from env with the Rust defaults
 *  (`KLOUDLITE_TRACE_SAMPLE_RATIO`, `_PROBE_RATE`, `_PROBE_BURST`, `_PROMOTE_RATE`,
 *  `_PROMOTE_BURST`), read once at start; the upgrade is serving them from that route and reading
 *  them through `lib/clone.ts`'s cached `centralSettings()` on a beat. */
import {
  createContextKey,
  SamplingDecision,
  SpanStatusCode,
  trace,
  TraceFlags,
  type Attributes,
  type Context,
  type TextMapGetter,
  type TextMapPropagator,
  type TextMapSetter,
} from "@opentelemetry/api";
import { W3CTraceContextPropagator } from "@opentelemetry/core";
import {
  BatchSpanProcessor,
  TraceIdRatioBasedSampler,
  type ReadableSpan,
  type Sampler,
  type SamplingResult,
  type Span,
  type SpanExporter,
  type SpanProcessor,
} from "@opentelemetry/sdk-trace-base";
import { NodeTracerProvider } from "@opentelemetry/sdk-trace-node";
import { OTLPTraceExporter } from "@opentelemetry/exporter-trace-otlp-proto";
import { registerInstrumentations } from "@opentelemetry/instrumentation";
import { HttpInstrumentation } from "@opentelemetry/instrumentation-http";
import { UndiciInstrumentation } from "@opentelemetry/instrumentation-undici";
import { resourceFromAttributes } from "@opentelemetry/resources";
import { createRequire } from "node:module";

const SLOW_MS = 1_000;
const MAX_TRACES = 4096;
const MAX_SPANS = 512;
export const PROBE_HEADER = "x-kloudlite-probe";
const UNTRACED = new Set(["/healthz", "/readyz", "/livez", "/metrics", "/api/health"]);
const PROBE = createContextKey("kloudlite probe");

function env(name: string, fallback: number): number {
  const v = Number(process.env[name]);
  return process.env[name] && Number.isFinite(v) && v >= 0 ? v : fallback;
}

class Bucket {
  private tokens: number;
  private at = performance.now();
  private rate: number;
  private burst: number;
  // No parameter properties anywhere in this file: Node's strip-only TypeScript refuses them.
  constructor(rate: number, burst: number) {
    this.rate = rate;
    this.burst = burst;
    this.tokens = burst;
  }
  take(): boolean {
    const now = performance.now();
    this.tokens = Math.min(this.burst, this.tokens + ((now - this.at) / 1000) * this.rate);
    this.at = now;
    if (this.tokens < 1) return false;
    this.tokens -= 1;
    return true;
  }
}

export class KlSampler implements Sampler {
  private root: TraceIdRatioBasedSampler;
  private probes: Bucket;
  constructor(
    ratio = env("KLOUDLITE_TRACE_SAMPLE_RATIO", 0.1),
    probeRate = env("KLOUDLITE_TRACE_PROBE_RATE", 20),
    probeBurst = env("KLOUDLITE_TRACE_PROBE_BURST", 100),
  ) {
    this.root = new TraceIdRatioBasedSampler(Math.min(1, ratio));
    this.probes = new Bucket(probeRate, probeBurst);
  }
  shouldSample(cx: Context, traceId: string): SamplingResult {
    const parent = trace.getSpanContext(cx);
    if (parent && trace.isSpanContextValid(parent)) {
      const sampled = (parent.traceFlags & TraceFlags.SAMPLED) !== 0;
      if (!parent.isRemote || (cx.getValue(PROBE) && (!sampled || this.probes.take()))) {
        return { decision: sampled ? SamplingDecision.RECORD_AND_SAMPLED : SamplingDecision.RECORD, traceState: parent.traceState };
      }
    }
    const d = this.root.shouldSample(cx, traceId).decision;
    return { decision: d === SamplingDecision.RECORD_AND_SAMPLED ? d : SamplingDecision.RECORD };
  }
  toString() {
    return "KlSampler";
  }
}

export class KlPropagator implements TextMapPropagator {
  private w3c = new W3CTraceContextPropagator();
  inject(cx: Context, carrier: unknown, setter: TextMapSetter) {
    this.w3c.inject(cx, carrier, setter);
    if (cx.getValue(PROBE)) setter.set(carrier, PROBE_HEADER, "1");
  }
  extract(cx: Context, carrier: unknown, getter: TextMapGetter): Context {
    const out = this.w3c.extract(cx, carrier, getter);
    const v = getter.get(carrier, PROBE_HEADER);
    return (Array.isArray(v) ? v[0] : v) === "1" ? out.setValue(PROBE, true) : out;
  }
  fields() {
    return [...this.w3c.fields(), PROBE_HEADER];
  }
}

const SAFE_ATTRS = new Set([
  "http.request.method",
  "http.method",
  "http.response.status_code",
  "http.status_code",
  "http.route",
  "next.route",
  "next.rsc",
  "next.span_type",
  "next.span_category",
  "error.type",
  "server.address",
  "server.port",
  "url.scheme",
  "network.protocol.version",
]);

/** The span as it may leave the process: allow-listed attributes, a template name, and the
 *  sampled flag when promoted. A view, not a copy — every other field reads through. */
function exported(s: ReadableSpan, promote: boolean): ReadableSpan {
  const attributes: Attributes = {};
  for (const [k, v] of Object.entries(s.attributes)) if (SAFE_ATTRS.has(k) && v !== undefined) attributes[k] = v;
  const route = s.attributes["http.route"];
  const method = s.attributes["http.request.method"] ?? s.attributes["http.method"] ?? s.name.split(" ")[0];
  const name = typeof route === "string" && route ? `${method} ${route}` : s.name.split(/\s+https?:\/\/|\?/)[0];
  const events = (s.events ?? []).map((e) => ({ ...e, attributes: e.attributes?.["exception.type"] ? { "exception.type": e.attributes["exception.type"] } : {} }));
  const props: PropertyDescriptorMap = { name: { value: name }, attributes: { value: attributes }, events: { value: events } };
  if (promote) {
    const c = s.spanContext();
    props.spanContext = { value: () => ({ ...c, traceFlags: c.traceFlags | TraceFlags.SAMPLED }) };
  }
  return Object.create(s, props);
}

export class Promote implements SpanProcessor {
  private pending = new Map<string, ReadableSpan[]>();
  private cap: Bucket;
  private inner: SpanProcessor;
  constructor(inner: SpanProcessor, rate = env("KLOUDLITE_TRACE_PROMOTE_RATE", 2), burst = env("KLOUDLITE_TRACE_PROMOTE_BURST", 20)) {
    this.inner = inner;
    this.cap = new Bucket(rate, burst);
  }
  onStart(span: Span, cx: Context) {
    this.inner.onStart(span, cx);
  }
  onEnd(span: ReadableSpan) {
    const sc = span.spanContext();
    if (sc.traceFlags & TraceFlags.SAMPLED) return this.inner.onEnd(exported(span, false));
    const parent = span.parentSpanContext;
    if (parent && !parent.isRemote) {
      let waiting = this.pending.get(sc.traceId);
      if (!waiting) {
        // ponytail: evict the oldest-started trace when full (Map keeps insertion order); the
        // Rust side's last-touched order is the upgrade if a long request ever loses its children.
        if (this.pending.size >= MAX_TRACES) this.pending.delete(this.pending.keys().next().value!);
        this.pending.set(sc.traceId, (waiting = []));
      }
      if (waiting.length < MAX_SPANS) waiting.push(span);
      return;
    }
    const children = this.pending.get(sc.traceId) ?? [];
    this.pending.delete(sc.traceId);
    const ms = span.duration[0] * 1e3 + span.duration[1] / 1e6;
    if ((span.status.code !== SpanStatusCode.ERROR && ms <= SLOW_MS) || !this.cap.take()) return;
    for (const s of [...children, span]) this.inner.onEnd(exported(s, true));
  }
  forceFlush() {
    return this.inner.forceFlush();
  }
  shutdown() {
    return this.inner.shutdown();
  }
}

type UndiciRequest = { headers: string | unknown[] };
const OURS = /^(traceparent|tracestate|x-kloudlite-probe)$/i;

/** Undici's instrumentation APPENDS its headers, so a caller's own `traceparent` would go out
 *  twice. Its request hook runs before injection, so ours are removed there: replaced, never
 *  duplicated, like `inject_reqwest`. */
function dropOurs(r: UndiciRequest) {
  if (typeof r.headers === "string") {
    r.headers = r.headers
      .split("\r\n")
      .filter((l) => !OURS.test(l.split(":")[0].trim()))
      .join("\r\n");
  } else if (Array.isArray(r.headers)) {
    for (let i = r.headers.length - 2; i >= 0; i -= 2) if (OURS.test(String(r.headers[i]))) r.headers.splice(i, 2);
  }
}

let provider: NodeTracerProvider | undefined;

/** Install the global provider once. `exporter` replaces OTLP in tests. Returns the provider so a
 *  test can flush it. */
export function startTracing(service: string, url: string, exporter?: SpanExporter): NodeTracerProvider {
  if (provider) return provider;
  const out = exporter ?? new OTLPTraceExporter({ url: `${url.replace(/\/$/, "")}/v1/traces`, timeoutMillis: 5_000 });
  provider = new NodeTracerProvider({
    resource: resourceFromAttributes({ "service.name": service }),
    sampler: new KlSampler(),
    spanProcessors: [
      new Promote(new BatchSpanProcessor(out, { maxQueueSize: 2048, maxExportBatchSize: 512, scheduledDelayMillis: 5_000, exportTimeoutMillis: 5_000 })),
    ],
  });
  provider.register({ propagator: new KlPropagator() });
  registerInstrumentations({
    tracerProvider: provider,
    instrumentations: [
      new HttpInstrumentation({
        disableOutgoingRequestInstrumentation: true,
        ignoreIncomingRequestHook: (req) => UNTRACED.has(new URL(req.url ?? "/", "http://x").pathname),
        // Read back by the request-timing log line, which runs outside the span's context.
        requestHook: (span, req) => {
          (req as { klTraceId?: string }).klTraceId = span.spanContext().traceId;
        },
      }),
      new UndiciInstrumentation({ requestHook: (_span, req) => dropOurs(req as unknown as UndiciRequest) }),
    ],
  });
  // The http instrumentation patches `Server.prototype.emit` only when `http` is REQUIRED after
  // it is enabled (`InstrumentationBase.enable` hooks require-in-the-middle; it never patches an
  // already-loaded builtin), and Next has loaded it long before `register()`. One require here
  // applies the patch to the shared prototype, so the server Next creates next is traced.
  createRequire(`${process.cwd()}/`)("http");
  // Every log line written inside a span carries its trace id (`lib/log.ts` reads this).
  (globalThis as { __klTraceId?: () => string | undefined }).__klTraceId = () => {
    const sc = trace.getActiveSpan()?.spanContext();
    return sc && trace.isSpanContextValid(sc) ? sc.traceId : undefined;
  };
  return provider;
}
