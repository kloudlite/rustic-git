import { expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { scratchTools } from "./scratch.ts";

const setup = () => {
  const base = mkdtempSync(join(tmpdir(), "kl-scr-"));
  const root = join(base, "root");
  const outside = join(base, "outside");
  mkdirSync(outside);
  const t = Object.fromEntries(scratchTools(root).map((d) => [d.name, d]));
  return { root, outside, base, t };
};
const OUT = "error: path outside the scratch folder";

test("confine refuses ../, absolute outside, and symlinks out, for read and write", async () => {
  const { root, outside, t } = setup();
  writeFileSync(join(outside, "secret"), "s");
  symlinkSync(outside, join(root, "link"));
  symlinkSync(join(outside, "secret"), join(root, "flink"));
  symlinkSync(join(outside, "nope"), join(root, "dangling"));
  for (const p of ["../x", join(outside, "secret"), "link/secret", "link/new", "flink", "dangling"]) {
    expect(await t.read!.run({ path: p })).toStartWith(OUT);
    expect(await t.write!.run({ path: p, content: "x" })).toStartWith(OUT);
  }
  expect(existsSync(join(outside, "new"))).toBe(false);
  expect(existsSync(join(outside, "nope"))).toBe(false);
});

test("write creates nested dirs, read returns it, missing is an error", async () => {
  const { root, t } = setup();
  expect(await t.write!.run({ path: "a/b/c.txt", content: "l1\nl2\nl3" })).toBe("wrote 8 bytes to a/b/c.txt");
  expect(readFileSync(join(root, "a/b/c.txt"), "utf8")).toBe("l1\nl2\nl3");
  expect(await t.read!.run({ path: join(root, "a/b/c.txt") })).toBe("l1\nl2\nl3");
  expect(await t.read!.run({ path: "a/b/c.txt", offset: 2, limit: 1 })).toBe("l2");
  expect(await t.read!.run({ path: "gone" })).toBe("error: no such file: gone");
});

test("read caps at 256 KiB", async () => {
  const { t } = setup();
  await t.write!.run({ path: "big", content: "x".repeat(300 * 1024) });
  const out = (await t.read!.run({ path: "big" })) as string;
  expect(out).toContain("[truncated");
  expect(out.length).toBeLessThan(257 * 1024);
});

const sandbox = process.platform === "linux" && spawnSync("unshare", ["-Ur", "true"]).status === 0;

test.skipIf(sandbox)("bash without a sandbox says so and does not run the command", async () => {
  const { root, t } = setup();
  const r = JSON.parse((await t.bash!.run({ command: `touch ${join(root, "marker")}` })) as string);
  expect(r.exit_code).toBe(1);
  expect(r.output).toStartWith("error: bash sandbox unavailable");
  expect(existsSync(join(root, "marker"))).toBe(false);
});

test.skipIf(!sandbox)("bash runs confined, env-clean, propagates exit and times out", async () => {
  const { root, t } = setup();
  process.env.KL_API_URL = "http://secret";
  const run = async (a: any) => JSON.parse((await t.bash!.run(a)) as string);
  const w = await run({ command: "echo hi > f && cat f" });
  expect(w).toEqual({ output: "hi\n", exit_code: 0 });
  expect(readFileSync(join(root, "f"), "utf8")).toBe("hi\n");
  expect((await run({ command: "env" })).output).not.toContain("KL_");
  expect((await run({ command: "exit 3" })).exit_code).toBe(3);
  const to = await run({ command: "sleep 30", timeout: 1 });
  expect(to.exit_code).toBe(124);
  delete process.env.KL_API_URL;
});
