import { test, after } from "node:test";
import assert from "node:assert/strict";
import { fakeTools } from "./fake-tools.ts";
import { setBackend, httpBackend, remote } from "../src/engine/remote.ts";
import { TOOLS, runTool } from "../src/engine/index.ts";

const fake = await fakeTools((cmd) => ({ exit_code: cmd.includes("false") ? 1 : 0, stdout: `ran ${cmd}`, stderr: "" }));
after(() => fake.close());
setBackend("ws1", httpBackend(fake.address));
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
