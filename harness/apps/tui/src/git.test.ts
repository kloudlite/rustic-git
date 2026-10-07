import { expect, test } from "bun:test";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fullFile } from "./git.ts";

function repo(): string {
  const dir = mkdtempSync(join(tmpdir(), "kl-git-"));
  const git = (...args: string[]) => Bun.spawnSync(["git", ...args], { cwd: dir });
  git("init", "-q");
  git("config", "user.email", "t@t.t");
  git("config", "user.name", "t");
  writeFileSync(join(dir, "a.txt"), "one\ntwo\nthree\nfour\n");
  git("add", "-A");
  git("commit", "-qm", "first");
  return dir;
}

test("full file marks added lines and where lines were deleted", () => {
  const dir = repo();
  // drop "two", change nothing else, add a line at the end
  writeFileSync(join(dir, "a.txt"), "one\nthree\nfour\nfive\n");
  const lines = fullFile(dir, "a.txt", "M");

  const gap = lines.find((l) => l.mark === "deleted-gap");
  expect(gap?.count).toBe(1); // one line removed…
  expect(lines.indexOf(gap!)).toBe(1); // …right after "one"

  const added = lines.filter((l) => l.mark === "added").map((l) => l.text);
  expect(added).toEqual(["five"]);
  // the file itself is intact and readable
  expect(lines.filter((l) => l.mark !== "deleted-gap").map((l) => l.text)).toEqual([
    "one",
    "three",
    "four",
    "five",
    "",
  ]);
});

test("added lines land on the right numbers and deletions inside a change", () => {
  const dir = repo();
  // replace "three" with two lines: one removal, two additions at that spot
  writeFileSync(join(dir, "a.txt"), "one\ntwo\nthree-a\nthree-b\nfour\n");
  const lines = fullFile(dir, "a.txt", "M");
  expect(lines.filter((l) => l.mark === "added").map((l) => l.no)).toEqual([3, 4]);
  expect(lines.find((l) => l.mark === "deleted-gap")?.count).toBe(1);
});
