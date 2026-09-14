import { test } from "node:test";
import assert from "node:assert/strict";
import { screen } from "../../src/renderer/login.ts";

test("signed out offers sign in and the address, with the reason when there is one", () => {
  assert.deepEqual(screen({ phase: "signed-out" }), { title: "Sign in to Kloudlite", actions: ["signIn", "address"], busy: false });
  assert.equal(screen({ phase: "signed-out", reason: "signed out: expired or revoked" }).body, "signed out: expired or revoked");
});

test("waiting shows the code and the URL, reopen the browser or cancel", () => {
  const s = screen({ phase: "waiting", code: "BCDF-GH23", url: "https://k/cli/authorize?code=BCDF-GH23" });
  assert.equal(s.code, "BCDF-GH23");
  assert.equal(s.url, "https://k/cli/authorize?code=BCDF-GH23");
  assert.deepEqual(s.actions, ["openBrowser", "cancel"]);
  assert.equal(s.busy, true);
});

test("connecting is busy with its step and can still sign out", () => {
  assert.deepEqual(screen({ phase: "connecting", step: "bench is waking" }), { title: "Connecting", body: "bench is waking", actions: ["signOut"], busy: true });
});

test("starting is busy with nothing to press", () => {
  assert.deepEqual(screen({ phase: "starting" }), { title: "Kloudlite", actions: [], busy: true });
});

test("errors always offer sign out, and retry only when there is something to retry", () => {
  assert.deepEqual(screen({ phase: "error", message: "can't reach Kloudlite", retry: "launch" }).actions, ["retry", "signOut"]);
  assert.deepEqual(screen({ phase: "error", message: "no keychain", retry: "none" }).actions, ["signOut"]);
});

test("the picker lists teams by name, a team with no region disabled with a note", () => {
  const s = screen({ phase: "choose-team", teams: [{ slug: "kay", name: "Personal", region: "", personal: true }, { slug: "acme", name: "Acme", region: "r1", personal: false }, { slug: "fresh", name: "", region: "", personal: false }] });
  assert.equal(s.title, "Choose a team");
  assert.deepEqual(s.teams, [
    { slug: "kay", label: "Personal", disabled: true, note: "no region yet — ask an admin" },
    { slug: "acme", label: "Acme", disabled: false },
    { slug: "fresh", label: "fresh", disabled: true, note: "no region yet — ask an admin" },
  ]);
  assert.deepEqual(s.actions, ["signOut"]);
  assert.equal(s.busy, false);
});

test("the picker shows why it came back, and says so when there are no teams at all", () => {
  assert.equal(screen({ phase: "choose-team", teams: [], reason: "you are no longer in gone" }).body, "you are no longer in gone");
  assert.match(screen({ phase: "choose-team", teams: [] }).body!, /not in any team/);
});
