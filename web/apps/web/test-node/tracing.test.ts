// Runs under Node (`node --test`), not bun: instrumentation-http and -undici patch Node's own
// modules, which bun does not load.
import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { ROOT_CONTEXT, SpanKind, SpanStatusCode, trace, TraceFlags } from "@opentelemetry/api";
import { InMemorySpanExporter, SamplingDecision, SimpleSpanProcessor } from "@opentelemetry/sdk-trace-base";
import { KlPropagator, KlSampler, Promote, startTracing } from "../src/lib/tracing.ts";

const TID = "4bf92f3577b34da6a3ce929d0e0e4736";
const remote = (flags: number, probe = false) => {
  const cx = trace.setSpanContext(ROOT_CONTEXT, { traceId: TID, spanId: "00f067aa0ba902b7", traceFlags: flags, isRemote: true });
  return probe ? new KlPropagator().extract(cx, { "x-kloudlite-probe": "1" }, { get: (c, k) => (c as Record<string, string>)[k], keys: Object.keys }) : cx;
};
const decide = (s: KlSampler, cx = ROOT_CONTEXT, tid = TID) => s.shouldSample(cx, tid, "n", SpanKind.SERVER, {}, []).decision;

test("sampler: no probe header, a sampled outside parent still lets the ratio decide", () => {
  const s = new KlSampler(0, 2, 2);
  assert.equal(decide(s, remote(TraceFlags.SAMPLED)), SamplingDecision.RECORD, "outside caller cannot force a sample");
  assert.equal(decide(new KlSampler(1, 2, 2), remote(TraceFlags.SAMPLED)), SamplingDecision.RECORD_AND_SAMPLED, "ratio can still sample it");
});

// ingress-nginx (Task 7) never trusts an incoming traceparent and samples at 10%, so a probe
// request reaches this tier with `-00` nine times in ten — the bucket must catch it regardless.
test("sampler: a probe header is sampled through the bucket regardless of the incoming flag", () => {
  const s = new KlSampler(0, 2, 2);
  assert.equal(decide(s, remote(TraceFlags.NONE, true)), SamplingDecision.RECORD_AND_SAMPLED, "unsampled parent, bucket decides");
  assert.equal(decide(s, remote(TraceFlags.SAMPLED, true)), SamplingDecision.RECORD_AND_SAMPLED);
  assert.equal(decide(s, remote(TraceFlags.NONE, true)), SamplingDecision.RECORD, "past the burst the ratio decides");
});

test("sampler: local parent decides, and a root is never dropped", () => {
  const local = trace.setSpanContext(ROOT_CONTEXT, { traceId: TID, spanId: "00f067aa0ba902b7", traceFlags: TraceFlags.SAMPLED, isRemote: false });
  assert.equal(decide(new KlSampler(0), local), SamplingDecision.RECORD_AND_SAMPLED, "a local parent decides");
  assert.equal(decide(new KlSampler(0), ROOT_CONTEXT, "ffffffffffffffffffffffffffffffff"), SamplingDecision.RECORD, "never dropped");
  assert.equal(decide(new KlSampler(1), ROOT_CONTEXT, "00000000000000000000000000000001"), SamplingDecision.RECORD_AND_SAMPLED);
});

const mk = (id: string, parent: string | undefined, code: SpanStatusCode, ms: number) =>
  ({
    name: "GET",
    attributes: {},
    events: [],
    resource: { attributes: {} },
    spanContext: () => ({ traceId: TID, spanId: id, traceFlags: TraceFlags.NONE }),
    parentSpanContext: parent ? { traceId: TID, spanId: parent, traceFlags: 0, isRemote: false } : undefined,
    status: { code },
    duration: [Math.floor(ms / 1000), (ms % 1000) * 1e6],
  }) as never;

test("promote: an errored or slow local root keeps its unsampled trace, a fast ok one does not, capped", () => {
  const out = new InMemorySpanExporter();
  const p = new Promote(new SimpleSpanProcessor(out), 0, 2);
  p.onEnd(mk("0000000000000002", "0000000000000001", SpanStatusCode.UNSET, 1));
  p.onEnd(mk("0000000000000001", undefined, SpanStatusCode.UNSET, 1));
  assert.equal(out.getFinishedSpans().length, 0);
  p.onEnd(mk("0000000000000003", "0000000000000004", SpanStatusCode.UNSET, 1));
  p.onEnd(mk("0000000000000004", undefined, SpanStatusCode.ERROR, 1));
  assert.equal(out.getFinishedSpans().length, 2);
  p.onEnd(mk("0000000000000005", undefined, SpanStatusCode.UNSET, 1500));
  assert.equal(out.getFinishedSpans().length, 3, "slow over one second");
  p.onEnd(mk("0000000000000006", undefined, SpanStatusCode.ERROR, 1));
  assert.equal(out.getFinishedSpans().length, 3, "past the promote bucket a failure stays unexported");
});

test("export view: raw paths, queries, cookies and auth never leave the process", () => {
  const out = new InMemorySpanExporter();
  const p = new Promote(new SimpleSpanProcessor(out), 10, 10);
  const span = {
    ...(mk("0000000000000007", undefined, SpanStatusCode.ERROR, 1) as object),
    name: "fetch GET http://api:8080/v1/acme/secret-repo?token=x",
    attributes: { "http.route": undefined, "url.full": "http://api/acme/secret-repo?x=1", "http.target": "/acme/secret-repo", "http.request.header.cookie": "c", "http.request.method": "GET" },
    status: { code: SpanStatusCode.ERROR, message: "fetch http://api/acme/secret-repo?token=x failed" },
    events: [
      { name: "exception", attributes: { "exception.message": "GET /acme/secret-repo failed", "exception.type": "Error" } },
      { name: "loaded /acme/secret-repo", attributes: {} },
    ],
  } as never;
  p.onEnd(span);
  const s = out.getFinishedSpans()[0];
  const dump = JSON.stringify({ name: s.name, attributes: s.attributes, events: s.events, status: s.status });
  assert.equal(s.name, "fetch GET");
  assert.deepEqual(s.status, { code: SpanStatusCode.ERROR });
  assert.deepEqual(s.events.map((e) => e.name), ["exception"]);
  for (const bad of ["acme", "secret-repo", "token", "cookie", "?"]) assert.ok(!dump.includes(bad), `${bad} leaked: ${dump}`);
  const routed = { ...(mk("0000000000000008", undefined, SpanStatusCode.ERROR, 1) as object), name: "GET", attributes: { "http.route": "/[owner]/[repo]", "http.target": "/acme/x" } } as never;
  p.onEnd(routed);
  assert.equal(out.getFinishedSpans()[1].name, "GET /[owner]/[repo]");
});

const hex16 = (n: number) => n.toString(16).padStart(16, "0");
const child = (t: number, id: number) =>
  ({ ...(mk(hex16(id), "00000000000000ff", SpanStatusCode.UNSET, 1) as object), spanContext: () => ({ traceId: hex16(t).padStart(32, "0"), spanId: hex16(id), traceFlags: TraceFlags.NONE }) }) as never;

test("promote: a flood of waiting spans never holds more than 16384", () => {
  const p = new Promote(new SimpleSpanProcessor(new InMemorySpanExporter()), 0, 0);
  let id = 1;
  for (let t = 1; t <= 40; t++) for (let i = 0; i < 511; i++) p.onEnd(child(t, id++));
  for (let t = 100; t < 20_100; t++) p.onEnd(child(t, id++));
  assert.ok(p.pendingTotal <= 16_384 && p.pendingTotal > 16_000, `pending ${p.pendingTotal}`);
});

test("promote: a child ending after its promoted root is still exported", () => {
  const out = new InMemorySpanExporter();
  const p = new Promote(new SimpleSpanProcessor(out), 10, 10);
  p.onEnd(mk("0000000000000009", undefined, SpanStatusCode.ERROR, 1));
  assert.equal(out.getFinishedSpans().length, 1);
  p.onEnd(mk("000000000000000a", "0000000000000009", SpanStatusCode.UNSET, 1));
  assert.equal(out.getFinishedSpans().length, 2);
});

const listen = (h: http.RequestListener) =>
  new Promise<[http.Server, number]>((ok) => {
    const s = http.createServer(h).listen(0, "127.0.0.1", () => ok([s, (s.address() as { port: number }).port]));
  });

test("round trip: an incoming traceparent and probe marker reach the downstream fetch, replaced not duplicated", async () => {
  const out = new InMemorySpanExporter();
  const provider = startTracing("web-test", "http://127.0.0.1:9", out);
  const seen: http.IncomingHttpHeaders[] = [];
  const [down, downPort] = await listen((req, res) => {
    seen.push(req.headers);
    res.end("ok");
  });
  const [up, upPort] = await listen(async (_req, res) => {
    await fetch(`http://127.0.0.1:${downPort}/`, { headers: { traceparent: "00-11111111111111111111111111111111-1111111111111111-01" } });
    res.end("ok");
  });
  const call = (headers: http.OutgoingHttpHeaders) =>
    new Promise<void>((ok) => http.get({ port: upPort, host: "127.0.0.1", path: "/acme/secret-repo?q=1", headers }, (r) => r.resume().on("end", ok)));
  await call({ traceparent: `00-${TID}-00f067aa0ba902b7-01`, "x-kloudlite-probe": "1" });
  await call({ traceparent: `00-${TID}-00f067aa0ba902b7-01` });
  up.close();
  down.close();
  await provider.forceFlush();
  assert.equal(String(seen[0].traceparent).slice(3, 35), TID);
  assert.ok(!String(seen[0].traceparent).includes(","), "one traceparent, not two");
  assert.equal(seen[0]["x-kloudlite-probe"], "1");
  assert.equal(String(seen[1].traceparent).slice(3, 35), TID, "the trace id is continued without the marker too");
  assert.equal(seen[1]["x-kloudlite-probe"], undefined);
  const dump = JSON.stringify(out.getFinishedSpans().map((s) => [s.name, s.attributes]));
  assert.ok(out.getFinishedSpans().length >= 2, dump);
  for (const bad of ["secret-repo", "q=1"]) assert.ok(!dump.includes(bad), `${bad} leaked: ${dump}`);
});
