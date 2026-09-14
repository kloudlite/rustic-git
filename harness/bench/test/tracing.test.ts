import { test } from "node:test";
import assert from "node:assert/strict";
import { trace } from "@opentelemetry/api";
import { startTracing, traceparent } from "../src/tracing.ts";

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
