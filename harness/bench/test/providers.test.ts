import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { serve } from "../src/server.ts";
import { FAKE } from "./fake-pi.ts";
import { authPath, listProviders, removeProvider, setProvider } from "../src/providers.ts";

/** A temp HOME, so no test ever touches the developer's own auth.json. */
function tmpHome<T>(fn: (file: string) => T): T {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "pi-home-"));
  const prev = process.env.HOME;
  process.env.HOME = home;
  try {
    return fn(authPath());
  } finally {
    process.env.HOME = prev;
    fs.rmSync(home, { recursive: true, force: true });
  }
}

test("a missing auth.json lists everything unconfigured", () => {
  tmpHome((file) => {
    assert.ok(file.endsWith(path.join(".pi", "agent", "auth.json")));
    const all = listProviders();
    assert.ok(all.some((p) => p.id === "deepseek"));
    assert.ok(all.every((p) => !p.configured));
    assert.ok(all.every((p) => !("key" in p)), "a listing never carries the key");
  });
});

test("PUT writes 0600, merges, marks configured; DELETE removes only that entry", () => {
  tmpHome((file) => {
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, JSON.stringify({ anthropic: { type: "oauth", access: "a", refresh: "r", expires: 1 } }));
    setProvider("deepseek", "sk-secret");
    const data = JSON.parse(fs.readFileSync(file, "utf-8"));
    assert.deepEqual(data.deepseek, { type: "api_key", key: "sk-secret" });
    assert.equal(data.anthropic.type, "oauth", "another provider's login survives");
    assert.equal(fs.statSync(file).mode & 0o777, 0o600);
    assert.equal(listProviders().find((p) => p.id === "deepseek")?.configured, true);

    setProvider("deepseek", "  sk-second  ");
    assert.equal(JSON.parse(fs.readFileSync(file, "utf-8")).deepseek.key, "sk-second");

    removeProvider("deepseek");
    const after = JSON.parse(fs.readFileSync(file, "utf-8"));
    assert.equal(after.deepseek, undefined);
    assert.equal(after.anthropic.type, "oauth");
    removeProvider("deepseek"); // removing what is not there is not an error
  });
});

test("an unknown provider or an empty key is refused, and a broken file is never clobbered", () => {
  tmpHome((file) => {
    assert.throws(() => setProvider("not-a-provider", "k"), /unknown provider/);
    assert.throws(() => setProvider("openai", ""), /apiKey required/);
    assert.throws(() => setProvider("openai", undefined), /apiKey required/);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, "[]");
    assert.throws(() => setProvider("openai", "k"), /not an object/);
    assert.equal(fs.readFileSync(file, "utf-8"), "[]");
  });
});

test("the routes list, write and remove, and refuse an unknown provider", async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "pi-home-"));
  const prev = process.env.HOME;
  process.env.HOME = home;
  const bench = new Bench({ dir: fs.mkdtempSync(path.join(os.tmpdir(), "bench-prov-")), readOnly: false, model: "fake/m", bin: FAKE });
  await bench.start();
  const srv = await serve(bench, 0);
  const base = `http://127.0.0.1:${srv.port}`;
  try {
    const before = (await (await fetch(base + "/providers")).json()) as { id: string; configured: boolean }[];
    assert.equal(before.find((p) => p.id === "deepseek")?.configured, false);

    const put = await fetch(`${base}/providers/deepseek`, { method: "PUT", body: JSON.stringify({ apiKey: "sk-route" }) });
    assert.equal(put.status, 204);
    const after = (await (await fetch(base + "/providers")).json()) as { id: string; configured: boolean }[];
    assert.equal(after.find((p) => p.id === "deepseek")?.configured, true);
    assert.equal(JSON.stringify(after).includes("sk-route"), false, "a listing never carries the key");

    const bad = await fetch(`${base}/providers/nope`, { method: "PUT", body: JSON.stringify({ apiKey: "k" }) });
    assert.equal(bad.status, 400);

    assert.equal((await fetch(`${base}/providers/deepseek`, { method: "DELETE" })).status, 204);
    const gone = (await (await fetch(base + "/providers")).json()) as { id: string; configured: boolean }[];
    assert.equal(gone.find((p) => p.id === "deepseek")?.configured, false);
  } finally {
    await bench.stop();
    await srv.close();
    process.env.HOME = prev;
    fs.rmSync(home, { recursive: true, force: true });
  }
});
