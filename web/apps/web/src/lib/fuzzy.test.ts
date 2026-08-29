import { describe, expect, test } from "bun:test";
import { fuzzy } from "./fuzzy";

describe("fuzzy", () => {
  test("is a subsequence match", () => {
    expect(fuzzy("srau", "src/auth.rs")).not.toBeNull();
    expect(fuzzy("zzz", "src/auth.rs")).toBeNull();
    expect(fuzzy("", "anything")?.hits).toEqual([]);
  });

  test("prefers word starts, runs and the file name over a scattered match", () => {
    const tight = fuzzy("auth", "src/auth.rs")!.score;
    const scattered = fuzzy("auth", "a/u/t/h/xxxxxxxxxxxxxxxxxxxxxxxx.rs")!.score;
    expect(tight).toBeGreaterThan(scattered);
  });

  test("hits are the indexes the view highlights", () => {
    expect(fuzzy("sa", "src/auth.rs")?.hits).toEqual([0, 4]);
  });
});
