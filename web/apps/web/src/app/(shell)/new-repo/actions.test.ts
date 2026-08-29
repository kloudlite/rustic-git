import { describe, expect, mock, test } from "bun:test";

// The action runs on the server; everything it needs from Next is stubbed so the
// refusal path can be exercised as a plain function.
mock.module("server-only", () => ({}));
mock.module("next/navigation", () => ({ redirect: () => { throw new Error("redirected"); } }));
mock.module("@/lib/api-token", () => ({ tokenOr: async () => "t" }));
mock.module("@/lib/api", () => ({
  createRepo: async () => ({ ok: false, kind: "conflict", message: "" }),
}));
const { create } = await import("./actions");

const form = (fields: Record<string, string>) => {
  const fd = new FormData();
  for (const [k, v] of Object.entries(fields)) fd.set(k, v);
  return fd;
};

describe("create", () => {
  test("a refused submission hands every field back", async () => {
    const fields = { owner: "alice", name: "web", description: "the site", visibility: "public" };
    expect(await create(null, form(fields))).toEqual({ error: "alice/web already exists.", values: fields });
  });

  test("a field the form refuses itself comes back too", async () => {
    const r = await create(null, form({ owner: "alice", name: "no spaces", description: "kept" }));
    expect(r?.error).toMatch(/letters, digits/);
    expect(r?.values).toEqual({ owner: "alice", name: "no spaces", description: "kept", visibility: "private" });
  });
});
