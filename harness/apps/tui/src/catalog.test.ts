import { expect, test } from "bun:test";
import { models } from "@kloudlite-tui/agent";
import { spawnSync } from "bun";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { catalog, modelLabel, refreshCatalog } from "./models.ts";

// pi's bundled JSON is an offline seed, and every provider is dynamic — so a
// never-refreshed catalog is pinned to whatever shipped in dist. This is the
// bug that stranded us on `deepseek-v4-flash`, an id DeepSeek stopped serving.
test("every provider is dynamic, so the seed is never the whole story", () => {
  const providers = models.getProviders();
  expect(providers.length).toBeGreaterThan(0);
  // if pi ever ships a static provider, refresh silently skips it
  const dynamic = providers.filter(
    (p) => typeof (p as { refreshModels?: unknown }).refreshModels === "function",
  );
  expect(dynamic.length).toBe(providers.length);
});

// refresh must return the catalog, not mutate the import-time snapshot:
// `catalog` is a frozen const, which is why the picker needs the result
test("refreshCatalog returns a list without mutating the seed", async () => {
  const seeded = catalog.length;
  const live = await refreshCatalog();
  expect(Array.isArray(live)).toBe(true);
  expect(live.length).toBeGreaterThanOrEqual(seeded);
  expect(catalog.length).toBe(seeded);
});

// A cold start knows only pi's bundled seed, so validating a saved model
// against the catalog at import time threw away any choice the seed predates:
// `deepseek-flash` (the one DeepSeek model that accepts images) became an
// anthropic fallback on first launch. DEFAULT_MODEL is computed at import, and
// bun caches modules for the process — so this needs a real fresh process.
test("a saved default model survives a cold start", () => {
  const dir = mkdtempSync(join(tmpdir(), "kl-cold-"));
  writeFileSync(
    join(dir, "settings.json"),
    JSON.stringify({ defaultModel: { provider: "deepseek", id: "deepseek-flash" } }),
  );
  const out = spawnSync(
    [
      "bun",
      "-e",
      'const m = await import("./src/models.ts"); console.log(m.DEFAULT_MODEL.provider + "/" + m.DEFAULT_MODEL.id);',
    ],
    { cwd: import.meta.dir + "/..", env: { ...process.env, KLOUDLITE_CONFIG_DIR: dir } },
  );
  expect(out.stdout.toString().trim()).toBe("deepseek/deepseek-flash");
});

// the label is what the title bar shows before refresh lands
test("an unrefreshed model still labels as its id", () => {
  expect(modelLabel({ provider: "deepseek", id: "not-a-real-model" })).toBe("not-a-real-model");
});
