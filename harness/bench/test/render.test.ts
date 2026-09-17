import { strict as assert } from "node:assert";
import { test } from "node:test";
import { usage } from "../../src/renderer/live.ts";
import { KEYS, LEADER, keyHint, leaderIndex, underLeader } from "../../src/renderer/keys.ts";
import { playDemo, wantsDemo } from "../../src/renderer/demo.ts";

test("the footer says tokens, how full the window is, and what it cost", () => {
  assert.equal(usage(12400, 1.2, 32000), "12.4K (39%) · $1.20");
  assert.equal(usage(900, 0, undefined), "900");
  assert.equal(usage(64000, undefined, 32000), "64K (100%)", "a window over its limit stops at 100");
});

test("opencode's leader is an alias layer over our own keys", () => {
  const ev = (key: string, mods: Partial<KeyboardEvent> = {}) => ({ key, ctrlKey: false, metaKey: false, altKey: false, shiftKey: false, ...mods }) as KeyboardEvent;
  assert.ok(LEADER.match(ev("x", { ctrlKey: true })), "ctrl+x arms the leader");
  assert.ok(!LEADER.match(ev("x", { metaKey: true })), "⌘X is not the leader");
  assert.equal(underLeader(ev("b")), KEYS.panel);
  assert.equal(underLeader(ev("l")), KEYS.quickOpen);
  assert.equal(underLeader(ev("s")), KEYS.inspector);
  assert.equal(underLeader(ev("z")), undefined);
  assert.equal(leaderIndex(ev("3")), 2);
  assert.equal(leaderIndex(ev("0")), undefined);
  // Our own key still works, and the palette shows both.
  assert.ok(KEYS.panel.match(ev("b", { metaKey: true })));
  assert.equal(keyHint(KEYS.panel), "⌘B  ^X B");
  assert.equal(keyHint(KEYS.shell), "⌘J", "a binding with no alias reads as itself");
});

test("the demo turn plays pi's own events, in order, and can be stopped", async () => {
  const seen: Record<string, unknown>[] = [];
  const stop = playDemo("s-1", (ev) => seen.push(ev));
  await new Promise((r) => setTimeout(r, 120));
  assert.equal(seen[0].type, "agent_start");
  assert.equal(seen[0].session, "s-1", "every step is addressed to the thread it plays in");
  stop();
  const after = seen.length;
  await new Promise((r) => setTimeout(r, 300));
  assert.equal(seen.length, after, "a stopped demo sends nothing more");
  assert.ok(wantsDemo("?motion-demo"));
  assert.ok(!wantsDemo("?other=1"));
});
