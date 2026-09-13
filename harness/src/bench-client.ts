import fs from "node:fs";
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
const KEEP_MESSAGES = 200;
const KEEP_EXCHANGES = 500;
const OFFLINE = "not connected to the bench; nothing was sent";

export class BenchClient {
  private base: string;
  private emit: Emit;
  private cacheFile: string;
  private cache: Cache;
  private events?: WebSocket;
  private up = false;
  private closed = false;
  private backoff = 1000;
  private timer?: NodeJS.Timeout;
  private sockets = new Map<string, WebSocket>();
  private waiting = new Map<string, (r: Record<string, unknown>) => void>();
  private seq = 0;

  constructor(base: string, emit: Emit, cacheFile: string) {
    this.base = base.replace(/\/$/, "");
    this.emit = emit;
    this.cacheFile = cacheFile;
    const empty: Cache = { base: this.base, sessions: [], exchanges: [], messages: {} };
    try {
      const c = JSON.parse(fs.readFileSync(cacheFile, "utf8")) as Cache;
      // Keyed by the bench's address: another bench's list is not this one's.
      this.cache = c.base === this.base ? c : empty;
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
  private ws(p: string) {
    return new WebSocket(this.base.replace(/^http/, "ws") + p);
  }

  start(): void {
    if (this.closed) return;
    const w = this.ws("/events");
    this.events = w;
    w.on("open", async () => {
      this.backoff = 1000;
      this.setUp(true);
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
      if (ev.pi) return; // session events arrive on the session's own socket
      if (ev.type === "sessions" && Array.isArray(ev.sessions)) {
        this.cache.sessions = ev.sessions;
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
      this.setUp(false);
      for (const s of this.sockets.values()) s.terminate();
      this.sockets.clear();
      for (const [id, r] of this.waiting) r({ type: "response", id, success: false, error: "the bench connection dropped; reconnecting" });
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
    this.events?.terminate();
    for (const s of this.sockets.values()) s.terminate();
    this.sockets.clear();
  }

  async rest<T = unknown>(method: string, p: string, body?: unknown): Promise<T> {
    const r = await fetch(this.base + p, {
      method,
      headers: body === undefined ? {} : { "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await r.text();
    let data: unknown = null;
    try {
      data = text ? JSON.parse(text) : null;
    } catch {
      data = null;
    }
    if (r.status >= 400) throw new Error((data as { error?: string } | null)?.error ?? `bench answered ${r.status}`);
    if (method === "GET" && p === "/sessions") {
      this.cache.sessions = data as unknown[];
      this.save();
    }
    return data as T;
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
      const done = ev.type === "response" && ev.id ? this.waiting.get(ev.id) : undefined;
      if (done) {
        this.waiting.delete(ev.id!);
        done(ev);
        return; // the awaited answer, not a stream event
      }
      this.emit({ ...ev, pi: session });
    });
    w.on("close", () => {
      if (this.sockets.get(session) === w) this.sockets.delete(session);
    });
    w.on("error", () => undefined);
    return new Promise((resolve, reject) => {
      w.once("open", () => resolve(w));
      w.once("close", () => reject(new Error(OFFLINE)));
    });
  }

  async rpc(session: string, cmd: Record<string, unknown>): Promise<Record<string, unknown>> {
    if (!this.up) throw new Error(OFFLINE);
    const w = await this.socket(session);
    const id = `h${++this.seq}`;
    return new Promise((resolve) => {
      this.waiting.set(id, resolve);
      w.send(JSON.stringify({ ...cmd, id }));
    });
  }

  async messages(session: string): Promise<unknown[]> {
    const have = this.cache.messages[session] ?? { total: 0, tail: [] };
    if (!this.up) return have.tail;
    const at = `/sessions/${encodeURIComponent(session)}/messages`;
    try {
      const r = await this.rest<{ messages: unknown[]; total: number }>("GET", `${at}?after=${have.total}`);
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
