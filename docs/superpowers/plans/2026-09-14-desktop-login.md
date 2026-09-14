# Desktop Login and Connect Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The harness desktop app opens to a login screen, signs the person in with the CLI device-code flow, finds and wakes their bench, and connects to it through an in-process tunnel — no `HARNESS_BENCH`, no hand-started `kl-connect bench`.

**Architecture:** Everything that holds the token lives in the Electron main process: a `safeStorage`-encrypted credential file (`auth/store.ts`), the device-code client (`auth/device.ts`), an electron-free auth state machine (`auth/controller.ts`) with its effects injected, the bench session client (`connect/bench.ts`), and a 127.0.0.1 TCP listener that mints one bench-session token per accepted connection and splices it to the gateway WebSocket (`connect/tunnel.ts`). `main.ts` only wires those to IPC; the renderer gets an `AuthState` (never the token) and shows `LoginScreen` until the state is `ready`, and only then mounts `App`.

**Tech Stack:** Electron 39.8.10, TypeScript 5.9 (main compiled to CommonJS by `tsc -p tsconfig.main.json`), Solid 1.9 renderer (Vite), `ws` 8, tests on Node 24's `node --test` running `.ts` directly (type stripping is on by default in Node 24; `harness/bench/test/*.test.ts` is the glob `npm run bench:test` runs).

**Spec:** `docs/superpowers/specs/2026-09-14-desktop-login-design.md` (commit 026731bb), approved with both open-question defaults: API base is a build-time constant with a settings override; the login is an ordinary CLI credential distinguished by device label `<hostname> (desktop)`.

## Global Constraints

- The token exists only in the main process. No IPC reply, event, log line or error message carries it; the renderer receives `AuthState` only.
- `contextIsolation: true` and `nodeIntegration: false` stay on every window.
- `shell.openExternal` is called only with a URL for which `isAuthorizeUrl(api, url)` is true — exactly `{api}/cli/authorize?code=<CODE>`; anything else is refused.
- `safeStorage.isEncryptionAvailable()` must be true to load or save; otherwise the app shows an error and stores nothing. Never plaintext. File mode 0600 in `app.getPath("userData")`.
- The tunnel binds `127.0.0.1` only, on an ephemeral port, and mints a fresh single-use bench-session token (`POST /v1/bench/session`) for every accepted TCP connection; a token is never reused.
- No server changes. Routes used, as they are today: `POST /v1/cli/code` (201 `{code, poll, expiresIn}`), `GET /v1/cli/token?poll=` (202 pending / 200 `{token, expiresAt}` / 410 terminal), `GET /v1/cli/tokens` (cheap authed read; 401 when expired or revoked), `DELETE /v1/cli/tokens/{jti}`, `POST /v1/bench` (create or start), `POST /v1/bench/start` (202), `POST /v1/bench/session` (201 `{id, token, gateway, expires_at}`; 202 `{state}`; 409 `"bench is stopped; start it"`; 404 `"no bench"`; other 409 = quota refusal). Note: the spec says 200 for a session; the server answers **201** (`crates/workspaces/src/api/bench.rs` `bench_session`), so the client accepts any 2xx other than 202.
- `HARNESS_BENCH` stays a developer override: when set, login is still required, then Connect is skipped and `BenchClient` uses that URL.
- The existing offline cache (`bench-cache.json`) is kept, but `BenchClient` is only constructed after login, so nothing cached renders before it; the cache is keyed by username so a second person on the same machine never sees the first person's list.
- Default API base: `https://dev.kloudlite.io` (same constant as `bins/kl-connect/src/config.rs` `DEFAULT_API` and `harness/pi/kloudlite.ts`), overridable from the login screen, stored in `userData/api.txt`.
- Device label: `` `${os.hostname()} (desktop)` ``.
- Tests use the existing runner (`cd harness && npm run bench:test`), stub `node:http` / `ws` servers on 127.0.0.1, no real network. New modules under `harness/src/auth` and `harness/src/connect` import no sibling module at runtime (only `import type`), because the test runner loads them as ESM where an extensionless `./x` import does not resolve while the tsc CommonJS build requires it extensionless.
- Commits: imperative sentence case, no tool attribution.

---

## File Structure

| File | Status | Responsibility |
|---|---|---|
| `harness/src/auth/store.ts` | create | load/save/clear the credential file through an injected `safeStorage` |
| `harness/src/auth/device.ts` | create | `Credential` type, JWT claim reader, authorize-URL builder/guard, device-code login (code + poll + cancel) |
| `harness/src/auth/controller.ts` | create | `AuthState` and the launch / sign-in / cancel / sign-out / expired state machine, effects injected |
| `harness/src/connect/bench.ts` | create | `POST /v1/bench/session` answers, `ensureBench` (create/start/wake), `mintSession`, `Expired` |
| `harness/src/connect/tunnel.ts` | create | 127.0.0.1 listener, one mint + one gateway WebSocket per TCP connection |
| `harness/src/main.ts` | modify | single-instance lock, auth IPC, launch gate, connect wiring, sign-out menu item |
| `harness/src/preload.ts` | modify | `harness.auth.*` surface |
| `harness/src/bench-client.ts` | modify | optional cache key (username) instead of the ephemeral base URL |
| `harness/src/renderer/login.ts` | create | pure `screen(state)` view model for the login screen |
| `harness/src/renderer/components/LoginScreen.tsx` | create | the login / waiting / connecting / error screen |
| `harness/src/renderer/index.tsx` | modify | `Gate`: `LoginScreen` until `ready`, then `App` |
| `harness/src/renderer/App.tsx` | modify | drop the `/login` alias and the `HARNESS_BENCH` hint |
| `harness/src/renderer/components/SettingsPage.tsx` | modify | "Account" page with the username and Sign out |
| `harness/bench/test/desktop-store.test.ts` | create | store tests |
| `harness/bench/test/desktop-device.test.ts` | create | device-code tests against a stub api |
| `harness/bench/test/desktop-controller.test.ts` | create | state machine tests |
| `harness/bench/test/desktop-bench.test.ts` | create | bench session client tests against a stub api |
| `harness/bench/test/desktop-tunnel.test.ts` | create | tunnel tests against a stub api + gateway |
| `harness/bench/test/desktop-login-screen.test.ts` | create | `screen()` tests |

`tsconfig.main.json` needs no change: tsc follows `main.ts`'s imports into `src/auth` and `src/connect`, and `preload.ts`'s type-only import of `auth/controller.ts` is followed the same way for `tsconfig.renderer.json`.

**Decision on the existing `/login` and `/kl-login` (App.tsx:457, :497, :489; live.ts:219):** remove the renderer's `/login` alias; keep `/kl-login` and the `kl-login` message rendering. `/kl-login` is pi's own command (`harness/pi/kloudlite.ts:165-188`) and pi runs on the bench pod: it writes a *bench-side* credential (device label `… (bench)`) that pi's `kl_*` tools use there. The desktop login cannot supply that credential without shipping the desktop token to the bench, which the spec forbids. What goes is `/login`, whose name now reads as "sign this app in" and would instead silently log in the bench.

---

### Task 1: Credential store

**Files:**
- Create: `harness/src/auth/store.ts`
- Create: `harness/src/auth/device.ts` (the `Credential` type only in this task; Task 2 fills the rest)
- Test: `harness/bench/test/desktop-store.test.ts`

**Interfaces:**
- Produces: `type Credential = { api: string; token: string; expiresAt: string; username: string }` (device.ts); `type Crypto = { isEncryptionAvailable(): boolean; encryptString(s: string): Buffer; decryptString(b: Buffer): string }`; `class NoKeychain extends Error`; `createStore(file: string, crypto: Crypto): { load(): Credential | undefined; save(c: Credential): void; clear(): void }`.

- [ ] **Step 1: Write the failing test**

`harness/bench/test/desktop-store.test.ts`:

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createStore, NoKeychain } from "../../src/auth/store.ts";

// Reversible and visibly not plaintext: a file holding the token verbatim fails the test.
const fake = (available = true) => ({
  isEncryptionAvailable: () => available,
  encryptString: (s: string) => Buffer.from(s, "utf8").map((b) => b ^ 0x5a) as Buffer,
  decryptString: (b: Buffer) => Buffer.from(b.map((x) => x ^ 0x5a)).toString("utf8"),
});
const cred = { api: "https://k.test", token: "aaa.bbb.ccc", expiresAt: "2030-01-01T00:00:00Z", username: "karthik" };
const tmp = () => path.join(fs.mkdtempSync(path.join(os.tmpdir(), "desk-store-")), "credential.bin");

test("a saved credential round-trips, is 0600, and is not plaintext on disk", () => {
  const file = tmp();
  const s = createStore(file, fake());
  s.save(cred);
  assert.deepEqual(s.load(), cred);
  assert.equal(fs.statSync(file).mode & 0o777, 0o600);
  assert.ok(!fs.readFileSync(file).toString("latin1").includes(cred.token));
});

test("nothing stored loads as undefined; clear removes the file and is idempotent", () => {
  const file = tmp();
  const s = createStore(file, fake());
  assert.equal(s.load(), undefined);
  s.save(cred);
  s.clear();
  s.clear();
  assert.equal(fs.existsSync(file), false);
  assert.equal(s.load(), undefined);
});

test("no keychain: save and load refuse, and no file is written", () => {
  const file = tmp();
  const s = createStore(file, fake(false));
  assert.throws(() => s.save(cred), NoKeychain);
  assert.throws(() => s.load(), NoKeychain);
  assert.equal(fs.existsSync(file), false);
});

test("an undecryptable or malformed file loads as undefined rather than throwing", () => {
  const file = tmp();
  fs.writeFileSync(file, "garbage");
  assert.equal(createStore(file, fake()).load(), undefined);
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd harness && node --test bench/test/desktop-store.test.ts`
Expected: FAIL with `Cannot find module '.../src/auth/store.ts'`

- [ ] **Step 3: Write minimal implementation**

`harness/src/auth/device.ts` (Task 2 extends this file):

```ts
/** A desktop login: the same CLI credential kl-connect holds, kept apart from it. */
export type Credential = { api: string; token: string; expiresAt: string; username: string };
```

`harness/src/auth/store.ts`:

```ts
import fs from "node:fs";
import type { Credential } from "./device";

/**
 * The desktop login on disk: one file in userData, encrypted by the OS keychain through
 * Electron's safeStorage (injected, so this runs under node --test). No keychain means no
 * login is kept at all — a plaintext fallback would be a bearer token on disk.
 */
export type Crypto = { isEncryptionAvailable(): boolean; encryptString(s: string): Buffer; decryptString(b: Buffer): string };

export class NoKeychain extends Error {
  constructor() {
    super("this computer has no keychain Kloudlite can use, so your login cannot be stored safely");
    this.name = "NoKeychain";
  }
}

export function createStore(file: string, crypto: Crypto) {
  const need = () => {
    if (!crypto.isEncryptionAvailable()) throw new NoKeychain();
  };
  return {
    load(): Credential | undefined {
      need();
      try {
        const c = JSON.parse(crypto.decryptString(fs.readFileSync(file))) as Credential;
        return typeof c.token === "string" && typeof c.api === "string" ? c : undefined;
      } catch {
        return undefined; // missing, or written by another keychain: signed out, not an error
      }
    },
    save(c: Credential): void {
      need();
      const tmp = `${file}.tmp`;
      fs.writeFileSync(tmp, crypto.encryptString(JSON.stringify(c)), { mode: 0o600 });
      fs.renameSync(tmp, file);
    },
    clear(): void {
      fs.rmSync(file, { force: true });
    },
  };
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd harness && node --test bench/test/desktop-store.test.ts`
Expected: PASS, 4 tests

- [ ] **Step 5: Commit**

```bash
git add harness/src/auth/store.ts harness/src/auth/device.ts harness/bench/test/desktop-store.test.ts
git commit -m "Keep the desktop login in the OS keychain"
```

---

### Task 2: Device-code client

**Files:**
- Modify: `harness/src/auth/device.ts`
- Test: `harness/bench/test/desktop-device.test.ts`

**Interfaces:**
- Consumes: `Credential` (Task 1).
- Produces:
  - `claim(token: string, key: string): string | undefined`
  - `authorizeUrl(api: string, code: string): string`
  - `isAuthorizeUrl(api: string, url: string): boolean`
  - `class LoginFailed extends Error` (name `"LoginFailed"`)
  - `startLogin(api: string, device: string, opts: { signal: AbortSignal; pollMs?: number }): Promise<{ code: string; url: string; done: Promise<Credential> }>` — `done` rejects `LoginFailed` on 410 / timeout, and an `AbortError` on cancel.

Polling semantics copied from `bins/kl-connect/src/login.rs`: every 2 s; 202 keeps polling; 200 stores; 410 is terminal; a network error or any other status keeps polling until the deadline (`expiresIn` seconds from the code answer). The spec's "every `poll` s" is a misreading: `poll` is the opaque poll handle, not an interval.

- [ ] **Step 1: Write the failing test**

`harness/bench/test/desktop-device.test.ts`:

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import type { AddressInfo } from "node:net";
import { authorizeUrl, claim, isAuthorizeUrl, LoginFailed, startLogin } from "../../src/auth/device.ts";

const jwt = (claims: object) => `h.${Buffer.from(JSON.stringify(claims)).toString("base64url")}.s`;

/** A stub api: POST /v1/cli/code, then GET /v1/cli/token answers from `polls` in order. */
async function stub(polls: number[], expiresIn = 600) {
  const seen: { device?: string; polls: string[] } = { polls: [] };
  const token = jwt({ username: "karthik", jti: "j1" });
  const srv = http.createServer((req, res) => {
    if (req.method === "POST" && req.url === "/v1/cli/code") {
      let b = "";
      req.on("data", (d) => (b += d));
      req.on("end", () => {
        seen.device = JSON.parse(b).device;
        res.writeHead(201, { "content-type": "application/json" }).end(JSON.stringify({ code: "BCDF-GH23", poll: "p0ll", expiresIn }));
      });
      return;
    }
    if (req.method === "GET" && req.url?.startsWith("/v1/cli/token?")) {
      seen.polls.push(new URL(req.url, "http://x").searchParams.get("poll")!);
      const st = polls.shift() ?? 202;
      if (st === 200) return void res.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({ token, expiresAt: "2030-01-01T00:00:00Z" }));
      return void res.writeHead(st).end();
    }
    res.writeHead(404).end();
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  return { api: `http://127.0.0.1:${(srv.address() as AddressInfo).port}`, seen, token, close: () => srv.close() };
}

test("202 then 200: the credential carries the api, token, expiry and username", async () => {
  const s = await stub([202, 202, 200]);
  try {
    const l = await startLogin(s.api, "mac (desktop)", { signal: new AbortController().signal, pollMs: 5 });
    assert.equal(l.code, "BCDF-GH23");
    assert.equal(l.url, `${s.api}/cli/authorize?code=BCDF-GH23`);
    assert.deepEqual(await l.done, { api: s.api, token: s.token, expiresAt: "2030-01-01T00:00:00Z", username: "karthik" });
    assert.equal(s.seen.device, "mac (desktop)");
    assert.deepEqual(s.seen.polls, ["p0ll", "p0ll", "p0ll"]);
  } finally {
    s.close();
  }
});

test("a 5xx is retried, a 410 is terminal", async () => {
  const s = await stub([502, 410]);
  try {
    const l = await startLogin(s.api, "d", { signal: new AbortController().signal, pollMs: 5 });
    await assert.rejects(l.done, LoginFailed);
    assert.equal(s.seen.polls.length, 2);
  } finally {
    s.close();
  }
});

test("past expiresIn the login times out", async () => {
  const s = await stub([], 0);
  try {
    const l = await startLogin(s.api, "d", { signal: new AbortController().signal, pollMs: 5 });
    await assert.rejects(l.done, /timed out/);
  } finally {
    s.close();
  }
});

test("cancel stops polling", async () => {
  const s = await stub([]);
  try {
    const ac = new AbortController();
    const l = await startLogin(s.api, "d", { signal: ac.signal, pollMs: 5 });
    ac.abort();
    await assert.rejects(l.done, { name: "AbortError" });
    const n = s.seen.polls.length;
    await new Promise((r) => setTimeout(r, 30));
    assert.equal(s.seen.polls.length, n);
  } finally {
    s.close();
  }
});

test("claim reads the JWT payload without verifying it", () => {
  assert.equal(claim(jwt({ jti: "abc" }), "jti"), "abc");
  assert.equal(claim("not-a-jwt", "jti"), undefined);
});

test("only the api's own authorize URL is openable", () => {
  const api = "https://dev.kloudlite.io";
  assert.ok(isAuthorizeUrl(api, authorizeUrl(api, "BCDF-GH23")));
  assert.ok(!isAuthorizeUrl(api, "https://evil.test/cli/authorize?code=BCDF-GH23"));
  assert.ok(!isAuthorizeUrl(api, `${api}/cli/authorize?code=BCDF-GH23&next=https://evil.test`));
  assert.ok(!isAuthorizeUrl(api, `${api}.evil.test/cli/authorize?code=BCDF-GH23`));
  assert.ok(!isAuthorizeUrl(api, `file:///etc/passwd`));
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd harness && node --test bench/test/desktop-device.test.ts`
Expected: FAIL with `does not provide an export named 'authorizeUrl'`

- [ ] **Step 3: Write minimal implementation**

Append to `harness/src/auth/device.ts`:

```ts
/**
 * The device-code login, exactly as `bins/kl-connect/src/login.rs` runs it: ask for a code, let
 * the person approve it in their own browser, poll for the token. Nothing held before approval
 * is a credential. 410 is the api's one terminal answer (expired, denied or already collected);
 * anything else unexpected is retried until the code's own expiry, as the CLI does.
 */
export class LoginFailed extends Error {
  constructor(message: string) {
    super(message);
    this.name = "LoginFailed";
  }
}

const CODE = /^[A-Z0-9]{4}-[A-Z0-9]{4}$/;

/** One claim of a JWT payload, unverified: display name and revocation id only. */
export function claim(token: string, key: string): string | undefined {
  try {
    const v = JSON.parse(Buffer.from(token.split(".")[1] ?? "", "base64url").toString("utf8")) as Record<string, unknown>;
    return typeof v[key] === "string" ? (v[key] as string) : undefined;
  } catch {
    return undefined;
  }
}

export const authorizeUrl = (api: string, code: string) => `${api}/cli/authorize?code=${code}`;

/** The one URL the app will hand to the OS browser. */
export function isAuthorizeUrl(api: string, url: string): boolean {
  const prefix = authorizeUrl(api, "");
  return url.startsWith(prefix) && CODE.test(url.slice(prefix.length));
}

const sleep = (ms: number, signal: AbortSignal) =>
  new Promise<void>((resolve, reject) => {
    if (signal.aborted) return reject(signal.reason);
    const t = setTimeout(resolve, ms);
    signal.addEventListener("abort", () => (clearTimeout(t), reject(signal.reason)), { once: true });
  });

export async function startLogin(api: string, device: string, opts: { signal: AbortSignal; pollMs?: number }) {
  const r = await fetch(`${api}/v1/cli/code`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ device }),
    signal: opts.signal,
  });
  if (!r.ok) throw new LoginFailed(`Kloudlite would not start a login (${r.status})`);
  const dc = (await r.json()) as { code: string; poll: string; expiresIn: number };
  if (!CODE.test(dc.code)) throw new LoginFailed("Kloudlite answered with a malformed login code");
  const deadline = Date.now() + dc.expiresIn * 1000;
  const every = opts.pollMs ?? 2000;

  const done = (async (): Promise<Credential> => {
    for (;;) {
      if (Date.now() > deadline) throw new LoginFailed("timed out waiting for approval");
      let status = 0;
      let body: { token?: string; expiresAt?: string } = {};
      try {
        const p = await fetch(`${api}/v1/cli/token?poll=${encodeURIComponent(dc.poll)}`, { signal: opts.signal });
        status = p.status;
        if (status === 200) body = (await p.json()) as typeof body;
        else await p.body?.cancel();
      } catch (e) {
        if (opts.signal.aborted) throw e;
        // a dropped connection mid-login is retried: the code stays valid until the api expires it
      }
      if (status === 200 && body.token) {
        const username = claim(body.token, "username") ?? claim(body.token, "name") ?? claim(body.token, "sub") ?? "";
        return { api, token: body.token, expiresAt: body.expiresAt ?? "", username };
      }
      if (status === 410) throw new LoginFailed("that login expired or was denied");
      await sleep(every, opts.signal);
    }
  })();
  done.catch(() => undefined); // observed by the caller; never an unhandled rejection meanwhile
  return { code: dc.code, url: authorizeUrl(api, dc.code), done };
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd harness && node --test bench/test/desktop-device.test.ts`
Expected: PASS, 6 tests

- [ ] **Step 5: Commit**

```bash
git add harness/src/auth/device.ts harness/bench/test/desktop-device.test.ts
git commit -m "Sign the desktop app in with the CLI device code"
```

---

### Task 3: Auth state machine

**Files:**
- Create: `harness/src/auth/controller.ts`
- Test: `harness/bench/test/desktop-controller.test.ts`

**Interfaces:**
- Consumes (type-only): `Credential` (Task 1).
- Produces:

```ts
export type AuthState =
  | { phase: "starting" }
  | { phase: "signed-out"; reason?: string }
  | { phase: "waiting"; code: string; url: string }
  | { phase: "connecting"; step: string }
  | { phase: "ready"; username: string }
  | { phase: "error"; message: string; retry: "launch" | "connect" | "none" };

export type Deps = {
  api(): string;
  store: { load(): Credential | undefined; save(c: Credential): void; clear(): void };
  startLogin(api: string, signal: AbortSignal): Promise<{ code: string; url: string; done: Promise<Credential> }>;
  openExternal(url: string): Promise<void>;
  validate(c: Credential): Promise<"ok" | "expired">; // throws when Kloudlite is unreachable
  connect(c: Credential, step: (s: string) => void): Promise<() => void>; // resolves a disconnect; rejects name "Expired" on 401
  revoke(c: Credential): Promise<void>; // best effort, never throws
  emit(s: AuthState): void;
};

export function createAuth(d: Deps): {
  state(): AuthState;
  launch(): Promise<void>;
  retry(): Promise<void>;
  signIn(): Promise<void>;
  cancel(): void;
  signOut(): Promise<void>;
  expired(): void;
};
```

- [ ] **Step 1: Write the failing test**

`harness/bench/test/desktop-controller.test.ts`:

```ts
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd harness && node --test bench/test/desktop-controller.test.ts`
Expected: FAIL with `Cannot find module '.../src/auth/controller.ts'`

- [ ] **Step 3: Write minimal implementation**

`harness/src/auth/controller.ts`:

```ts
import type { Credential } from "./device";

/**
 * What the window shows, decided here and nowhere else. Every effect (keychain, network,
 * browser, tunnel) is injected so the transitions run under node --test; main.ts supplies the
 * real ones. The state never carries the token — it is what crosses to the renderer.
 */
export type AuthState =
  | { phase: "starting" }
  | { phase: "signed-out"; reason?: string }
  | { phase: "waiting"; code: string; url: string }
  | { phase: "connecting"; step: string }
  | { phase: "ready"; username: string }
  | { phase: "error"; message: string; retry: "launch" | "connect" | "none" };

export type Deps = {
  api(): string;
  store: { load(): Credential | undefined; save(c: Credential): void; clear(): void };
  startLogin(api: string, signal: AbortSignal): Promise<{ code: string; url: string; done: Promise<Credential> }>;
  openExternal(url: string): Promise<void>;
  validate(c: Credential): Promise<"ok" | "expired">;
  connect(c: Credential, step: (s: string) => void): Promise<() => void>;
  revoke(c: Credential): Promise<void>;
  emit(s: AuthState): void;
};

const EXPIRED = "signed out: expired or revoked";
const msg = (e: unknown) => (e instanceof Error ? e.message : String(e));
const named = (e: unknown, name: string) => e instanceof Error && e.name === name;

export function createAuth(d: Deps) {
  let state: AuthState = { phase: "starting" };
  let pending: AbortController | undefined;
  let disconnect: (() => void) | undefined;
  const set = (s: AuthState) => {
    state = s;
    d.emit(s);
  };
  const drop = () => {
    disconnect?.();
    disconnect = undefined;
  };

  const connect = async (c: Credential) => {
    set({ phase: "connecting", step: "connecting to your bench" });
    try {
      disconnect = await d.connect(c, (step) => set({ phase: "connecting", step }));
      set({ phase: "ready", username: c.username });
    } catch (e) {
      if (named(e, "Expired")) return expired();
      set({ phase: "error", message: msg(e), retry: "connect" });
    }
  };

  const launch = async () => {
    let c: Credential | undefined;
    try {
      c = d.store.load();
    } catch (e) {
      return set({ phase: "error", message: msg(e), retry: "none" });
    }
    if (!c) return set({ phase: "signed-out" });
    try {
      if ((await d.validate(c)) === "expired") {
        d.store.clear();
        return set({ phase: "signed-out", reason: EXPIRED });
      }
    } catch (e) {
      // unreachable is not revoked: the login stays for the retry
      return set({ phase: "error", message: msg(e), retry: "launch" });
    }
    await connect(c);
  };

  function expired() {
    drop();
    d.store.clear();
    set({ phase: "signed-out", reason: EXPIRED });
  }

  return {
    state: () => state,
    launch,
    async retry() {
      if (state.phase !== "error") return;
      if (state.retry === "launch") return launch();
      const c = state.retry === "connect" ? d.store.load() : undefined;
      if (c) return connect(c);
    },
    async signIn() {
      if (state.phase === "waiting" || state.phase === "connecting" || state.phase === "ready") return;
      pending = new AbortController();
      const ac = pending;
      try {
        const l = await d.startLogin(d.api(), ac.signal);
        set({ phase: "waiting", code: l.code, url: l.url });
        await d.openExternal(l.url).catch(() => undefined); // the code and URL are on screen either way
        const c = await l.done;
        d.store.save(c);
        await connect(c);
      } catch (e) {
        if (ac.signal.aborted) return set({ phase: "signed-out" });
        if (named(e, "NoKeychain")) return set({ phase: "error", message: msg(e), retry: "none" });
        set({ phase: "signed-out", reason: msg(e) });
      } finally {
        if (pending === ac) pending = undefined;
      }
    },
    cancel() {
      pending?.abort();
    },
    async signOut() {
      const c = (() => {
        try {
          return d.store.load();
        } catch {
          return undefined;
        }
      })();
      if (c) await d.revoke(c);
      d.store.clear();
      drop();
      set({ phase: "signed-out" });
    },
    expired,
  };
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd harness && node --test bench/test/desktop-controller.test.ts`
Expected: PASS, 12 tests

- [ ] **Step 5: Commit**

```bash
git add harness/src/auth/controller.ts harness/bench/test/desktop-controller.test.ts
git commit -m "Decide the desktop login state in one place"
```

---

### Task 4: Bench session client

**Files:**
- Create: `harness/src/connect/bench.ts`
- Test: `harness/bench/test/desktop-bench.test.ts`

**Interfaces:**
- Produces:
  - `type Session = { id: string; token: string; gateway: string; expires_at: string }`
  - `class Expired extends Error` (name `"Expired"`)
  - `ensureBench(api: string, token: string, step: (s: string) => void, opts?: { sleepMs?: number; waitMs?: number }): Promise<void>` — creates the bench on 404 (`POST /v1/bench` `{}`), starts it on the stopped 409 (`POST /v1/bench/start`), waits through 202 with `step("bench is <state>")`, returns once a session would be ready; any other refusal throws `Error(<server message>)`; 401 throws `Expired`.
  - `mintSession(api: string, token: string, opts?: { sleepMs?: number; waitMs?: number }): Promise<Session>` — one ready session; waits through 202 (a bench that idled between connections), never creates or starts; 401 `Expired`; anything else throws the server's message.

Both default `sleepMs` 1000 and `waitMs` 90 000, the values `bins/kl-connect/src/bench.rs` uses (`BENCH_START_WAIT`). The first session `ensureBench` mints is thrown away unused; it expires in 60 s on its own (single-use, never dialled).

- [ ] **Step 1: Write the failing test**

`harness/bench/test/desktop-bench.test.ts`:

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import type { AddressInfo } from "node:net";
import { ensureBench, Expired, mintSession } from "../../src/connect/bench.ts";

type Answer = { status: number; body?: unknown };
/** A stub api answering each route from its own queue; the last answer repeats. */
async function stub(routes: Record<string, Answer[]>) {
  const calls: string[] = [];
  const auth: string[] = [];
  const srv = http.createServer((req, res) => {
    const key = `${req.method} ${req.url}`;
    calls.push(key);
    auth.push(req.headers.authorization ?? "");
    const q = routes[key];
    const a = q && (q.length > 1 ? q.shift()! : q[0]);
    if (!a) return void res.writeHead(404, { "content-type": "application/json" }).end(JSON.stringify({ error: "no route" }));
    res.writeHead(a.status, { "content-type": "application/json" }).end(a.body === undefined ? "" : JSON.stringify(a.body));
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  return { api: `http://127.0.0.1:${(srv.address() as AddressInfo).port}`, calls, auth, close: () => srv.close() };
}
const ready = { status: 201, body: { id: "bench-k", token: "s1", gateway: "wss://g/tunnel/bench-k", expires_at: "2030" } };
const fast = { sleepMs: 1, waitMs: 2000 };

test("a ready bench: one session call, bearer auth", async () => {
  const s = await stub({ "POST /v1/bench/session": [ready] });
  try {
    await ensureBench(s.api, "tok", () => undefined, fast);
    assert.deepEqual(s.calls, ["POST /v1/bench/session"]);
    assert.equal(s.auth[0], "Bearer tok");
  } finally {
    s.close();
  }
});

test("no bench yet: it is created, then waited for", async () => {
  const steps: string[] = [];
  const s = await stub({
    "POST /v1/bench/session": [{ status: 404, body: { error: "no bench" } }, { status: 202, body: { state: "starting" } }, ready],
    "POST /v1/bench": [{ status: 201, body: {} }],
  });
  try {
    await ensureBench(s.api, "tok", (x) => steps.push(x), fast);
    assert.deepEqual(s.calls, ["POST /v1/bench/session", "POST /v1/bench", "POST /v1/bench/session", "POST /v1/bench/session"]);
    assert.ok(steps.includes("creating your bench"));
    assert.ok(steps.includes("bench is starting"));
  } finally {
    s.close();
  }
});

test("a stopped bench is started once", async () => {
  const s = await stub({
    "POST /v1/bench/session": [{ status: 409, body: { error: "bench is stopped; start it" } }, ready],
    "POST /v1/bench/start": [{ status: 202 }],
  });
  try {
    await ensureBench(s.api, "tok", () => undefined, fast);
    assert.deepEqual(s.calls, ["POST /v1/bench/session", "POST /v1/bench/start", "POST /v1/bench/session"]);
  } finally {
    s.close();
  }
});

test("a quota refusal is shown as the server said it, with no retry loop", async () => {
  const s = await stub({
    "POST /v1/bench/session": [{ status: 409, body: { error: "bench is stopped; start it" } }],
    "POST /v1/bench/start": [{ status: 409, body: { error: "cpu: 40 of 40 in use; request more under Quota" } }],
  });
  try {
    await assert.rejects(ensureBench(s.api, "tok", () => undefined, fast), /cpu: 40 of 40 in use/);
    assert.equal(s.calls.filter((c) => c === "POST /v1/bench/start").length, 1);
  } finally {
    s.close();
  }
});

test("401 anywhere is Expired", async () => {
  const s = await stub({ "POST /v1/bench/session": [{ status: 401 }] });
  try {
    await assert.rejects(ensureBench(s.api, "tok", () => undefined, fast), Expired);
    await assert.rejects(mintSession(s.api, "tok", fast), Expired);
  } finally {
    s.close();
  }
});

test("mintSession waits through waking and gives up after waitMs", async () => {
  const s = await stub({ "POST /v1/bench/session": [{ status: 202, body: { state: "waking" } }, ready] });
  try {
    assert.equal((await mintSession(s.api, "tok", fast)).token, "s1");
  } finally {
    s.close();
  }
  const never = await stub({ "POST /v1/bench/session": [{ status: 202, body: { state: "waking" } }] });
  try {
    await assert.rejects(mintSession(never.api, "tok", { sleepMs: 5, waitMs: 30 }), /did not start/);
  } finally {
    never.close();
  }
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd harness && node --test bench/test/desktop-bench.test.ts`
Expected: FAIL with `Cannot find module '.../src/connect/bench.ts'`

- [ ] **Step 3: Write minimal implementation**

`harness/src/connect/bench.ts`:

```ts
/**
 * The person's own bench, as `kl-connect bench` reaches it (`bins/kl-connect/src/api.rs`):
 * `POST /v1/bench/session` answers 201 with a single-use session, 202 while it wakes, 409 when
 * stopped, 404 when there is none. `ensureBench` is the Connect step (create/start/wait, once);
 * `mintSession` is what every tunnel connection calls and never allocates anything itself.
 */
export type Session = { id: string; token: string; gateway: string; expires_at: string };

export class Expired extends Error {
  constructor() {
    super("your login has expired or was revoked");
    this.name = "Expired";
  }
}

const STOPPED = "bench is stopped; start it";
type Answer = { status: number; body: { error?: string; state?: string } & Partial<Session> };

async function call(api: string, token: string, method: string, path: string, body?: unknown): Promise<Answer> {
  const r = await fetch(api + path, {
    method,
    headers: { authorization: `Bearer ${token}`, ...(body === undefined ? {} : { "content-type": "application/json" }) },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (r.status === 401) throw new Expired();
  const text = await r.text();
  let parsed: Answer["body"] = {};
  try {
    parsed = text ? JSON.parse(text) : {};
  } catch {
    parsed = { error: text };
  }
  return { status: r.status, body: parsed };
}
const refused = (a: Answer) => new Error(a.body.error || `Kloudlite answered ${a.status}`);
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export async function ensureBench(api: string, token: string, step: (s: string) => void, opts: { sleepMs?: number; waitMs?: number } = {}) {
  const deadline = Date.now() + (opts.waitMs ?? 90_000);
  let created = false;
  let started = false;
  for (;;) {
    const a = await call(api, token, "POST", "/v1/bench/session");
    if (a.status >= 200 && a.status < 300 && a.status !== 202) return;
    if (a.status === 404 && !created) {
      created = true;
      step("creating your bench");
      const c = await call(api, token, "POST", "/v1/bench", {});
      if (c.status >= 300) throw refused(c);
      continue;
    }
    if (a.status === 409 && a.body.error === STOPPED && !started) {
      started = true;
      step("starting your bench");
      const s = await call(api, token, "POST", "/v1/bench/start");
      if (s.status >= 300) throw refused(s);
      continue;
    }
    if (a.status !== 202) throw refused(a);
    step(`bench is ${a.body.state ?? "starting"}`);
    if (Date.now() >= deadline) throw new Error("your bench did not start within 90 s");
    await sleep(opts.sleepMs ?? 1000);
  }
}

export async function mintSession(api: string, token: string, opts: { sleepMs?: number; waitMs?: number } = {}): Promise<Session> {
  const deadline = Date.now() + (opts.waitMs ?? 90_000);
  for (;;) {
    const a = await call(api, token, "POST", "/v1/bench/session");
    if (a.status === 202) {
      if (Date.now() >= deadline) throw new Error("your bench did not start within 90 s");
      await sleep(opts.sleepMs ?? 1000);
      continue;
    }
    if (a.status >= 300 || !a.body.token || !a.body.gateway) throw refused(a);
    return a.body as Session;
  }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd harness && node --test bench/test/desktop-bench.test.ts`
Expected: PASS, 6 tests

- [ ] **Step 5: Commit**

```bash
git add harness/src/connect/bench.ts harness/bench/test/desktop-bench.test.ts
git commit -m "Find, start and wake the person's bench from the desktop app"
```

---

### Task 5: In-process tunnel

**Files:**
- Create: `harness/src/connect/tunnel.ts`
- Test: `harness/bench/test/desktop-tunnel.test.ts`

**Interfaces:**
- Consumes (type-only): `Session` (Task 4). The mint function is injected, so the tunnel imports nothing of `bench.ts` at runtime.
- Produces: `openTunnel(mint: () => Promise<Session>, onError: (e: Error) => void): Promise<{ base: string; close(): void }>` — `base` is `http://127.0.0.1:<port>`.

Mirrors `bins/kl-connect/src/bench.rs` `serve_conn` + `proxy::connect`/`pump_io`: the socket is paused from accept (`pauseOnConnect`) so bytes written while the bench wakes are delivered, the gateway is dialled with `Authorization: Bearer <session token>`, frames are binary both ways, and either side closing closes the other. Error text passed to `onError` never includes the session token (`ws` errors carry no request headers; the mint's errors are the server's messages).

- [ ] **Step 1: Write the failing test**

`harness/bench/test/desktop-tunnel.test.ts`:

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import net from "node:net";
import type { AddressInfo } from "node:net";
import { WebSocketServer } from "ws";
import { openTunnel } from "../../src/connect/tunnel.ts";

/** A stub gateway: /tunnel/bench-k echoes binary frames and records each upgrade's bearer. */
async function gateway() {
  const bearers: string[] = [];
  const srv = http.createServer((_q, r) => r.writeHead(404).end());
  const wss = new WebSocketServer({ noServer: true });
  srv.on("upgrade", (req, sock, head) => {
    if (req.url !== "/tunnel/bench-k") return sock.destroy();
    bearers.push(req.headers.authorization ?? "");
    wss.handleUpgrade(req, sock, head, (ws) => ws.on("message", (d, bin) => ws.send(d, { binary: bin })));
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const url = `ws://127.0.0.1:${(srv.address() as AddressInfo).port}/tunnel/bench-k`;
  return { url, bearers, close: () => (wss.close(), srv.close()) };
}

const roundTrip = (port: number, payload: Buffer) =>
  new Promise<Buffer>((resolve, reject) => {
    const s = net.connect(port, "127.0.0.1", () => void s.write(payload));
    let got = Buffer.alloc(0);
    s.on("data", (d) => {
      got = Buffer.concat([got, d]);
      if (got.length >= payload.length) (s.end(), resolve(got));
    });
    s.on("error", reject);
  });

test("each TCP connection mints its own token and reaches the gateway with it", async () => {
  const g = await gateway();
  let n = 0;
  const t = await openTunnel(async () => ({ id: "bench-k", token: `s${++n}`, gateway: g.url, expires_at: "2030" }), (e) => assert.fail(e));
  try {
    const port = Number(new URL(t.base).port);
    assert.equal((await roundTrip(port, Buffer.from("a"))).toString(), "a");
    assert.equal((await roundTrip(port, Buffer.from("b"))).toString(), "b");
    assert.equal(n, 2);
    assert.deepEqual(g.bearers, ["Bearer s1", "Bearer s2"]);
  } finally {
    t.close();
    g.close();
  }
});

test("bytes written while the mint is still waiting are delivered", async () => {
  const g = await gateway();
  const t = await openTunnel(async () => {
    await new Promise((r) => setTimeout(r, 100)); // a waking bench
    return { id: "bench-k", token: "s", gateway: g.url, expires_at: "2030" };
  }, (e) => assert.fail(e));
  try {
    const echoed = await roundTrip(Number(new URL(t.base).port), Buffer.from("early bytes"));
    assert.equal(echoed.toString(), "early bytes");
  } finally {
    t.close();
    g.close();
  }
});

test("it listens on 127.0.0.1 only", async () => {
  const t = await openTunnel(async () => { throw new Error("unused"); }, () => undefined);
  try {
    assert.match(t.base, /^http:\/\/127\.0\.0\.1:\d+$/);
  } finally {
    t.close();
  }
});

test("a failed mint closes that connection and reports, without the listener dying", async () => {
  const errors: string[] = [];
  const t = await openTunnel(async () => { throw Object.assign(new Error("your login has expired or was revoked"), { name: "Expired" }); }, (e) => errors.push(e.name));
  try {
    const port = Number(new URL(t.base).port);
    for (let i = 0; i < 2; i++) {
      await new Promise<void>((resolve) => {
        const s = net.connect(port, "127.0.0.1");
        s.on("close", () => resolve());
        s.on("error", () => undefined);
      });
    }
    assert.deepEqual(errors, ["Expired", "Expired"]);
  } finally {
    t.close();
  }
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd harness && node --test bench/test/desktop-tunnel.test.ts`
Expected: FAIL with `Cannot find module '.../src/connect/tunnel.ts'`

- [ ] **Step 3: Write minimal implementation**

`harness/src/connect/tunnel.ts`:

```ts
import net from "node:net";
import WebSocket from "ws";
import type { Session } from "./bench";

/**
 * `kl-connect bench`, in the main process: a 127.0.0.1 port whose every accepted TCP connection
 * gets its own single-use session token and its own gateway WebSocket — nothing is multiplexed,
 * so a token is spent exactly once. BenchClient talks plain HTTP/WS to `base` and never learns
 * the tunnel exists, the same as it did with HARNESS_BENCH.
 */
export async function openTunnel(mint: () => Promise<Session>, onError: (e: Error) => void): Promise<{ base: string; close(): void }> {
  const open = new Set<net.Socket>();
  // Paused from accept: a client that writes while the bench wakes keeps its bytes queued.
  const server = net.createServer({ pauseOnConnect: true }, (sock) => {
    open.add(sock);
    sock.on("close", () => open.delete(sock));
    sock.on("error", () => undefined); // close follows
    void (async () => {
      let s: Session;
      try {
        s = await mint();
      } catch (e) {
        onError(e as Error);
        return void sock.destroy();
      }
      if (sock.destroyed) return;
      const ws = new WebSocket(s.gateway, { headers: { authorization: `Bearer ${s.token}` } });
      ws.on("open", () => {
        sock.on("data", (d) => ws.send(d, { binary: true }));
        sock.on("end", () => ws.close());
        sock.resume();
      });
      ws.on("message", (d) => sock.write(d as Buffer));
      ws.on("close", () => sock.end());
      ws.on("error", () => sock.destroy()); // the message may name the gateway, never the token
      sock.on("close", () => ws.readyState === WebSocket.CLOSED || ws.terminate());
    })();
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const { port } = server.address() as net.AddressInfo;
  return {
    base: `http://127.0.0.1:${port}`,
    close() {
      server.close();
      for (const s of open) s.destroy();
    },
  };
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd harness && node --test bench/test/desktop-tunnel.test.ts`
Expected: PASS, 4 tests

- [ ] **Step 5: Commit**

```bash
git add harness/src/connect/tunnel.ts harness/bench/test/desktop-tunnel.test.ts
git commit -m "Tunnel to the bench from the desktop app, one token per connection"
```

---

### Task 6: Main-process wiring, preload, sign-out

**Files:**
- Modify: `harness/src/main.ts:1-14` (imports, `BENCH`, `bench`), `:253-256` (`needBench` message), `:315` (`before-quit`), `:322-338` (`whenReady`)
- Modify: `harness/src/preload.ts` (add `auth`)
- Modify: `harness/src/bench-client.ts:34-46` (cache key)

**Interfaces:**
- Consumes: `createStore`, `NoKeychain` (Task 1); `startLogin`, `claim`, `isAuthorizeUrl`, `Credential` (Task 2); `createAuth`, `AuthState` (Task 3); `ensureBench`, `mintSession`, `Expired` (Task 4); `openTunnel` (Task 5).
- Produces for the renderer (`window.harness.auth`):
  - `status(): Promise<AuthState>`
  - `signIn(): Promise<void>`, `cancel(): Promise<void>`, `retry(): Promise<void>`, `signOut(): Promise<void>`
  - `api(): Promise<string>`, `setApi(url: string): Promise<void>` (only while signed out)
  - `onState(fn: (s: AuthState) => void): void` (IPC channel `auth:state`)
- `BenchClient` constructor becomes `(base: string, emit: Emit, cacheFile: string, cacheKey = base)`.

No unit test here: this is the glue between Electron and the tested modules. It is checked by `npm run typecheck`, the full `npm run bench:test` (the existing `bench-client.test.ts` must still pass with the defaulted `cacheKey`), and Task 8.

- [ ] **Step 1: Let BenchClient key its cache by the person, not the port**

In `harness/src/bench-client.ts` replace the constructor (lines 34-46):

```ts
  // `cacheKey` defaults to the address; the desktop app passes the username, because the
  // tunnel's port is new every launch and would otherwise empty the cache every time.
  constructor(base: string, emit: Emit, cacheFile: string, cacheKey = base) {
    this.base = base.replace(/\/$/, "");
    this.emit = emit;
    this.cacheFile = cacheFile;
    const empty: Cache = { base: cacheKey, sessions: [], exchanges: [], messages: {} };
    try {
      const c = JSON.parse(fs.readFileSync(cacheFile, "utf8")) as Cache;
      // Keyed by whose bench it is: another person's list is not this one's.
      this.cache = c.base === cacheKey ? c : empty;
    } catch {
      this.cache = empty;
    }
  }
```

- [ ] **Step 2: Replace the top of `main.ts` (lines 1-14)**

```ts
import { app, BrowserWindow, Menu, WebContentsView, clipboard, ipcMain, nativeTheme, safeStorage, shell, type WebContents } from "electron";
import os from "node:os";
import path from "node:path";
import fs from "node:fs/promises";
import { readFileSync, writeFileSync, rmSync } from "node:fs";
import { BenchClient } from "./bench-client";
import { batchImport, isLaptopRow, safeJsonlName, toItem, type ImportRow } from "./import-payload";
import { createStore } from "./auth/store";
import { claim, isAuthorizeUrl, startLogin, type Credential } from "./auth/device";
import { createAuth, type AuthState } from "./auth/controller";
import { ensureBench, mintSession } from "./connect/bench";
import { openTunnel } from "./connect/tunnel";

// One app, one login, one tunnel: a second launch focuses the first instead. `exit`, not
// `quit`: quit is asynchronous and whenReady below would still open a window first.
if (!app.requestSingleInstanceLock()) app.exit(0);

let mainWin: BrowserWindow | undefined;
// Login is always required. HARNESS_BENCH is a developer override for WHERE the bench is
// (a hand-started tunnel or a local harness-bench); without it the app finds the person's own.
const BENCH = process.env.HARNESS_BENCH;
let bench: BenchClient | undefined;
const toRenderer = (ev: Record<string, unknown>) => {
  if (mainWin && !mainWin.isDestroyed()) mainWin.webContents.send("pi:event", ev);
};

// The API base: a build-time default, overridable from the login screen while signed out.
const DEFAULT_API = "https://dev.kloudlite.io";
const apiFile = () => path.join(app.getPath("userData"), "api.txt");
const apiBase = () => {
  try {
    return readFileSync(apiFile(), "utf8").trim() || DEFAULT_API;
  } catch {
    return DEFAULT_API;
  }
};
```

- [ ] **Step 3: Update `needBench` (line 254) and `before-quit` (line 315)**

```ts
const needBench = () => {
  if (!bench) throw new Error("not connected to your bench yet");
  return bench;
};
```

```ts
app.on("before-quit", () => disconnect());
```

- [ ] **Step 4: Replace the `whenReady` block and add the auth wiring (lines 322-338)**

```ts
/** Tears down whatever Connect built; safe to call when nothing is connected. */
let closeTunnel: (() => void) | undefined;
function disconnect() {
  bench?.close();
  bench = undefined;
  closeTunnel?.();
  closeTunnel = undefined;
}

let auth: ReturnType<typeof createAuth>;
let wasReady = false;
const emitAuth = (s: AuthState) => {
  if (!mainWin || mainWin.isDestroyed()) return;
  // Leaving `ready` reloads the window: App registers listeners for the life of the page, so a
  // fresh page is the honest way back to the login screen.
  if (wasReady && s.phase !== "ready") mainWin.webContents.reload();
  wasReady = s.phase === "ready";
  mainWin.webContents.send("auth:state", s);
};

function authDeps() {
  const store = createStore(path.join(app.getPath("userData"), "credential.bin"), safeStorage);
  return {
    api: apiBase,
    store,
    startLogin: (api: string, signal: AbortSignal) => startLogin(api, `${os.hostname()} (desktop)`, { signal }),
    openExternal: async (url: string) => {
      if (!isAuthorizeUrl(apiBase(), url)) throw new Error("refusing to open a URL that is not Kloudlite's login page");
      await shell.openExternal(url);
    },
    validate: async (c: Credential) => {
      let r: Response;
      try {
        r = await fetch(`${c.api}/v1/cli/tokens`, { headers: { authorization: `Bearer ${c.token}` }, signal: AbortSignal.timeout(10_000) });
      } catch {
        throw new Error("can't reach Kloudlite");
      }
      await r.body?.cancel();
      if (r.status === 401) return "expired" as const;
      if (!r.ok) throw new Error(`Kloudlite answered ${r.status}`);
      return "ok" as const;
    },
    connect: async (c: Credential, step: (s: string) => void) => {
      const cache = path.join(app.getPath("userData"), "bench-cache.json");
      if (BENCH) {
        bench = new BenchClient(BENCH, toRenderer, cache, `${c.username}@${BENCH}`);
      } else {
        await ensureBench(c.api, c.token, step);
        const t = await openTunnel(
          () => mintSession(c.api, c.token),
          (e) => (e.name === "Expired" ? auth.expired() : console.error(`bench tunnel: ${e.message}`)),
        );
        closeTunnel = t.close;
        bench = new BenchClient(t.base, toRenderer, cache, `${c.username}@${c.api}`);
      }
      bench.start();
      return disconnect;
    },
    revoke: async (c: Credential) => {
      const jti = claim(c.token, "jti");
      if (!jti) return;
      // Best effort: a login that cannot be revoked server-side still leaves this disk.
      await fetch(`${c.api}/v1/cli/tokens/${encodeURIComponent(jti)}`, {
        method: "DELETE",
        headers: { authorization: `Bearer ${c.token}` },
        signal: AbortSignal.timeout(5000),
      }).catch(() => undefined);
    },
    emit: emitAuth,
  };
}

ipcMain.handle("auth:status", () => auth.state());
ipcMain.handle("auth:signIn", () => auth.signIn());
ipcMain.handle("auth:cancel", () => auth.cancel());
ipcMain.handle("auth:retry", () => auth.retry());
ipcMain.handle("auth:signOut", () => auth.signOut());
ipcMain.handle("auth:api", () => apiBase());
ipcMain.handle("auth:setApi", (_e, url: unknown) => {
  if (auth.state().phase !== "signed-out") throw new Error("sign out before changing the Kloudlite address");
  if (typeof url !== "string") throw new Error("not an address");
  if (url.trim() === "") return void rmSync(apiFile(), { force: true });
  const u = new URL(url.trim());
  const local = u.protocol === "http:" && (u.hostname === "127.0.0.1" || u.hostname === "localhost");
  if (u.protocol !== "https:" && !local) throw new Error("the Kloudlite address must be https");
  writeFileSync(apiFile(), u.origin);
});

app.on("second-instance", () => {
  if (!mainWin || mainWin.isDestroyed()) return;
  if (mainWin.isMinimized()) mainWin.restore();
  mainWin.focus();
});

void app.whenReady().then(() => {
  // The standard menus, explicitly: on macOS ⌘C/⌘V/⌘X/⌘A reach a web page
  // only through Edit-menu roles, and a pasted image is a paste event first.
  Menu.setApplicationMenu(
    Menu.buildFromTemplate([
      { role: "appMenu" },
      { role: "editMenu" },
      { role: "viewMenu" },
      { role: "windowMenu" },
      { label: "Account", submenu: [{ label: "Sign Out", click: () => void auth.signOut() }] },
    ]),
  );
  auth = createAuth(authDeps());
  createWindow();
  void auth.launch();
  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});
```

- [ ] **Step 5: Expose `auth` in `preload.ts`**

Add the import at the top of `harness/src/preload.ts`:

```ts
import type { AuthState } from "./auth/controller";
```

Add to the `harness` object, before `setTheme`:

```ts
  /** The desktop login. The renderer sees only the state — the token never leaves main. */
  auth: {
    status: (): Promise<AuthState> => ipcRenderer.invoke("auth:status"),
    signIn: (): Promise<void> => ipcRenderer.invoke("auth:signIn"),
    cancel: (): Promise<void> => ipcRenderer.invoke("auth:cancel"),
    retry: (): Promise<void> => ipcRenderer.invoke("auth:retry"),
    signOut: (): Promise<void> => ipcRenderer.invoke("auth:signOut"),
    api: (): Promise<string> => ipcRenderer.invoke("auth:api"),
    setApi: (url: string): Promise<void> => ipcRenderer.invoke("auth:setApi", url),
    onState: (fn: (s: AuthState) => void): void => void ipcRenderer.on("auth:state", (_e, s: AuthState) => fn(s)),
  },
```

- [ ] **Step 6: Typecheck and run the whole suite**

Run: `cd harness && npm run typecheck && npm run bench:test`
Expected: typecheck exits 0; every test passes, including the existing `bench-client.test.ts`.

- [ ] **Step 7: Commit**

```bash
git add harness/src/main.ts harness/src/preload.ts harness/src/bench-client.ts
git commit -m "Gate the desktop app on login and connect it to the bench itself"
```

---

### Task 7: Login screen and the renderer gate

**Files:**
- Create: `harness/src/renderer/login.ts`
- Create: `harness/src/renderer/components/LoginScreen.tsx`
- Modify: `harness/src/renderer/index.tsx`
- Modify: `harness/src/renderer/App.tsx:437-439` (hint), `:457` (`/login`)
- Modify: `harness/src/renderer/components/SettingsPage.tsx:24-32` (PAGES), `:64` (add Account section)
- Test: `harness/bench/test/desktop-login-screen.test.ts`

**Interfaces:**
- Consumes: `AuthState` (Task 3, type-only); `window.harness.auth` (Task 6).
- Produces: `screen(s: AuthState): { title: string; body?: string; code?: string; url?: string; actions: ("signIn" | "cancel" | "retry" | "address")[]; busy: boolean }`; `LoginScreen(props: { state: AuthState })`.

- [ ] **Step 1: Write the failing test**

`harness/bench/test/desktop-login-screen.test.ts`:

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import { screen } from "../../src/renderer/login.ts";

test("signed out offers sign in and the address, with the reason when there is one", () => {
  assert.deepEqual(screen({ phase: "signed-out" }), { title: "Sign in to Kloudlite", actions: ["signIn", "address"], busy: false });
  assert.equal(screen({ phase: "signed-out", reason: "signed out: expired or revoked" }).body, "signed out: expired or revoked");
});

test("waiting shows the code and the URL, and only cancel", () => {
  const s = screen({ phase: "waiting", code: "BCDF-GH23", url: "https://k/cli/authorize?code=BCDF-GH23" });
  assert.equal(s.code, "BCDF-GH23");
  assert.equal(s.url, "https://k/cli/authorize?code=BCDF-GH23");
  assert.deepEqual(s.actions, ["cancel"]);
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd harness && node --test bench/test/desktop-login-screen.test.ts`
Expected: FAIL with `Cannot find module '.../src/renderer/login.ts'`

- [ ] **Step 3: Write `login.ts`**

`harness/src/renderer/login.ts`:

```ts
import type { AuthState } from "../auth/controller";

/** What the login screen shows for a state: kept pure so the transitions are tested without a DOM. */
export function screen(s: AuthState): { title: string; body?: string; code?: string; url?: string; actions: ("signIn" | "cancel" | "retry" | "address")[]; busy: boolean } {
  switch (s.phase) {
    case "starting":
    case "ready":
      return { title: "Kloudlite", actions: [], busy: true };
    case "signed-out":
      return { title: "Sign in to Kloudlite", ...(s.reason ? { body: s.reason } : {}), actions: ["signIn", "address"], busy: false };
    case "waiting":
      return { title: "Confirm this code in your browser", body: "waiting for approval", code: s.code, url: s.url, actions: ["cancel"], busy: true };
    case "connecting":
      return { title: "Connecting", body: s.step, actions: [], busy: true };
    case "error":
      return { title: "Can't continue", body: s.message, actions: s.retry === "none" ? [] : ["retry"], busy: false };
  }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd harness && node --test bench/test/desktop-login-screen.test.ts`
Expected: PASS, 5 tests

- [ ] **Step 5: Write `LoginScreen.tsx`**

`harness/src/renderer/components/LoginScreen.tsx`:

```tsx
import { Show, createSignal } from "solid-js";
import type { AuthState } from "../../auth/controller";
import { Button } from "../ui/Button";
import { screen } from "../login";

/** The whole window until the person is signed in and connected. Holds no credential. */
export function LoginScreen(props: { state: AuthState }) {
  const v = () => screen(props.state);
  const has = (a: "signIn" | "cancel" | "retry" | "address") => v().actions.includes(a);
  const [editing, setEditing] = createSignal(false);
  const [address, setAddress] = createSignal("");
  const [note, setNote] = createSignal("");
  const openAddress = async () => {
    setAddress(await window.harness.auth.api());
    setNote("");
    setEditing(true);
  };
  const saveAddress = () =>
    window.harness.auth.setApi(address()).then(
      () => setEditing(false),
      (e: Error) => setNote(e.message),
    );

  return (
    <div class="flex h-screen flex-col items-center justify-center gap-4 bg-bg px-4 text-fg" style={{ "-webkit-app-region": "drag" }}>
      <div class="flex w-full max-w-[360px] flex-col items-center gap-3 text-center" style={{ "-webkit-app-region": "no-drag" }}>
        <h1 class="text-base font-medium">{v().title}</h1>
        <Show when={v().code}>{(c) => <div class="select-text font-mono text-2xl tracking-widest">{c()}</div>}</Show>
        <Show when={v().body}>{(b) => <p class="text-sm text-subtle">{b()}</p>}</Show>
        <Show when={v().url}>{(u) => <p class="select-text break-all font-mono text-xs text-subtle">{u()}</p>}</Show>
        <div class="flex gap-2">
          <Show when={has("signIn")}><Button variant="primary" onClick={() => void window.harness.auth.signIn()}>Sign in with browser</Button></Show>
          <Show when={has("cancel")}><Button onClick={() => void window.harness.auth.cancel()}>Cancel</Button></Show>
          <Show when={has("retry")}><Button variant="primary" onClick={() => void window.harness.auth.retry()}>Retry</Button></Show>
        </div>
        <Show when={has("address")}>
          <Show when={editing()} fallback={<button class="text-xs text-subtle underline" onClick={() => void openAddress()}>Kloudlite address</button>}>
            <div class="flex w-full gap-2">
              <input
                class="h-6 flex-1 rounded-[2px] border border-input-line bg-input px-1.5 font-mono text-sm outline-none focus:border-focus"
                placeholder="https://dev.kloudlite.io"
                value={address()}
                onInput={(e) => setAddress(e.currentTarget.value)}
              />
              <Button size="sm" onClick={() => void saveAddress()}>Save</Button>
            </div>
            <Show when={note()}>{(n) => <p class="text-xs text-danger">{n()}</p>}</Show>
          </Show>
        </Show>
      </div>
    </div>
  );
}
```

- [ ] **Step 6: Gate `App` in `index.tsx`**

Replace `harness/src/renderer/index.tsx` with:

```tsx
import { Show, createSignal } from "solid-js";
import { render } from "solid-js/web";
import "./fonts.css";
import "./styles/app.css";
import { App } from "./App";
import { LoginScreen } from "./components/LoginScreen";
import type { Harness } from "../preload";
import type { AuthState } from "../auth/controller";

declare global {
  interface Window { harness: Harness }
}

/** Nothing of the app — not even the cached session list — mounts before the person is signed in and connected. */
function Gate() {
  const [state, setState] = createSignal<AuthState>({ phase: "starting" });
  window.harness.auth.onState(setState);
  void window.harness.auth.status().then(setState);
  return (
    <Show when={state().phase === "ready"} fallback={<LoginScreen state={state()} />}>
      <App />
    </Show>
  );
}

// Paint once, with the faces: the first frame after a reload used to be the
// fallback font and the intro, then everything swapped. Load the two faces
// the UI is set in before anything renders; a face that fails to load does
// not hold the app hostage (the fallback stack is there for that).
void Promise.all([document.fonts.load('13px "IBM Plex Sans"'), document.fonts.load("13px Lilex")])
  .catch(() => undefined)
  .then(() => render(() => <Gate />, document.getElementById("root")!));
```

`Show` without `keyed` keeps `App` mounted while the state stays `ready`; leaving `ready` reloads the page from main (Task 6), so `App` is never unmounted and remounted in place.

- [ ] **Step 7: Drop the `/login` alias and the stale hint in `App.tsx`**

Delete line 457:

```ts
    "/login": { help: "log in to Kloudlite in your browser", run: () => void pi({ type: "prompt", message: "/kl-login" }) },
```

Replace line 439:

```ts
    if (!st.configured) return void live.thread("bench").note("no bench: start the harness with HARNESS_BENCH=http://127.0.0.1:<port>");
```

with:

```ts
    if (!st.configured) return void live.thread("bench").note("not connected to your bench yet");
```

Change the two `/kl-login` help strings (line 489's trailing `\n/kl-login  log in to Kloudlite` and line 497's `help`) to `log the bench's own tools in to Kloudlite`, so nobody mistakes it for signing this app in. Leave `live.ts:219` (the `kl-login` message note) as it is.

- [ ] **Step 8: Add the Account page to `SettingsPage.tsx`**

Extend the `Page` union and `PAGES` (lines 24-32):

```ts
  type Page = "model" | "providers" | "tools" | "skills" | "mcp" | "hooks" | "keys" | "account" | "discover";
  const PAGES: { id: Page; label: string }[] = [
    { id: "model", label: "Model" },
    { id: "providers", label: "Providers" },
    { id: "tools", label: "Tools" },
    { id: "skills", label: "Skills" },
    { id: "mcp", label: "MCP servers" },
    { id: "hooks", label: "Hooks" },
    { id: "keys", label: "Keyboard" },
    { id: "account", label: "Account" },
  ];
  const [who, setWho] = createSignal("");
  void window.harness.auth.status().then((s) => s.phase === "ready" && setWho(s.username));
```

Insert before `<Show when={page() === "model"}>` (line 64):

```tsx
          <Show when={page() === "account"}>
            <Section id="account" title="Account" hint="this app's Kloudlite login">
              <Row name="signed in as" detail="a CLI login labelled with this computer's name and (desktop)">
                <span class="font-mono text-sm text-fg">{who()}</span>
              </Row>
              <Row name="sign out" detail="revokes this login, forgets it here, and disconnects the bench">
                <Button variant="danger" onClick={() => void window.harness.auth.signOut()}>Sign out</Button>
              </Row>
            </Section>
          </Show>
```

- [ ] **Step 9: Typecheck, run the suite, build**

Run: `cd harness && npm run typecheck && npm run bench:test && npm run build`
Expected: all exit 0; the five new login-screen tests pass with the rest.

- [ ] **Step 10: Commit**

```bash
git add harness/src/renderer/login.ts harness/src/renderer/components/LoginScreen.tsx harness/src/renderer/index.tsx harness/src/renderer/App.tsx harness/src/renderer/components/SettingsPage.tsx harness/bench/test/desktop-login-screen.test.ts
git commit -m "Show the login screen until the desktop app is signed in and connected"
```

---

### Task 8: Manual acceptance (the owner runs it)

**Files:** none.

Run on the Mac, from the branch's `harness/` after `npm install`. Each line is pass/fail; a failure goes back to the task named.

- [ ] **Step 1: Fresh profile shows only the login screen**

Run: `rm -rf "$HOME/Library/Application Support/kloudlite-harness" && npm start`
Expected: the window shows "Sign in to Kloudlite" and nothing else — no session list, no cached threads (Tasks 6, 7).

- [ ] **Step 2: Sign in**

Press "Sign in with browser".
Expected: the window shows an `XXXX-XXXX` code and the URL; the default browser opens `https://dev.kloudlite.io/cli/authorize?code=…` naming the device `<hostname> (desktop)`. Approve.
Expected: "Connecting" with "creating your bench" / "starting your bench" / "bench is …" as applicable, then the main app with the bench's sessions (Tasks 2, 4, 5, 6).

- [ ] **Step 3: The credential is not plaintext and the renderer has no token**

Run: `ls -l "$HOME/Library/Application Support/kloudlite-harness/credential.bin" && grep -c eyJ "$HOME/Library/Application Support/kloudlite-harness/credential.bin"`
Expected: mode `-rw-------`; grep count `0`. In the app's devtools console, `JSON.stringify(await window.harness.auth.status())` contains no `token` field (Task 1, 6).

- [ ] **Step 4: Relaunch reconnects without asking**

Quit (⌘Q) and `npm start` again.
Expected: straight to "Connecting" then the app; no login screen (Task 3).

- [ ] **Step 5: Second instance focuses the first**

With the app open, run `npx electron .` from `harness/` in another terminal.
Expected: the second process exits and the existing window comes to the front (Task 6).

- [ ] **Step 6: Waking a stopped bench**

Stop the bench from another terminal (`kl-connect bench stop` or `POST /v1/bench/stop`), quit and relaunch the app.
Expected: "starting your bench", then connected (Task 4).

- [ ] **Step 7: Sign out**

Account → Sign Out (menu), or Settings → Account → Sign out.
Expected: the login screen; the login is gone from the web's CLI token list; `credential.bin` is gone (Tasks 3, 6).

- [ ] **Step 8: Revoke from the web signs the app out**

Sign in again, then revoke the `<hostname> (desktop)` login in the web's token list, and quit and relaunch.
Expected: the login screen with "signed out: expired or revoked". With the app left running instead, the next new bench connection (for example opening another session) returns it to the login screen with the same reason (Tasks 3, 5).

- [ ] **Step 9: Unreachable API keeps the login**

Sign in, quit, turn off networking, relaunch.
Expected: "can't reach Kloudlite" with Retry; turn networking back on, press Retry, and the app connects without a new login (Task 3).

- [ ] **Step 10: HARNESS_BENCH still needs a login**

Sign out, then `HARNESS_BENCH=http://127.0.0.1:<port of a hand-started kl-connect bench> npm start`.
Expected: the login screen first; after sign-in, connected to that address with no bench create/start calls (Task 6).
