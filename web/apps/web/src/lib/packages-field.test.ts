import { describe, expect, test } from "bun:test";
import { packageListError, packagesField } from "./packages-field";

const fd = (v?: string) => {
  const f = new FormData();
  if (v !== undefined) f.set("packages", v);
  return f;
};

describe("packagesField", () => {
  test("an absent field is undefined — the snapshot's own list stands", () => {
    expect(packagesField(fd())).toBeUndefined();
  });

  test("a present but empty field is an EMPTY list, not undefined", () => {
    // The snapshot froze `packages: []`, the input rendered blank, and the person accepted it.
    // Sending nothing would silently restore the snapshot's list instead.
    expect(packagesField(fd(""))).toEqual([]);
  });

  test("a list is split, trimmed, and blanks dropped", () => {
    expect(packagesField(fd(" ripgrep ,, fd,"))).toEqual(["ripgrep", "fd"]);
  });
});

// Mirrors `crates/workspaces/src/packages.rs`'s own tests: the two grammars have to agree, or
// the field refuses what the api would accept (or worse, the other way round).
describe("packageListError", () => {
  test("a bare attr, and every shape of pin, pass", () => {
    for (const ok of ["jq", "nodejs@latest", "nodejs@20", "python3@3.11.4", "python3Packages.requests", "gcc-wrapper", "libc++", "nodejs_20"]) {
      expect(packageListError([ok])).toBeNull();
    }
  });

  test("a bad version names the entry", () => {
    for (const bad of ["nodejs@", "nodejs@^20", "nodejs@20.x", "nodejs@20-rc1", "nodejs@>=20", "nodejs@1.2.3.4", "a@b@c"]) {
      expect(packageListError([bad])).toBe(`"${bad}" is not a version: use latest, N, N.N or N.N.N`);
    }
  });

  test("a bad attr is refused before anything is sent", () => {
    for (const bad of ["$(id)", "a b", "-lead", "@20", "x".repeat(65)]) {
      expect(packageListError([bad])).toBe(`"${bad}" is not a package attribute name`);
    }
  });

  test("duplicates are keyed on the attr, not the entry", () => {
    expect(packageListError(["nodejs", "nodejs@20"])).toBe(`"nodejs" is listed twice`);
    expect(packageListError(["nodejs@20", "jq"])).toBeNull();
  });

  test("the list has a ceiling", () => {
    expect(packageListError(Array.from({ length: 101 }, (_, i) => `p${i}`))).toBe("101 packages; the limit is 100");
  });
});
