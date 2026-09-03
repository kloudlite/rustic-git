import { describe, expect, test } from "bun:test";
import { packagesField } from "./packages-field";

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
