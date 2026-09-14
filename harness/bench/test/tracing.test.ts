import { test } from "node:test";
import assert from "node:assert/strict";
import { context, ROOT_CONTEXT, SpanStatusCode, trace, TraceFlags } from "@opentelemetry/api";
import { InMemorySpanExporter, SamplingDecision, SimpleSpanProcessor } from "@opentelemetry/sdk-trace-base";
import { childTraceEnv, KlPropagator, KlSampler, Promote, startTracing, traceparent } from "../src/tracing.ts";

test("the child's traceparent is the active span's trace", async () => {
  assert.equal(traceparent(), undefined, "no provider, no header");
  startTracing("bench-test", "http://127.0.0.1:9");
  await trace.getTracer("t").startActiveSpan("session", async (span) => {
    const tp = traceparent();
    assert.ok(tp);
    assert.equal(tp.slice(3, 35), span.spanContext().traceId);
    span.end();
  });
});

const TID = "4bf92f3577b34da6a3ce929d0e0e4736";
const getter = { get: (c: unknown, k: string) => (c as Record<string, string>)[k], keys: (c: unknown) => Object.keys(c as object) };

test("a child spawned inside a probe request carries KL_PROBE, and only then", () => {
  startTracing("bench-test", "http://127.0.0.1:9");
  const tp = `00-${TID}-00f067aa0ba902b7-01`;
  const probe = new KlPropagator().extract(ROOT_CONTEXT, { traceparent: tp, "x-kloudlite-probe": "1" }, getter);
  const plain = new KlPropagator().extract(ROOT_CONTEXT, { traceparent: tp }, getter);
  const a = context.with(probe, childTraceEnv);
  assert.equal(a.KL_PROBE, "1");
  assert.equal(a.KL_TRACEPARENT?.slice(3, 35), TID);
  const b = context.with(plain, childTraceEnv);
  assert.equal(b.KL_PROBE, undefined);
  assert.equal(b.KL_TRACEPARENT?.slice(3, 35), TID);
});

const remoteCx = (flags: number, probe = false) => {
  const cx = trace.setSpanContext(ROOT_CONTEXT, { traceId: TID, spanId: "00f067aa0ba902b7", traceFlags: flags, isRemote: true });
  return probe ? new KlPropagator().extract(cx, { "x-kloudlite-probe": "1" }, getter) : cx;
};
const decide = (s: KlSampler, cx = ROOT_CONTEXT, tid = TID) => s.shouldSample(cx, tid, "n", 0, {}, []).decision;

test("sampler: no probe header, a sampled outside parent still lets the ratio decide", () => {
  const s = new KlSampler(0, 2, 2);
  assert.equal(decide(s, remoteCx(TraceFlags.SAMPLED)), SamplingDecision.RECORD, "outside caller cannot force a sample");
  assert.equal(decide(new KlSampler(1, 2, 2), remoteCx(TraceFlags.SAMPLED)), SamplingDecision.RECORD_AND_SAMPLED, "ratio can still sample it");
});

// ingress-nginx (Task 7) never trusts an incoming traceparent and samples at 10%, so a probe
// request reaches this tier with `-00` nine times in ten — the bucket must catch it regardless.
test("sampler: a probe header is sampled through the bucket regardless of the incoming flag", () => {
  const s = new KlSampler(0, 2, 2);
  assert.equal(decide(s, remoteCx(TraceFlags.NONE, true)), SamplingDecision.RECORD_AND_SAMPLED, "unsampled parent, bucket decides");
  assert.equal(decide(s, remoteCx(TraceFlags.SAMPLED, true)), SamplingDecision.RECORD_AND_SAMPLED);
  assert.equal(decide(s, remoteCx(TraceFlags.NONE, true)), SamplingDecision.RECORD, "past the burst the ratio decides");
});

const mk = (id: string, code: SpanStatusCode) =>
  ({
    name: "GET",
    attributes: {},
    events: [],
    resource: { attributes: {} },
    spanContext: () => ({ traceId: TID, spanId: id, traceFlags: TraceFlags.NONE }),
    parentSpanContext: undefined,
    status: { code },
    duration: [0, 1e6],
  }) as object;

// web's "export view" test, against the bench's copy: a tool call's workspace path, a session id
// in a query, an error message and a stray event must not leave the process.
test("export view: allow-listed attributes, status code only, exception events only", () => {
  const out = new InMemorySpanExporter();
  const p = new Promote(new SimpleSpanProcessor(out), 10, 10);
  p.onEnd({
    ...mk("0000000000000007", SpanStatusCode.ERROR),
    name: "fetch POST http://10.42.0.9:7788/tools/read?session=secret-session",
    attributes: { "url.full": "http://10.42.0.9:7788/tools/read?session=secret-session", "http.target": "/home/kl/workspaces/secret-ws/a.rs", "http.request.header.cookie": "c", "http.request.method": "POST", "http.response.status_code": 500 },
    status: { code: SpanStatusCode.ERROR, message: "read /home/kl/workspaces/secret-ws/a.rs failed" },
    events: [
      { name: "exception", attributes: { "exception.message": "ENOENT secret-ws/a.rs", "exception.type": "Error" } },
      { name: "prompt secret-prompt", attributes: {} },
    ],
  } as never);
  const s = out.getFinishedSpans()[0];
  const dump = JSON.stringify({ name: s.name, attributes: s.attributes, events: s.events, status: s.status });
  assert.equal(s.name, "fetch POST");
  assert.deepEqual(s.status, { code: SpanStatusCode.ERROR });
  assert.deepEqual(s.attributes, { "http.request.method": "POST", "http.response.status_code": 500 });
  assert.deepEqual(s.events.map((e) => [e.name, e.attributes]), [["exception", { "exception.type": "Error" }]]);
  for (const bad of ["secret-session", "secret-ws", "secret-prompt", "a.rs", "cookie", "?", "10.42"]) assert.ok(!dump.includes(bad), `${bad} leaked: ${dump}`);
});
