// Its own process, with no SDK installed: that is pi's child as it runs. With the bench's provider
// in the same process, undici's instrumentation replaces the header with the active context's.
import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { ToolServer, traceHeaders, TRACE_MAX_AGE_S } from "../../pi/workspace-tools.ts";

test("a tool call carries KL_TRACEPARENT's trace, and nothing when it is unset", async () => {
  const seen: string[] = [];
  const srv = http.createServer((req, res) => {
    seen.push(String(req.headers.traceparent ?? ""));
    res.writeHead(200, { "content-type": "application/json" });
    res.end("{}");
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const at = `127.0.0.1:${(srv.address() as { port: number }).port}`;
  const tools = new ToolServer("ws", async () => at);
  const tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
  process.env.KL_TRACEPARENT = tp;
  await tools.call({ tool: "read", args: { path: "a" } });
  delete process.env.KL_TRACEPARENT;
  await tools.call({ tool: "read", args: { path: "a" } });
  srv.close();
  assert.deepEqual(seen, [tp, ""]);
});

test("a child past an hour stops joining its spawn trace, and the probe marker rides along before that", async () => {
  const tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
  assert.deepEqual(traceHeaders({ KL_TRACEPARENT: tp }, TRACE_MAX_AGE_S - 1), { traceparent: tp });
  assert.deepEqual(traceHeaders({ KL_TRACEPARENT: tp, KL_PROBE: "1" }, 1), { traceparent: tp, "x-kloudlite-probe": "1" });
  assert.deepEqual(traceHeaders({ KL_TRACEPARENT: tp, KL_PROBE: "1" }, TRACE_MAX_AGE_S), {});
  assert.deepEqual(traceHeaders({ KL_PROBE: "1" }, 1), {});

  const seen: http.IncomingHttpHeaders[] = [];
  const srv = http.createServer((req, res) => {
    seen.push(req.headers);
    res.writeHead(200, { "content-type": "application/json" });
    res.end("{}");
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const at = `127.0.0.1:${(srv.address() as { port: number }).port}`;
  process.env.KL_TRACEPARENT = tp;
  process.env.KL_PROBE = "1";
  await new ToolServer("ws", async () => at).call({ tool: "read", args: {} });
  delete process.env.KL_TRACEPARENT;
  delete process.env.KL_PROBE;
  srv.close();
  assert.equal(seen[0]["x-kloudlite-probe"], "1");
  assert.equal(seen[0].traceparent, tp);
});
