import { expect, test } from "bun:test";
import { LocalBackend, GATED } from "./local.ts";
import { PROTOCOL } from "./wire.ts";

test("hello carries what the TUI reads at boot", async () => {
  const h = await new LocalBackend().hello();
  expect(h.protocol).toBe(PROTOCOL);
  expect(h.cwd).toBe(process.cwd());
  expect(h.tools).toEqual(["web_fetch", "web_search"]);
  expect(h.catalog.length).toBeGreaterThan(0);
  expect(h.catalog[0]).toHaveProperty("input");
  expect(h.defaultModel).toHaveProperty("provider");
});

test("settings write lands in the next hello", async () => {
  const b = new LocalBackend();
  await b.settings.write({ theme: "kloudlite-light" });
  expect((await b.hello()).settings.theme).toBe("kloudlite-light");
});

test("fs wraps git.ts", async () => {
  const b = new LocalBackend();
  expect(typeof (await b.fs.isGitRepo(process.cwd()))).toBe("boolean");
  expect(Array.isArray(await b.fs.listDir(process.cwd(), ""))).toBe(true);
});

test("gate set is the four mutating tools", () => {
  expect([...GATED].sort()).toEqual(["bash", "edit", "web_fetch", "write"]);
});
