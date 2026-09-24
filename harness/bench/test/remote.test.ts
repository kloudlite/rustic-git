import { test, after } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { fakeTools } from "./fake-tools.ts";
import { setBackend, httpBackend, remote } from "../src/engine/remote.ts";
import { TOOLS, runTool } from "../src/engine/index.ts";
import { setBgWaitMs } from "../src/engine/tools.ts";

const fake = await fakeTools((cmd) => ({ exit_code: cmd.includes("false") ? 1 : 0, stdout: `ran ${cmd}`, stderr: "" }));
after(() => fake.close());
setBackend("ws1", httpBackend(async () => ({ address: fake.address })));
const tool = (n: string) => TOOLS.find((t) => t.name === n)!;

test("remote posts to /tools/{name} and a 4xx is an error string, not a throw", async () => {
  assert.deepEqual(await remote("ws1", "write", { path: "a.txt", content: "hi\n" }), { path: "a.txt", bytes: 3 });
  await assert.rejects(remote("nope", "read", {}), /no tool backend for nope/);
});

test("bash maps to exec and returns stdout then stderr", async () => {
  const out = await runTool("ws1", tool("bash"), { command: "echo x" });
  assert.match(out, /ran echo x/);
  assert.equal(fake.execs.at(-1), "echo x");
});

test("read, write, edit, glob, grep go to the pod by their own names", async () => {
  assert.match(await runTool("ws1", tool("write"), { path: "src/a.ts", content: "const a = 1;\n" }), /src\/a\.ts/);
  assert.match(await runTool("ws1", tool("read"), { path: "src/a.ts" }), /const a = 1/);
  assert.match(await runTool("ws1", tool("edit"), { path: "src/a.ts", old_string: "1", new_string: "2" }), /-.*1[\s\S]*\+.*2/);
  assert.equal(fake.files.get("src/a.ts"), "const a = 2;\n");
  assert.match(await runTool("ws1", tool("glob"), { folder: "all folders", pattern: "**" }), /src\/a\.ts/);
  assert.match(await runTool("ws1", tool("grep"), { pattern: "const", path: "all files" }), /src\/a\.ts:1:/);
});

test("a missing file is a tool result the model can read", async () => {
  const out = await runTool("ws1", tool("read"), { path: "none.txt" });
  assert.match(out, /no such file/);
});

test("bash background sends detach and returns the process id", async () => {
  const out = await runTool("ws1", tool("bash"), { command: "echo bg", background: "true" });
  assert.match(out, /started in background as p1/);
});

test("a stale token 401s once, the resolver re-mints a fresh one, and the call succeeds", async () => {
  const tokens = { stale: "tok-old", fresh: "tok-new" };
  let current = tokens.stale;
  const seen: (string | undefined)[] = [];
  const srv = http.createServer((req, res) => {
    seen.push(req.headers.authorization);
    if (req.headers.authorization !== `Bearer ${tokens.fresh}`) { res.statusCode = 401; return res.end(JSON.stringify({ error: "stale token" })); }
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => res.end(JSON.stringify({ ok: true })));
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  try {
    const address = `127.0.0.1:${(srv.address() as { port: number }).port}`;
    setBackend("ws3", httpBackend(async (fresh) => {
      if (fresh) current = tokens.fresh;
      return { address, token: current === tokens.fresh ? tokens.fresh : tokens.stale };
    }));
    const out = await remote("ws3", "read", {});
    assert.deepEqual(out, { ok: true });
    assert.deepEqual(seen, [`Bearer ${tokens.stale}`, `Bearer ${tokens.fresh}`], "one retry with a re-resolved token, not a loop");
  } finally {
    srv.close();
  }
});

test("a foreground timeout tells the model to rerun in the background", async () => {
  const slow = await fakeTools((cmd) => ({ exit_code: 0, stdout: `ran ${cmd}`, stderr: "", timed_out: true }));
  setBackend("ws2", httpBackend(async () => ({ address: slow.address })));
  setBgWaitMs(1);
  try {
    const out = await runTool("ws2", tool("bash"), { command: "sleep 100" });
    assert.match(out, /rerun with background: true/);
  } finally {
    setBgWaitMs(10000);
    slow.close();
  }
});
