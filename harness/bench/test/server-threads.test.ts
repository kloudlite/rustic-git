import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import WebSocket from "ws";
import { Bench } from "../src/bench.ts";
import { serve } from "../src/server.ts";
import { FAKE } from "./fake-pi.ts";

test("a workspace thread opens over HTTP, streams on its socket and reads back as the workspace's messages", async () => {
  const bench = new Bench({ dir: fs.mkdtempSync(path.join(os.tmpdir(), "bench-srvt-")), readOnly: false, model: "fake/m", bin: FAKE });
  await bench.start();
  const srv = await serve(bench, 0);
  try {
    const base = `http://127.0.0.1:${srv.port}`;
    const j = async (method: string, p: string) => {
      const r = await fetch(base + p, { method });
      return { status: r.status, body: await r.json() };
    };

    const opened = await j("POST", "/workspaces/api/session");
    assert.equal(opened.status, 200);
    assert.equal(opened.body.id, "w-api");
    const w = new WebSocket(`ws://127.0.0.1:${srv.port}/sessions/w-api/rpc`);
    try {
      await new Promise((r) => w.once("open", r));
      const done = new Promise((r) => w.on("message", (d) => JSON.parse(d.toString()).type === "agent_end" && r(undefined)));
      w.send(JSON.stringify({ id: "1", type: "prompt", message: "hi" }));
      await done;
      assert.equal((await j("GET", "/workspaces/api/messages")).body.total, 2);
      assert.deepEqual((await j("GET", "/workspaces/web/messages")).body, { messages: [], total: 0 });
      assert.equal((await j("POST", "/workspaces/..%2Fx/session")).status, 400);
      assert.equal((await j("POST", "/workspaces/api/eph/api-eph-1/session")).body.id, "e-api-eph-1");
      assert.deepEqual((await j("GET", "/exchanges?workspace=api")).body, [], "the exchange view moved, it did not go");
    } finally {
      w.close();
    }
  } finally {
    await bench.stop();
    await srv.close();
  }
});
