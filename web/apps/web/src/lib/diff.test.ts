import { describe, expect, test } from "bun:test";
import { LARGE_FILE, parseDiff } from "./diff";

const SAMPLE = [
  "--- a/src/a.rs",
  "+++ b/src/a.rs",
  "@@ -10,3 +10,4 @@ fn main() {",
  " let x = 1;",
  "-let y = 2;",
  "+let y = 3;",
  "+let z = 4;",
  " print(x);",
  "--- a/img.png",
  "+++ b/img.png",
  "Binary file not shown",
  "",
].join("\n");

describe("parseDiff", () => {
  test("numbers lines from the hunk header, not from the diff", () => {
    const d = parseDiff(SAMPLE);
    expect(d.files.map((f) => f.path)).toEqual(["src/a.rs", "img.png"]);
    const [a] = d.files;
    expect(a.hunks).toHaveLength(1);
    expect(a.hunks[0].lines.map((l) => [l.kind, l.old, l.new])).toEqual([
      ["ctx", 10, 10],
      ["del", 11, undefined],
      ["add", undefined, 11],
      ["add", undefined, 12],
      ["ctx", 12, 13],
    ]);
    expect([a.additions, a.deletions]).toEqual([2, 1]);
    expect([d.additions, d.deletions]).toEqual([2, 1]);
  });

  test("a binary file is flagged and has no hunks", () => {
    const img = parseDiff(SAMPLE).files[1];
    expect(img.binary).toBe(true);
    expect(img.hunks).toEqual([]);
  });

  test("the api's truncation marker is carried, not rendered as a line", () => {
    const d = parseDiff(`${SAMPLE}\n[diff truncated]\n`);
    expect(d.truncated).toBe(true);
    expect(d.files[1].hunks).toEqual([]);
    expect(parseDiff(SAMPLE).truncated).toBe(false);
  });

  test("lines before the first hunk and empty input are ignored", () => {
    expect(parseDiff("")).toEqual({ files: [], additions: 0, deletions: 0, truncated: false });
    expect(parseDiff("+++ b/x\nstray\n").files[0].hunks).toEqual([]);
  });

  test("the fold threshold is a positive count of lines", () => {
    expect(LARGE_FILE).toBeGreaterThan(0);
  });
});
