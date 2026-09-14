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

test("connecting is busy with its step and no actions", () => {
  assert.deepEqual(screen({ phase: "connecting", step: "bench is waking" }), { title: "Connecting", body: "bench is waking", actions: [], busy: true });
});

test("starting is busy with nothing to press", () => {
  assert.deepEqual(screen({ phase: "starting" }), { title: "Kloudlite", actions: [], busy: true });
});

test("errors offer retry only when there is something to retry", () => {
  assert.deepEqual(screen({ phase: "error", message: "can't reach Kloudlite", retry: "launch" }).actions, ["retry"]);
  assert.deepEqual(screen({ phase: "error", message: "no keychain", retry: "none" }).actions, []);
});
