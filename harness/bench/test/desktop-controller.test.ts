import { test } from "node:test";
import assert from "node:assert/strict";
import { createAuth, type AuthState, type Deps } from "../../src/auth/controller.ts";
import type { Team } from "../../src/connect/bench.ts";

const cred = { api: "https://k.test", token: "t", expiresAt: "2030", username: "karthik" };
const expired = () => Object.assign(new Error("your login has expired"), { name: "Expired" });
const ACME: Team = { slug: "acme", name: "Acme", region: "r1", personal: false };
const BETA: Team = { slug: "beta", name: "Beta", region: "r2", personal: false };
const FRESH: Team = { slug: "fresh", name: "Fresh", region: "", personal: false };

function harness(over: Partial<Deps> = {}, teams: Team[] = [ACME]) {
  let stored: typeof cred | undefined;
  let chosen: string | undefined;
  const log: string[] = [];
  const states: AuthState[] = [];
  const d: Deps = {
    api: () => "https://k.test",
    store: { load: () => stored, save: (c) => void (stored = c), clear: () => void (stored = undefined) },
    team: { load: () => chosen, save: (s) => void (chosen = s), clear: () => void (chosen = undefined) },
    startLogin: async () => ({ code: "BCDF-GH23", url: "https://k.test/cli/authorize?code=BCDF-GH23", done: Promise.resolve(cred) }),
    openExternal: async (u) => void log.push(`open ${u}`),
    validate: async () => "ok",
    teams: async () => teams,
    connect: async (_c, team) => (log.push(`connect ${team}`), () => void log.push("disconnect")),
    revoke: async () => void log.push("revoke"),
    emit: (s) => void states.push(s),
    ...over,
  };
  return {
    auth: createAuth(d),
    d,
    log,
    states,
    set: (c?: typeof cred) => (stored = c),
    get: () => stored,
    choose: (s?: string) => (chosen = s),
    chosen: () => chosen,
  };
}
const phases = (s: AuthState[]) => s.map((x) => x.phase);

test("launch with nothing stored is the login screen and nothing connects", async () => {
  const h = harness();
  await h.auth.launch();
  assert.deepEqual(h.auth.state(), { phase: "signed-out" });
  assert.deepEqual(h.log, []);
});

test("a single team with a region is chosen on its own, remembered, and connected", async () => {
  const h = harness();
  h.set(cred);
  await h.auth.launch();
  assert.deepEqual(h.auth.state(), { phase: "ready", username: "karthik", team: "acme" });
  assert.deepEqual(h.log, ["connect acme"]);
  assert.equal(h.chosen(), "acme");
});

test("several teams: the picker, nothing connects until one is chosen", async () => {
  const h = harness({}, [ACME, BETA]);
  h.set(cred);
  await h.auth.launch();
  assert.deepEqual(h.auth.state(), { phase: "choose-team", teams: [ACME, BETA] });
  assert.deepEqual(h.log, []);
  await h.auth.chooseTeam("beta");
  assert.deepEqual(h.auth.state(), { phase: "ready", username: "karthik", team: "beta" });
  assert.deepEqual(h.log, ["connect beta"]);
  assert.equal(h.chosen(), "beta");
});

test("a remembered team goes straight to its bench", async () => {
  const h = harness({}, [ACME, BETA]);
  h.set(cred);
  h.choose("beta");
  await h.auth.launch();
  assert.deepEqual(h.log, ["connect beta"]);
  assert.equal(h.auth.state().phase, "ready");
});

test("a remembered team the person is no longer in falls back to the picker with a reason", async () => {
  const h = harness({}, [ACME, BETA]);
  h.set(cred);
  h.choose("gone");
  await h.auth.launch();
  assert.deepEqual(h.auth.state(), { phase: "choose-team", teams: [ACME, BETA], reason: "you are no longer in gone" });
  assert.equal(h.chosen(), undefined);
  assert.deepEqual(h.log, []);
});

test("a remembered team is not auto-replaced even when only one team is left", async () => {
  const h = harness();
  h.set(cred);
  h.choose("gone");
  await h.auth.launch();
  assert.equal(h.auth.state().phase, "choose-team");
  assert.deepEqual(h.log, []);
});

test("a team without a region is never auto-selected nor choosable, and no bench is made", async () => {
  const h = harness({}, [FRESH]);
  h.set(cred);
  await h.auth.launch();
  assert.deepEqual(h.auth.state(), { phase: "choose-team", teams: [FRESH] });
  await h.auth.chooseTeam("fresh");
  assert.equal(h.auth.state().phase, "choose-team");
  await h.auth.chooseTeam("not-on-the-list");
  assert.equal(h.auth.state().phase, "choose-team");
  assert.deepEqual(h.log, []);
  assert.equal(h.chosen(), undefined);
});

test("a remembered team that lost its region says so", async () => {
  const h = harness({}, [FRESH, ACME]);
  h.set(cred);
  h.choose("fresh");
  await h.auth.launch();
  assert.deepEqual(h.auth.state(), { phase: "choose-team", teams: [FRESH, ACME], reason: "fresh has no region yet" });
});

test("choosing another team while ready closes this bench and connects that one", async () => {
  const h = harness({}, [ACME, BETA, FRESH]);
  h.set(cred);
  h.choose("acme");
  await h.auth.launch();
  assert.deepEqual(h.auth.teams(), [ACME, BETA, FRESH]);
  await h.auth.chooseTeam("acme");
  await h.auth.chooseTeam("fresh");
  await h.auth.chooseTeam("gone");
  assert.deepEqual(h.log, ["connect acme"]);
  await h.auth.chooseTeam("beta");
  assert.deepEqual(h.log, ["connect acme", "disconnect", "connect beta"]);
  assert.equal(h.chosen(), "beta");
  assert.deepEqual(h.auth.state(), { phase: "ready", username: "karthik", team: "beta" });
});

test("a 401 listing teams signs out; an unreachable list is a retryable error", async () => {
  const h = harness({ teams: async () => { throw expired(); } });
  h.set(cred);
  await h.auth.launch();
  assert.deepEqual(h.auth.state(), { phase: "signed-out", reason: "signed out: expired or revoked" });
  let down = true;
  const u = harness({ teams: async () => { if (down) throw new Error("Kloudlite answered 503 listing your teams"); return [ACME]; } });
  u.set(cred);
  await u.auth.launch();
  assert.deepEqual(u.auth.state(), { phase: "error", message: "Kloudlite answered 503 listing your teams", retry: "connect" });
  down = false;
  await u.auth.retry();
  assert.equal(u.auth.state().phase, "ready");
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

test("sign in: waiting with the code, browser opened, then stored, team found and connected", async () => {
  const h = harness();
  await h.auth.launch();
  await h.auth.signIn();
  assert.deepEqual(phases(h.states), ["signed-out", "waiting", "connecting", "connecting", "ready"]);
  assert.deepEqual(h.states[1], { phase: "waiting", code: "BCDF-GH23", url: "https://k.test/cli/authorize?code=BCDF-GH23" });
  assert.deepEqual(h.get(), cred);
  assert.deepEqual(h.log, ["open https://k.test/cli/authorize?code=BCDF-GH23", "connect acme"]);
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
  assert.deepEqual(h.log, ["connect acme", "revoke", "disconnect"]);
  assert.equal(h.get(), undefined);
  assert.deepEqual(h.auth.state(), { phase: "signed-out" });
});

test("sign out from the error screen works (nothing connected to drop)", async () => {
  const h = harness({ connect: async () => { throw new Error("bench refused"); } });
  h.set(cred);
  await h.auth.launch();
  assert.equal(h.auth.state().phase, "error");
  await h.auth.signOut();
  assert.deepEqual(h.log, ["revoke"]);
  assert.deepEqual(h.auth.state(), { phase: "signed-out" });
});

test("expired mid-use clears and disconnects without a revoke call", async () => {
  const h = harness();
  h.set(cred);
  await h.auth.launch();
  h.auth.expired();
  assert.deepEqual(h.log, ["connect acme", "disconnect"]);
  assert.equal(h.get(), undefined);
  assert.deepEqual(h.auth.state(), { phase: "signed-out", reason: "signed out: expired or revoked" });
});

test("retry connect with the stored login gone lands on signed-out, not error", async () => {
  const h = harness({ connect: async () => { throw new Error("bench refused"); } });
  h.set(cred);
  await h.auth.launch();
  assert.equal(h.auth.state().phase, "error");
  h.set(undefined);
  await h.auth.retry();
  assert.deepEqual(h.auth.state(), { phase: "signed-out" });
});

test("a sign-out reason stays in the state for a reloaded window", async () => {
  const h = harness();
  h.set(cred);
  await h.auth.launch();
  await h.auth.signOut("keychain went away");
  assert.deepEqual(h.auth.state(), { phase: "signed-out", reason: "keychain went away" });
});

test("expired while connect is in flight closes what connect built and stays signed out", async () => {
  let finish!: () => void;
  const h = harness({ connect: async () => { await new Promise<void>((r) => (finish = r)); log.push("built"); return () => void log.push("closed"); } });
  const log = h.log;
  h.set(cred);
  const p = h.auth.launch();
  await new Promise((r) => setImmediate(r));
  h.auth.expired();
  finish();
  await p;
  assert.deepEqual(log, ["built", "closed"]);
  assert.equal(h.get(), undefined);
  assert.deepEqual(h.auth.state(), { phase: "signed-out", reason: "signed out: expired or revoked" });
});

test("signing out while the team list is loading never lands on the picker", async () => {
  let release!: (t: Team[]) => void;
  const h = harness({ teams: () => new Promise<Team[]>((r) => (release = r)) });
  h.set(cred);
  const p = h.auth.launch();
  await new Promise((r) => setImmediate(r));
  await h.auth.signOut();
  release([ACME, BETA]);
  await p;
  assert.deepEqual(h.auth.state(), { phase: "signed-out" });
});

test("Personal is chosen like a team: its handle is the team the bench is opened in", async () => {
  const ME: Team = { slug: "kay", name: "Personal", region: "r9", personal: true };
  const NONE: Team = { slug: "kay", name: "Personal", region: "", personal: true };
  const h = harness({}, [ME, ACME]);
  h.set(cred);
  await h.auth.launch();
  assert.deepEqual(h.auth.state(), { phase: "choose-team", teams: [ME, ACME] });
  await h.auth.chooseTeam("kay");
  assert.deepEqual(h.auth.state(), { phase: "ready", username: cred.username, team: "kay" });
  const u = harness({}, [NONE, ACME]);
  u.set(cred);
  await u.auth.launch();
  await u.auth.chooseTeam("kay");
  assert.equal(u.auth.state().phase, "choose-team", "a region-less Personal is not choosable");
});
