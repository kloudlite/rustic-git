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
