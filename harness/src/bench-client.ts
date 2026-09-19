import fs from "node:fs";
import http from "node:http";
import https from "node:https";
import WebSocket from "ws";

/**
 * The laptop side of a remote bench: every device is a view of one
 * harness-bench, reached at HARNESS_BENCH (the tunnel's local end). Sessions
 * stream over one WebSocket each; list, exchange, task and process changes
 * over /events. What was last seen is cached so a disconnected harness still
 * reads, and nothing typed while disconnected is ever queued here. The cache
 * holds rows the bench served and nothing else — never a credential.
 */
export type Emit = (ev: Record<string, unknown> & { type: string; pi?: string }) => void;
// `total` is the bench's own message count: once a session outgrows the kept
// tail, the tail's length is no longer the index `after=` needs.
type Cache = { base: string; sessions: unknown[]; exchanges: unknown[]; messages: Record<string, { total: number; tail: unknown[] }> };
/** What a window asks for when it opens a thread it has never seen. */
const OPEN_WITH = 60;
/**
 * A session shorter than this is read WHOLE on open, so nothing of it is missing. Past it the tail
 * stands on its own: a thread of thousands would read every message back through pi before the
 * window could draw. ponytail: no scroll-up paging yet — the upgrade is a `before=` page fetched
 * when the person reaches the top.
 */
const FULL_UNDER = 2000;
const KEEP_MESSAGES = 200;
const KEEP_EXCHANGES = 500;
export type PtySession = { name: string; windows: number; attached: number; created: number };
export type BenchAuthHeaders = { authorization: string; "x-kl-owner": string; "x-kl-login": string };
const OFFLINE = "not connected to the bench; nothing was sent";

export class BenchResponseError extends Error {
  readonly status: number;
  readonly code?: string;
  readonly expectedRevision?: number;
  readonly actualRevision?: number;
  readonly details?: Record<string, unknown>;
  constructor(status: number, body: unknown) {
    const detail = body && typeof body === "object" && !Array.isArray(body) && "error" in body ? (body as { error?: unknown }).error : undefined;
    const structured = detail && typeof detail === "object" && !Array.isArray(detail) ? detail as Record<string, unknown> : undefined;
    super(typeof structured?.message === "string" ? structured.message : typeof detail === "string" ? detail : `bench answered ${status}`);
    this.name = "BenchResponseError";
    this.status = status;
    this.code = typeof structured?.code === "string" ? structured.code : undefined;
    this.expectedRevision = typeof structured?.expectedRevision === "number" ? structured.expectedRevision : undefined;
    this.actualRevision = typeof structured?.actualRevision === "number" ? structured.actualRevision : undefined;
    this.details = structured ? Object.fromEntries(Object.entries(structured).filter(([key]) => !["code", "message"].includes(key))) : undefined;
  }
}

/**
 * One connection, many requests. Each accepted TCP connection to the tunnel opens its OWN gateway
 * WebSocket — a TLS handshake to the edge, 120–190 ms of round trip from here — so a `fetch` per
 * request paid that every time while the bench itself answered in 1–50 ms (measured, 2026-09-17).
 * A keep-alive agent turns hundreds of requests into one handshake.
 *
 * `http.request` rather than `fetch`: Node's fetch takes a `dispatcher`, but that needs undici as a
 * real dependency, and the agent below is four lines and no dependency at all.
 */
const KEEP_ALIVE = { keepAlive: true, keepAliveMsecs: 15_000, maxSockets: 4, maxFreeSockets: 4, timeout: 0 };

export class BenchClient {
  private base: string;
  private emit: Emit;
  private cacheFile: string;
  private cache: Cache;
  private events?: WebSocket;
  private agents: Record<string, http.Agent> = {};
  private up = false;
  private closed = false;
  private backoff = 1000;
  private timer?: NodeJS.Timeout;
  /** Keeps `/events` alive past the edge's ~100 s idle reap; cleared whenever the socket goes. */
  private ping?: NodeJS.Timeout;
  private sockets = new Map<string, WebSocket>();
  private waiting = new Map<string, { w: WebSocket; done: (r: Record<string, unknown>) => void }>();
  private seq = 0;

  private nonce?: string;
  private requestTimeoutMs: number;

  // `cacheKey` defaults to the address; the desktop app passes the username, because the
  // tunnel's port is new every launch and would otherwise empty the cache every time.
  // `nonce` is the in-process tunnel's per-launch secret, sent on every request and upgrade;
  // unset for a HARNESS_BENCH address, which has no such gate.
  constructor(base: string, emit: Emit, cacheFile: string, cacheKey = base, nonce?: string, requestTimeoutMs = 30_000) {
    this.base = base.replace(/\/$/, "");
    this.emit = emit;
    this.cacheFile = cacheFile;
    this.nonce = nonce;
    this.requestTimeoutMs = requestTimeoutMs;
    const empty: Cache = { base: cacheKey, sessions: [], exchanges: [], messages: {} };
    try {
      const c = JSON.parse(fs.readFileSync(cacheFile, "utf8")) as Cache;
      // Keyed by whose bench it is: another person's list is not this one's.
      this.cache = c.base === cacheKey ? c : empty;
    } catch {
      this.cache = empty;
    }
  }

  connected(): boolean {
    return this.up;
  }
  cached() {
    return { sessions: this.cache.sessions, exchanges: this.cache.exchanges };
  }
  private save() {
    try {
      fs.writeFileSync(`${this.cacheFile}.tmp`, JSON.stringify(this.cache));
      fs.renameSync(`${this.cacheFile}.tmp`, this.cacheFile);
    } catch {
      /* a cache that cannot be written is only a slower start */
    }
  }
  private setUp(v: boolean) {
    if (this.up === v) return;
    this.up = v;
    this.emit({ type: "bench", connected: v });
  }
  /** One request over a pooled connection; the agent is per-scheme and made once. */
  private send(method: string, p: string, body?: unknown, headers: Record<string, string> = {}): Promise<{ status: number; body: string }> {
    const url = new URL(this.base + p);
    const mod = url.protocol === "https:" ? https : http;
    this.agents[url.protocol] ??= url.protocol === "https:" ? new https.Agent(KEEP_ALIVE) : new http.Agent(KEEP_ALIVE);
    const payload = body === undefined ? undefined : Buffer.from(JSON.stringify(body));
    return new Promise((resolve, reject) => {
      let settled = false;
      let timer: NodeJS.Timeout | undefined;
      const finish = (value: { status: number; body: string } | Error) => {
        if (settled) return;
        settled = true;
        if (timer) clearTimeout(timer);
        if (value instanceof Error) reject(value);
        else resolve(value);
      };
      const req = mod.request(
        url,
        {
          method,
          agent: this.agents[url.protocol],
          headers: { ...this.tunnel(), ...headers, ...(payload ? { "content-type": "application/json", "content-length": String(payload.length) } : {}) },
        },
        (res) => {
          let text = "";
          res.setEncoding("utf8");
          res.on("data", (c: string) => (text += c));
          res.once("end", () => finish({ status: res.statusCode ?? 0, body: text }));
          res.once("aborted", () => finish(new Error("bench response aborted")));
          res.once("error", (error) => finish(error));
        },
      );
      req.once("error", (error) => finish(error));
      timer = setTimeout(() => {
        const error = new Error(`bench request timed out after ${this.requestTimeoutMs}ms`);
        req.destroy(error);
        finish(error);
      }, this.requestTimeoutMs);
      timer.unref?.();
      if (payload) req.write(payload);
      req.end();
    });
  }

  /** Open the pool before anything needs it: the first request otherwise pays the handshake. */
  private warm(): void {
    for (let i = 0; i < 2; i++) void this.send("GET", "/healthz").catch(() => undefined);
  }

  private tunnel(): Record<string, string> {
    return this.nonce ? { "x-kl-tunnel": this.nonce } : {};
  }
  private ws(p: string) {
    return new WebSocket(this.base.replace(/^http/, "ws") + p, { headers: this.tunnel() });
  }

  /**
   * One WebSocket per shell, through the same tunnel and headers as every other
   * bench socket. The caller owns it: this class neither tracks nor closes it —
   * a shell's life is its socket's, and /events dropping kills it anyway.
   */
  pty(scope: string, session?: string): WebSocket {
    if (!this.up) throw new Error(OFFLINE);
    // No session name: the socket IS the shell, in the pod's `shell` sidecar (spec §2.3).
    return this.ws(`/pty?scope=${encodeURIComponent(scope)}`);
  }


  /**
   * A workspace's file-system watch, owned by the caller exactly as `pty` is: the bench starts the
   * watch in the pod when this opens and stops it when it closes.
   */
  watch(scope: string): WebSocket {
    if (!this.up) throw new Error(OFFLINE);
    return this.ws(`/watch?scope=${encodeURIComponent(scope)}`);
  }

  start(): void {
    if (this.closed) return;
    const w = this.ws("/events");
    this.events = w;
    w.on("open", async () => {
      this.backoff = 1000;
      this.setUp(true);
      // The Cloudflare edge reaps a WebSocket after ~100 s with no CLIENT→server traffic
      // (`bins/gateway/src/tunnel.rs:23`). `/events` is server→client only, so it was being cut
      // every 79–136 s — about 25 times an hour — and every cut cost a token mint and a fresh
      // handshake. sshd's ClientAliveInterval covers ssh; nothing covered the bench.
      clearInterval(this.ping);
      this.ping = setInterval(() => {
        try {
          w.ping();
        } catch {
          /* the close handler is what reconnects; a failed ping is not its own error */
        }
      }, 30_000);
      this.ping.unref?.();
      // A reconnect means new sockets: warm them before the resync asks for everything at once.
      this.warm();
      try {
        await this.rest("GET", "/sessions");
      } catch {
        /* the list arrives with the next change */
      }
      this.emit({ type: "bench:resync" });
    });
    w.on("message", (d) => {
      let ev: Record<string, unknown> & { type: string; pi?: string };
      try {
        ev = JSON.parse(d.toString());
      } catch {
        return;
      }
      // A session this device holds a socket for streams there; any other session's
      // live turn streams here, so a second device sees it before it ever sends.
      // Responses answer another device's command: never this one's.
      if (ev.pi && (this.sockets.has(ev.pi) || ev.type === "response")) return;
      if (ev.type === "sessions" && Array.isArray(ev.sessions)) {
        this.cache.sessions = ev.sessions;
        this.save();
      }
      // A session that started afresh: what this window cached about it is a different session's.
      if (ev.type === "cleared" && typeof ev.session === "string") {
        delete this.cache.messages[ev.session];
        this.save();
      }
      if (ev.type === "exchange" && ev.row) {
        this.cache.exchanges = [...this.cache.exchanges, ev.row].slice(-KEEP_EXCHANGES);
        this.save();
      }
      this.emit(ev);
    });
    w.on("close", () => {
      if (this.events !== w) return;
      clearInterval(this.ping);
      this.ping = undefined;
      this.setUp(false);
      for (const s of this.sockets.values()) s.terminate();
      this.sockets.clear();
      for (const [id, x] of this.waiting) x.done({ type: "response", id, success: false, error: "the bench connection dropped; reconnecting" });
      this.waiting.clear();
      if (this.closed) return;
      this.timer = setTimeout(() => this.start(), this.backoff);
      this.backoff = Math.min(this.backoff * 2, 15_000);
    });
    w.on("error", () => undefined); // close follows
  }

  close(): void {
    this.closed = true;
    clearTimeout(this.timer);
    clearInterval(this.ping);
    this.ping = undefined;
    this.events?.terminate();
    for (const s of this.sockets.values()) s.terminate();
    this.sockets.clear();
  }

  async rest<T = unknown>(method: string, p: string, body?: unknown): Promise<T> {
    return this.request(method, p, body);
  }

  private async request<T = unknown>(method: string, p: string, body?: unknown, headers?: Record<string, string>): Promise<T> {
    const text = await this.send(method, p, body, headers);
    let data: unknown = null;
    try {
      data = text.body ? JSON.parse(text.body) : null;
    } catch {
      data = null;
    }
    // Every 401 names its route (never a token): the desktop log had no 401 line at all, so the
    // source of a sign-out loop could not be read off it (coordinator, 2026-09-18).
    if (text.status === 401) {
      console.error(`auth: 401 from bench ${method} ${p}`);
      const error = new Error("your login has expired or was revoked") as Error & { route: string };
      error.name = "Expired";
      error.route = `${method} ${p}`;
      throw error;
    }
    if (text.status >= 400) throw new BenchResponseError(text.status, data);
    if (method === "GET" && p === "/sessions") {
      this.cache.sessions = data as unknown[];
      this.save();
    }
    return data as T;
  }

  operationSnapshot<T>(operationId: string, auth: BenchAuthHeaders): Promise<T> {
    return this.request("GET", `/operations/${encodeURIComponent(operationId)}`, undefined, auth);
  }

  operationEvents<T>(operationId: string, after: string | undefined, limit: number | undefined, auth: BenchAuthHeaders): Promise<T> {
    const query = new URLSearchParams();
    if (after !== undefined) query.set("after", after);
    if (limit !== undefined) query.set("limit", String(limit));
    const suffix = query.size ? `?${query}` : "";
    return this.request("GET", `/operations/${encodeURIComponent(operationId)}/events${suffix}`, undefined, auth);
  }

  cancelOperation<T>(operationId: string, expectedRevision: number, auth: BenchAuthHeaders): Promise<T> {
    return this.request("POST", `/operations/${encodeURIComponent(operationId)}/cancel`, { expectedRevision }, auth);
  }

  recordOperationDecision<T>(operationId: string, decisionId: string, body: unknown, auth: BenchAuthHeaders): Promise<T> {
    return this.request("POST", `/operations/${encodeURIComponent(operationId)}/decisions/${encodeURIComponent(decisionId)}`, body, auth);
  }

  provideOperationInput<T>(operationId: string, decisionId: string, expectedRevision: number, inputs: Record<string, unknown>, auth: BenchAuthHeaders): Promise<T> {
    return this.request("POST", `/operations/${encodeURIComponent(operationId)}/input`, { decisionId, expectedRevision, inputs }, auth);
  }

  private socket(session: string): Promise<WebSocket> {
    const have = this.sockets.get(session);
    if (have?.readyState === WebSocket.OPEN) return Promise.resolve(have);
    have?.terminate();
    const w = this.ws(`/sessions/${encodeURIComponent(session)}/rpc`);
    this.sockets.set(session, w);
    w.on("message", (d) => {
      let ev: Record<string, unknown> & { type: string; id?: string };
      try {
        ev = JSON.parse(d.toString());
      } catch {
        return;
      }
      const x = ev.type === "response" && ev.id ? this.waiting.get(ev.id) : undefined;
      if (x) {
        this.waiting.delete(ev.id!);
        x.done(ev);
        return; // the awaited answer, not a stream event
      }
      this.emit({ ...ev, pi: session });
    });
    // A session socket can drop on its own (a proxy's idle timeout) while
    // /events stays up; whatever it was carrying must be answered, not hang.
    const drop = () => {
      if (this.sockets.get(session) === w) this.sockets.delete(session);
      for (const [id, x] of this.waiting) {
        if (x.w !== w) continue;
        this.waiting.delete(id);
        x.done({ type: "response", id, success: false, error: "bench connection closed" });
      }
    };
    w.on("close", drop);
    w.on("error", drop);
    return new Promise((resolve, reject) => {
      w.once("open", () => resolve(w));
      w.once("close", () => reject(new Error(OFFLINE)));
    });
  }

  async rpc(session: string, cmd: Record<string, unknown>): Promise<Record<string, unknown>> {
    if (!this.up) throw new Error(OFFLINE);
    const w = await this.socket(session);
    // /events may have dropped, or this socket closed, while it opened.
    if (!this.up) throw new Error(OFFLINE);
    const id = `h${++this.seq}`;
    if (w.readyState !== WebSocket.OPEN) return { type: "response", id, success: false, error: "bench connection closed" };
    return new Promise((done) => {
      this.waiting.set(id, { w, done });
      w.send(JSON.stringify({ ...cmd, id }));
    });
  }

  async messages(session: string): Promise<unknown[]> {
    const have = this.cache.messages[session] ?? { total: 0, tail: [] };
    if (!this.up) return have.tail;
    const at = `/sessions/${encodeURIComponent(session)}/messages`;
    try {
      // With nothing cached, ask for the newest page FIRST so the window can draw, then — unless
      // the session is genuinely long — fetch the rest and keep the whole thing. Opening with a
      // tail and never filling it in is what hid the owner's first prompt behind eleven tool calls:
      // the session looked like it began mid-conversation, because the start had never been asked
      // for (owner: "session is not fully visible").
      const q = have.total ? `after=${have.total}` : `tail=${OPEN_WITH}`;
      let r = await this.rest<{ messages: unknown[]; total: number }>("GET", `${at}?${q}`);
      if (!have.total && r.total > r.messages.length && r.total <= FULL_UNDER)
        r = await this.rest<{ messages: unknown[]; total: number }>("GET", at).catch(() => r);
      // A shorter history than the cache means it was cleared or compacted:
      // start over rather than append to a history that no longer exists.
      const all = r.total < have.total ? (await this.rest<{ messages: unknown[] }>("GET", at)).messages : [...have.tail, ...r.messages];
      this.cache.messages[session] = { total: r.total, tail: all.slice(-KEEP_MESSAGES) };
      this.save();
      return all;
    } catch {
      return have.tail;
    }
  }
}
