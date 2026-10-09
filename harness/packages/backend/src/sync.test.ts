import { test, expect } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { LocalBackend } from "./local";
import { pair } from "./pair";

async function models() {
  process.env.KLOUDLITE_CONFIG_DIR = mkdtempSync(join(tmpdir(), "kl-cfg-"));
  delete process.env.KL_API_URL;
  const { models } = await import("@kloudlite-tui/agent");
  return models.getModels().filter((x: any) => x.provider !== "anthropic") as any[];
}

// No provider key is needed to switch models: the daemon only records what its agent accepted.
const fake = (async () => ({
  messages: [],
  agent: {},
  subscribe: () => () => {},
  setModel: async () => {},
  setThinkingLevel() {},
  setAutoCompactionEnabled() {},
  dispose() {},
})) as any;

test("A's setModel reaches B as session_state; B's open does not change A's model", async () => {
  const ms = await models();
  const [m0, m1] = [ms[0], ms.find((x) => x.id !== ms[0].id) ?? ms[0]];
  const local = new LocalBackend({ create: fake });
  const a = await pair(local), b = await pair(local);
  const ha = await a.session("main", { initial: { model: { provider: m0.provider, id: m0.id } }, fresh: true, tools: [] });
  const hb = await b.session("main", { initial: { model: { provider: m0.provider, id: m0.id } }, tools: [] });
  const seen: any[] = [];
  hb.subscribe((e) => e.type === "session_state" && seen.push(e));
  await ha.setModel({ provider: m1.provider, id: m1.id });
  await Bun.sleep(10);
  expect(seen.at(-1)?.model.id).toBe(m1.id);
  // a third open carrying the old model as `initial` must not move it back
  const hc = await (await pair(local)).session("main", { initial: { model: { provider: m0.provider, id: m0.id } }, tools: [] });
  expect(hc.state.model.id).toBe(m1.id);
  await Promise.all([ha.dispose(), hb.dispose(), hc.dispose()]);
}, 20000);

test("open returns the session state", async () => {
  const [m] = await models();
  const local = new LocalBackend({ create: fake });
  const h = await (await pair(local)).session("w1", { initial: { model: { provider: m.provider, id: m.id }, codemode: false, autoCompact: true, thinkingLevel: "low" }, fresh: true, tools: [] });
  expect(h.state).toMatchObject({ type: "session_state", codemode: false, autoCompact: true, thinkingLevel: "low", tokens: 0, queued: { steering: [], followUp: [] } });
  await h.dispose();
}, 20000);
