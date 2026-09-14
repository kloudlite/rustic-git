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
