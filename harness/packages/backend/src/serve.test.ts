import { expect, test } from "bun:test";
import { join } from "node:path";

const SERVE = join(import.meta.dir, "serve.ts");

test("serve answers hello over stdio and keeps console.log off stdout", async () => {
  const p = Bun.spawn(["bun", "run", "--silent", SERVE], {
    stdin: "pipe",
    stdout: "pipe",
    stderr: "pipe",
    env: { ...process.env, KL_SERVE_TEST_NOISE: "1" },
  });
  p.stdin.write('{"id":1,"op":"hello","args":null}\n');
  p.stdin.end(); // EOF: serve disposes and exits after replying
  const out = await new Response(p.stdout).text();
  const err = await new Response(p.stderr).text();
  await p.exited;
  const lines = out.trim().split("\n");
  expect(lines.length).toBe(1);
  const reply = JSON.parse(lines[0]!);
  expect(reply.re).toBe(1);
  expect(reply.value.protocol).toBe(1);
  expect(err).toContain("noise");
});
