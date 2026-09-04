import { expect, test } from "bun:test";
import { KINDS, kindLabel, blockFor } from "./requests";

test("every kind has a label", () => {
  expect(KINDS.map(kindLabel)).toEqual(["More quota", "Team access", "A region", "Something else"]);
});

/** The api refuses a request whose block does not match its kind (422), so the form must send
 *  exactly one — this is the function that decides which. */
test("a form builds exactly the block for its kind", () => {
  const form = new FormData();
  form.set("team", "acme");
  form.set("role", "admin");
  expect(blockFor("access", form)).toEqual({ access: { team: "acme", role: "admin" } });

  const q = new FormData();
  q.set("workspaces", "12");
  q.set("cpu", "");
  expect(blockFor("quota", q)).toEqual({ quota: { workspaces: 12 } });
});

/** An empty required field is a refusal here, not a 422 from the api after a round trip. */
test("an incomplete form is refused before it is sent", () => {
  expect(() => blockFor("region", new FormData())).toThrow("Pick a region.");
  expect(() => blockFor("quota", new FormData())).toThrow("Raise at least one dimension.");
});
