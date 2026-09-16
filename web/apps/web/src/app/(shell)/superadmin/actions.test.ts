import { expect, mock, test } from "bun:test";

mock.module("server-only", () => ({}));
mock.module("next/cache", () => ({ revalidatePath: () => {} }));
mock.module("@/lib/api-token", () => ({ tokenOr: async () => "t" }));
const calls: string[] = [];
mock.module("@/lib/api", () => ({
  deleteRemovalNow: async (_t: string, team: string, owner: string) => (calls.push(`delete ${team} ${owner}`), { ok: true, value: { deletes_enabled: true } }),
}));
const { deleteRemovalNowAction } = await import("./actions");

// Mirrors the team page's own refusal test (`[owner]/(org)/settings/actions.test.ts`): the typed
// handle is checked server-side too, not only in the client component.
test("delete now is refused unless the handle is typed", async () => {
  expect(await deleteRemovalNowAction("acme", "ana", "an")).toEqual({ ok: false, message: "Type their handle to confirm." });
  expect(await deleteRemovalNowAction("", "ana", "ana")).toEqual({ ok: false, message: "Type their handle to confirm." });
  expect(calls).toEqual([]);
  expect(await deleteRemovalNowAction("acme", "ana", "ana")).toEqual({ ok: true, notice: "Their data goes within about 7 minutes." });
  expect(calls).toEqual(["delete acme ana"]);
});
