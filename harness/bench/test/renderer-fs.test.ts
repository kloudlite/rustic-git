import { test } from "node:test";
import assert from "node:assert/strict";

/**
 * The Files tab follows the workspace's watch instead of re-reading it: what was read stays cached
 * across tab and session switches, and an event patches that cache in place (owner: "can't we use
 * what VSCode is using to sync fs? it's taking time to load everything every time").
 *
 * `window.harness` is what the renderer reads through; here it counts the reads, so a test can say
 * that a patched tree cost NO read at all.
 */
const reads: string[] = [];
let answer: (path: string) => unknown = () => ({ entries: [] });
(globalThis as { window?: unknown }).window = {
  harness: {
    bench: async (_m: string, path: string) => {
      reads.push(path);
      return answer(path);
    },
    // No watch on the fake window: the stream is main's, and these tests are about the cache.
  },
};

const live = await import("../../src/renderer/live.ts");
const SCOPE = "ws-0123456789abcdef";

/** The tree as it stands, read from the cache — a second read would show up in `reads`. */
async function names(path?: string) {
  return ((await live.fsTree(SCOPE, path))?.entries ?? []).map((e) => e.name);
}

test("a created file is put under its parent without reading the tree again", async () => {
  answer = () => ({ entries: [{ name: "main.ts", kind: "file" }] });
  assert.deepEqual(await names("src"), ["main.ts"]);
  const first = reads.length;

  assert.equal(live.applyWatch(SCOPE, { path: "src/new.ts", kind: "create" }), "patched");
  assert.deepEqual(await names("src"), ["main.ts", "new.ts"]);
  assert.equal(reads.filter((r) => r.startsWith("/fs/tree")).length, first, "the tree is patched, never re-read");
});

test("a deleted file is taken out of it", async () => {
  assert.equal(live.applyWatch(SCOPE, { path: "src/main.ts", kind: "remove" }), "patched");
  assert.deepEqual(await names("src"), ["new.ts"]);
});

test("a dropped stream is one full read, not a patch", async () => {
  const before = reads.length;
  assert.equal(live.applyWatch(SCOPE, { resync: true }), "resync");
  live.refetchFs(SCOPE);
  answer = () => ({ entries: [{ name: "again.ts", kind: "file" }] });
  assert.deepEqual(await names("src"), ["again.ts"], "everything held is dropped, so the next draw reads");
  assert.equal(reads.length, before + 1, "and reads exactly once");
});
