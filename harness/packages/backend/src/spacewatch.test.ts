import { test, expect } from "bun:test";
import { SpaceWatch } from "./spacewatch";

const view = (n: number) => ({ available: true, user: "u", workspaces: [{ id: `w${n}` }], environments: [] }) as any;

test("emits on change only; a failing poll emits the last good view with error", async () => {
  let next: () => Promise<any> = async () => view(1);
  const seen: any[] = [];
  const w = new SpaceWatch(() => next(), (v) => seen.push(v), 1_000_000);
  w.poke(); await Bun.sleep(5);
  w.poke(); await Bun.sleep(5);
  expect(seen.length).toBe(1);
  next = async () => { throw new Error("pod read timed out"); };
  w.poke(); await Bun.sleep(5);
  expect(seen.length).toBe(2);
  expect(seen[1].workspaces).toEqual([{ id: "w1" }]);
  expect(seen[1].error).toContain("pod read timed out");
  next = async () => view(1);
  w.poke(); await Bun.sleep(5);
  expect(seen.length).toBe(3);
  expect(seen[2].error).toBeUndefined();
  w.stop();
});

test("never stacks polls", async () => {
  let calls = 0;
  let release!: () => void;
  const w = new SpaceWatch(() => (calls++, new Promise((r) => (release = () => r(view(1))))), () => {}, 1_000_000);
  w.poke(); w.poke(); w.poke();
  expect(calls).toBe(1);
  release(); await Bun.sleep(5);
  w.stop();
});
