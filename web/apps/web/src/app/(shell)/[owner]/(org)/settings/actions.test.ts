import { describe, expect, mock, test } from "bun:test";

mock.module("server-only", () => ({}));
mock.module("next/cache", () => ({ revalidatePath: () => {} }));
mock.module("next/navigation", () => ({ redirect: () => { throw new Error("redirected"); } }));
mock.module("@/lib/api-token", () => ({ tokenOr: async () => "t" }));
mock.module("@/lib/mail", () => ({ sendInvite: async () => ({ sent: true }) }));
mock.module("@/lib/session", () => ({ getSession: async () => null }));
const calls: string[] = [];
let deletesEnabled = true;
mock.module("@/lib/api", () => ({
  pauseTeamMember: async (_t: string, slug: string, email: string) => (calls.push(`pause ${slug} ${email}`), { ok: true }),
  unpauseTeamMember: async (_t: string, slug: string, email: string) => (calls.push(`unpause ${slug} ${email}`), { ok: true }),
  deleteRemovalNow: async (_t: string, slug: string, owner: string) => (calls.push(`delete ${slug} ${owner}`), { ok: true, value: { deletes_enabled: deletesEnabled } }),
}));
const { pauseMember, unpauseMember, deleteRemovalNow } = await import("./actions");

const form = (fields: Record<string, string>) => {
  const fd = new FormData();
  for (const [k, v] of Object.entries(fields)) fd.set(k, v);
  return fd;
};

describe("member actions", () => {
  test("pause and unpause reach the member", async () => {
    await pauseMember(null, form({ slug: "acme", email: "a@x.io" }));
    await unpauseMember(null, form({ slug: "acme", email: "a@x.io" }));
    expect(calls.splice(0)).toEqual(["pause acme a@x.io", "unpause acme a@x.io"]);
  });

  test("delete now is refused unless the handle is typed", async () => {
    expect(await deleteRemovalNow(null, form({ slug: "acme", owner: "ana", confirm: "an" }))).toEqual({ error: "Type their handle to confirm." });
    expect(calls).toEqual([]);
    expect(await deleteRemovalNow(null, form({ slug: "acme", owner: "ana", confirm: "ana" }))).toEqual({ ok: true, notice: "Their data goes within about 5 minutes." });
    expect(calls.splice(0)).toEqual(["delete acme ana"]);
  });

  test("delete now with deletion switched off says it is only marked", async () => {
    deletesEnabled = false;
    const r = await deleteRemovalNow(null, form({ slug: "acme", owner: "ana", confirm: "ana" }));
    expect(r).toEqual({ ok: true, notice: "Marked. Their data will be deleted once deletion is switched on for this platform." });
    calls.splice(0);
  });
});
