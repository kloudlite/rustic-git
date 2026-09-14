import { test } from "node:test";
import assert from "node:assert/strict";
import { createAuth, type AuthState, type Deps } from "../../src/auth/controller.ts";

const cred = { api: "https://k.test", token: "t", expiresAt: "2030", username: "karthik" };
const expired = () => Object.assign(new Error("your login has expired"), { name: "Expired" });

function harness(over: Partial<Deps> = {}) {
  let stored: typeof cred | undefined = over.store ? undefined : undefined;
  const log: string[] = [];
  const states: AuthState[] = [];
  const d: Deps = {
    api: () => "https://k.test",
    store: { load: () => stored, save: (c) => void (stored = c), clear: () => void (stored = undefined) },
    startLogin: async () => ({ code: "BCDF-GH23", url: "https://k.test/cli/authorize?code=BCDF-GH23", done: Promise.resolve(cred) }),
    openExternal: async (u) => void log.push(`open ${u}`),
    validate: async () => "ok",
    connect: async () => (log.push("connect"), () => void log.push("disconnect")),
    revoke: async () => void log.push("revoke"),
    emit: (s) => void states.push(s),
    ...over,
  };
  return { auth: createAuth(d), d, log, states, set: (c?: typeof cred) => (stored = c), get: () => stored };
}
const phases = (s: AuthState[]) => s.map((x) => x.phase);

test("launch with nothing stored is the login screen and nothing connects", async () => {
  const h = harness();
  await h.auth.launch();
  assert.deepEqual(h.auth.state(), { phase: "signed-out" });
  assert.deepEqual(h.log, []);
});

test("launch with a valid credential connects to ready", async () => {
  const h = harness();
  h.set(cred);
  await h.auth.launch();
  assert.deepEqual(h.auth.state(), { phase: "ready", username: "karthik" });
  assert.deepEqual(h.log, ["connect"]);
});

test("launch with a revoked credential clears it and says why", async () => {
  const h = harness({ validate: async () => "expired" });
  h.set(cred);
  await h.auth.launch();
  assert.equal(h.get(), undefined);
  assert.deepEqual(h.auth.state(), { phase: "signed-out", reason: "signed out: expired or revoked" });
});

test("launch while Kloudlite is unreachable keeps the credential and offers retry", async () => {
  let up = false;
  const h = harness({ validate: async () => { if (!up) throw new Error("can't reach Kloudlite"); return "ok"; } });
  h.set(cred);
  await h.auth.launch();
  assert.deepEqual(h.auth.state(), { phase: "error", message: "can't reach Kloudlite", retry: "launch" });
  assert.deepEqual(h.get(), cred);
  up = true;
  await h.auth.retry();
  assert.equal(h.auth.state().phase, "ready");
});

test("no keychain is an error, never a login", async () => {
  const h = harness({ store: { load: () => { throw Object.assign(new Error("no keychain"), { name: "NoKeychain" }); }, save: () => undefined, clear: () => undefined } });
  await h.auth.launch();
  assert.deepEqual(h.auth.state(), { phase: "error", message: "no keychain", retry: "none" });
});

test("sign in: waiting with the code, browser opened, then stored and connected", async () => {
  const h = harness();
  await h.auth.launch();
  await h.auth.signIn();
  assert.deepEqual(phases(h.states), ["signed-out", "waiting", "connecting", "ready"]);
  assert.deepEqual(h.states[1], { phase: "waiting", code: "BCDF-GH23", url: "https://k.test/cli/authorize?code=BCDF-GH23" });
  assert.deepEqual(h.get(), cred);
  assert.deepEqual(h.log, ["open https://k.test/cli/authorize?code=BCDF-GH23", "connect"]);
});

test("a denied or expired code goes back to the login screen with the reason", async () => {
  const h = harness({ startLogin: async () => ({ code: "BCDF-GH23", url: "u", done: Promise.reject(Object.assign(new Error("that login expired or was denied"), { name: "LoginFailed" })) }) });
  await h.auth.signIn();
  assert.deepEqual(h.auth.state(), { phase: "signed-out", reason: "that login expired or was denied" });
  assert.equal(h.get(), undefined);
});

test("cancel while waiting returns to signed-out without a reason", async () => {
  let abort: AbortSignal | undefined;
  const h = harness({
    startLogin: async (_api, signal) => {
      abort = signal;
      return { code: "BCDF-GH23", url: "u", done: new Promise((_r, j) => signal.addEventListener("abort", () => j(Object.assign(new Error("aborted"), { name: "AbortError" })))) };
    },
  });
  const p = h.auth.signIn();
  await new Promise((r) => setImmediate(r));
  h.auth.cancel();
  await p;
  assert.ok(abort?.aborted);
  assert.deepEqual(h.auth.state(), { phase: "signed-out" });
});

test("a bench refusal while connecting is shown, the login is kept, retry reconnects", async () => {
  let refuse = true;
  const h = harness({ connect: async () => { if (refuse) throw new Error("cpu: 40 of 40 in use; request more under Quota"); return () => undefined; } });
  h.set(cred);
  await h.auth.launch();
  assert.deepEqual(h.auth.state(), { phase: "error", message: "cpu: 40 of 40 in use; request more under Quota", retry: "connect" });
  assert.deepEqual(h.get(), cred);
  refuse = false;
  await h.auth.retry();
  assert.equal(h.auth.state().phase, "ready");
});

test("a 401 while connecting signs out", async () => {
  const h = harness({ connect: async () => { throw expired(); } });
  h.set(cred);
  await h.auth.launch();
  assert.equal(h.get(), undefined);
  assert.deepEqual(h.auth.state(), { phase: "signed-out", reason: "signed out: expired or revoked" });
});

test("sign out revokes, clears, disconnects", async () => {
  const h = harness();
  h.set(cred);
  await h.auth.launch();
  await h.auth.signOut();
  assert.deepEqual(h.log, ["connect", "revoke", "disconnect"]);
  assert.equal(h.get(), undefined);
  assert.deepEqual(h.auth.state(), { phase: "signed-out" });
});

test("expired mid-use clears and disconnects without a revoke call", async () => {
  const h = harness();
  h.set(cred);
  await h.auth.launch();
  h.auth.expired();
  assert.deepEqual(h.log, ["connect", "disconnect"]);
  assert.equal(h.get(), undefined);
  assert.deepEqual(h.auth.state(), { phase: "signed-out", reason: "signed out: expired or revoked" });
});
