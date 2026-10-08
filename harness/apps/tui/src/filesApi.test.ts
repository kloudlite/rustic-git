import { expect, test } from "bun:test";
import { filesApi } from "./filesApi.ts";

const fake = () => {
  const calls: string[] = [];
  const hit = (n: string) => async () => (calls.push(n), [] as never);
  const fs: any = { isGitRepo: hit("fs"), changes: hit("fs"), fileDiff: hit("fs"), fullFile: hit("fs"), listDir: hit("fs"), grep: hit("fs") };
  const podfs: any = { isGitRepo: hit("pod"), changes: hit("pod"), fileDiff: hit("pod"), fullFile: hit("pod"), listDir: hit("pod") };
  return { calls, be: { fs, podfs } };
};

test("a workspace view never calls the bench-local reader", async () => {
  const { calls, be } = fake();
  const api = filesApi("ws1", be);
  await api.isGitRepo("/home/kl");
  await api.changes("/home/kl");
  await api.listDir("/home/kl", ".");
  await api.fullFile("/home/kl", "a");
  await api.fileDiff("/home/kl", "a", "M");
  await expect(api.grep("/home/kl", "x")).rejects.toThrow("not available for workspaces");
  expect(calls.every((c) => c === "pod")).toBe(true);
  expect(calls.length).toBe(5);
});

test("no workspace reads the bench", async () => {
  const { calls, be } = fake();
  await filesApi(undefined, be).changes("/r");
  expect(calls).toEqual(["fs"]);
});
