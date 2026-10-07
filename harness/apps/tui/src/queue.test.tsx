import { expect, test } from "bun:test";
import { testRender } from "@opentui/react/test-utils";
import { Queue } from "./components/Queue.tsx";
import type { QueuedMessage } from "./sessions.ts";

const msgs: QueuedMessage[] = [
  { text: "also check the redis timeouts", kind: "steer" },
  { text: "then summarise for the ticket", kind: "followUp" },
];

async function render(selected: number | null, onSelect?: (i: number) => void) {
  const t = await testRender(
    <Queue messages={msgs} selected={selected} width={78} onSelect={onSelect} />,
    { width: 80, height: 6 },
  );
  await new Promise((r) => setTimeout(r, 40));
  await t.renderOnce();
  return t;
}

test("queue lists what's waiting, with how each is delivered", async () => {
  const t = await render(null);
  const f = t.captureCharFrame();
  expect(f).toContain("QUEUED  2");
  expect(f).toContain("steer also check the redis timeouts");
  expect(f).toContain("after then summarise for the ticket"); // followUp reads "after"
  expect(f).not.toContain("enter edit"); // keys only once a row is targeted
  t.renderer.destroy();
});

test("queue marks the targeted row and offers the edit keys", async () => {
  const t = await render(0);
  const f = t.captureCharFrame();
  expect(f).toContain("› steer also check the redis timeouts");
  expect(f).toContain("enter edit · d drop · esc done");
  t.renderer.destroy();
});

test("queue rows are clickable", async () => {
  const picks: number[] = [];
  const t = await render(null, (i) => picks.push(i));
  const lines = t.captureCharFrame().split("\n");
  const row = lines.findIndex((l) => l.includes("then summarise"));
  await t.mockMouse.click(4, row);
  expect(picks).toEqual([1]);
  t.renderer.destroy();
});

test("an empty queue renders nothing", async () => {
  const t = await testRender(<Queue messages={[]} selected={null} width={78} />, { width: 80, height: 3 });
  await new Promise((r) => setTimeout(r, 40));
  await t.renderOnce();
  expect(t.captureCharFrame()).not.toContain("QUEUED");
  t.renderer.destroy();
});
