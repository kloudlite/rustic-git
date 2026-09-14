/**
 * The person's own bench, as `kl-connect bench` reaches it (`bins/kl-connect/src/api.rs`):
 * `POST /v1/bench/session` answers 201 with a single-use session, 202 while it wakes, 409 when
 * stopped, 404 when there is none. `ensureBench` is the Connect step (create/start/wait, once);
 * `mintSession` is what every tunnel connection calls and never allocates anything itself.
 *
 * The gateway URL in a session answer is server-controlled but still untrusted client input (a
 * compromised or misconfigured API could point us at any WebSocket host); it is always
 * `wss://ws-{region}.khost.dev/tunnel/{id}` (`crates/workspaces/src/api/workspaces/mod.rs`
 * `gateway_url`/`GATEWAY_DOMAIN` — a fixed platform domain, unrelated to the configured API
 * host), so `mintSession` refuses anything else before returning it. `allowLocalGateway` exists
 * only for tests that stand up a local `ws://127.0.0.1` gateway.
 */
export type Session = { id: string; token: string; gateway: string; expires_at: string };

export class Expired extends Error {
  constructor() {
    super("your login has expired or was revoked");
    this.name = "Expired";
  }
}

export class BadGateway extends Error {
  constructor(reason: string) {
    super(`Kloudlite answered with a gateway address we won't connect to (${reason})`);
    this.name = "BadGateway";
  }
}

const GATEWAY_DOMAIN = "khost.dev";

function validateGateway(raw: string, allowLocal: boolean): string {
  let u: URL;
  try {
    u = new URL(raw);
  } catch {
    throw new BadGateway("not a URL");
  }
  const isLocal = allowLocal && u.protocol === "ws:" && (u.hostname === "127.0.0.1" || u.hostname === "localhost");
  if (!isLocal) {
    if (u.protocol !== "wss:") throw new BadGateway("scheme");
    if (u.hostname !== GATEWAY_DOMAIN && !u.hostname.endsWith(`.${GATEWAY_DOMAIN}`)) throw new BadGateway("host");
  }
  if (!u.pathname.startsWith("/tunnel/")) throw new BadGateway("path");
  return raw;
}

const STOPPED = "bench is stopped; start it";
type Answer = { status: number; body: { error?: string; state?: string } & Partial<Session> };

type Opts = { sleepMs?: number; waitMs?: number; signal?: AbortSignal; allowLocalGateway?: boolean };

async function call(api: string, token: string, method: string, path: string, body?: unknown, signal?: AbortSignal): Promise<Answer> {
  const r = await fetch(api + path, {
    method,
    headers: { authorization: `Bearer ${token}`, ...(body === undefined ? {} : { "content-type": "application/json" }) },
    body: body === undefined ? undefined : JSON.stringify(body),
    redirect: "error",
    signal,
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

function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) return reject(signal.reason ?? new DOMException("Aborted", "AbortError"));
    const t = setTimeout(resolve, ms);
    signal?.addEventListener("abort", () => (clearTimeout(t), reject(signal.reason ?? new DOMException("Aborted", "AbortError"))), { once: true });
  });
}

export async function ensureBench(api: string, token: string, step: (s: string) => void, opts: Opts = {}) {
  const deadline = Date.now() + (opts.waitMs ?? 90_000);
  let created = false;
  let started = false;
  for (;;) {
    const a = await call(api, token, "POST", "/v1/bench/session", undefined, opts.signal);
    if (a.status >= 200 && a.status < 300 && a.status !== 202) return;
    if (a.status === 404 && !created) {
      created = true;
      step("creating your bench");
      const c = await call(api, token, "POST", "/v1/bench", {}, opts.signal);
      if (c.status >= 300) throw refused(c);
      continue;
    }
    if (a.status === 409 && a.body.error === STOPPED && !started) {
      started = true;
      step("starting your bench");
      const s = await call(api, token, "POST", "/v1/bench/start", undefined, opts.signal);
      if (s.status >= 300) throw refused(s);
      continue;
    }
    if (a.status !== 202) throw refused(a);
    step(`bench is ${a.body.state ?? "starting"}`);
    if (Date.now() >= deadline) throw new Error("your bench did not start within 90 s");
    await sleep(opts.sleepMs ?? 1000, opts.signal);
  }
}

export async function mintSession(api: string, token: string, opts: Opts = {}): Promise<Session> {
  const deadline = Date.now() + (opts.waitMs ?? 90_000);
  for (;;) {
    const a = await call(api, token, "POST", "/v1/bench/session", undefined, opts.signal);
    if (a.status === 202) {
      if (Date.now() >= deadline) throw new Error("your bench did not start within 90 s");
      await sleep(opts.sleepMs ?? 1000, opts.signal);
      continue;
    }
    if (a.status >= 300 || !a.body.token || !a.body.gateway) throw refused(a);
    validateGateway(a.body.gateway, opts.allowLocalGateway ?? false);
    return a.body as Session;
  }
}
