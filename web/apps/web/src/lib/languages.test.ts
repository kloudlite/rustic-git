import { describe, expect, test } from "bun:test";
import { breakdown, languageOf } from "./languages";

describe("languageOf", () => {
  test("reads the extension, case-insensitively", () => {
    expect(languageOf("main.rs")?.name).toBe("Rust");
    expect(languageOf("App.TSX")?.name).toBe(languageOf("app.tsx")?.name);
  });

  test("dotfiles, lockfiles and unknown extensions count for nothing", () => {
    expect(languageOf(".gitignore")).toBeUndefined();
    expect(languageOf("README")).toBeUndefined();
    expect(languageOf("file.unknownext")).toBeUndefined();
    expect(languageOf("Dockerfile")?.name).toBe("Dockerfile");
  });
});

describe("breakdown", () => {
  test("shares by bytes, largest first, slivers folded into Other", () => {
    const out = breakdown([
      { name: "a.rs", size: 900 },
      { name: "b.ts", size: 95 },
      { name: "c.md", size: 5 },
      { name: "d.txt", size: 1000 },
      { name: "dir", size: null },
    ]);
    expect(out.map((l) => l.name)).toEqual(["Rust", "TypeScript", "Other"]);
    expect(out.map((l) => l.pct)).toEqual([90, 9.5, 0.5]);
  });

  test("nothing recognisable is an empty list, not NaN", () => {
    expect(breakdown([{ name: "x", size: 10 }])).toEqual([]);
    expect(breakdown([])).toEqual([]);
  });
});
