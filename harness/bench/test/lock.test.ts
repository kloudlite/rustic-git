import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { takeLock, FolderLocked } from "../src/lock.ts";
import { until } from "./wait.ts";

test("a second instance on the same folder refuses and names the holder", async () => {
  const d = fs.mkdtempSync(path.join(os.tmpdir(), "bench-lock-"));
  const a = await takeLock(d);
  await assert.rejects(takeLock(d), (e: unknown) => e instanceof FolderLocked && e.holder.includes(`pid ${process.pid}`));
  a.release();
  // release() closes flock's stdin; the lock goes when that child exits, a moment later.
  let b: Awaited<ReturnType<typeof takeLock>> | undefined;
  await until(async () => (b = await takeLock(d).catch(() => undefined)), 5_000, "the released lock to be free");
  b!.release();
});

test("wait mode takes the lock once the holder lets go", async () => {
  const d = fs.mkdtempSync(path.join(os.tmpdir(), "bench-lock-"));
  const a = await takeLock(d);
  let waited = "";
  const pending = takeLock(d, { wait: true, onWaiting: (h) => (waited = h) });
  await until(() => waited, 5_000, "the waiter to name the holder");
  a.release();
  const b = await pending;
  assert.match(waited, /pid \d+/);
  b.release();
});
