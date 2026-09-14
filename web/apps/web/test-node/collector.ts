// Shared by the two collector-outage tests: each runs in its own `node --test` process, because
// `startTracing` installs one global provider per process.
import assert from "node:assert/strict";
import http from "node:http";
import { startTracing } from "../src/lib/tracing.ts";

export async function requestsSurvive(collector: string) {
  process.env.KLOUDLITE_TRACE_SAMPLE_RATIO = "1";
  startTracing("web-test", collector);
  const srv = http.createServer((_q, r) => r.end("ok")).listen(0, "127.0.0.1");
  await new Promise((ok) => srv.once("listening", ok));
  const port = (srv.address() as { port: number }).port;
  const started = performance.now();
  // More than the 2048-span queue, so the drop path runs too.
  for (let i = 0; i < 1500; i++) assert.equal((await fetch(`http://127.0.0.1:${port}/`)).status, 200);
  const ms = performance.now() - started;
  srv.close();
  assert.ok(ms < 15_000, `requests took ${ms} ms`);
}
