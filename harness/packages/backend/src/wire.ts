//! JSON lines over a byte stream, symmetric: either side may request, the other replies. Frames:
//! `{id, op, args}` request, `{re, ok, value | error}` reply, `{cancel}` aborts the receiver's
//! handler, `{ev, key, event}` one-way event. Each side numbers its own requests, so a reply's
//! `re` always names one of the receiver's. Non-JSON lines are skipped: a stray write on the
//! host's stdout must not kill the session (serve.ts also routes console to stderr).
export const PROTOCOL = 1;

type Handler = (args: any, signal: AbortSignal) => Promise<unknown>;
type Listener = (ev: string, key: string, event: unknown) => void;

export function encode(frame: unknown): string {
  return (
    JSON.stringify(frame, (_k, v) => (v instanceof Error ? { name: v.name, message: v.message } : v)) + "\n"
  );
}

export class Peer {
  #next = 1;
  #pending = new Map<number, { resolve: (v: any) => void; reject: (e: Error) => void }>();
  #running = new Map<number, AbortController>();
  #handlers = new Map<string, Handler>();
  #listeners = new Set<Listener>();
  #decoder = new TextDecoder();
  #buf = "";
  #closed = false;

  constructor(private readonly write: (line: string) => void) {}

  get idle() {
    return this.#running.size === 0;
  }

  get closed() {
    return this.#closed;
  }

  handle(op: string, fn: Handler) {
    this.#handlers.set(op, fn);
  }

  onEvent(cb: Listener): () => void {
    this.#listeners.add(cb);
    return () => this.#listeners.delete(cb);
  }

  request<T>(op: string, args: unknown, signal?: AbortSignal): Promise<T> {
    if (this.#closed) return Promise.reject(new Error("disconnected"));
    const id = this.#next++;
    return new Promise<T>((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
      signal?.addEventListener(
        "abort",
        () => {
          if (!this.#pending.delete(id)) return;
          this.#send({ cancel: id });
          reject(new Error("aborted"));
        },
        { once: true },
      );
      this.#send({ id, op, args });
    });
  }

  emit(ev: string, key: string, event: unknown) {
    this.#send({ ev, key, event });
  }

  feed(chunk: Uint8Array) {
    this.#buf += this.#decoder.decode(chunk, { stream: true });
    let nl: number;
    while ((nl = this.#buf.indexOf("\n")) >= 0) {
      const line = this.#buf.slice(0, nl);
      this.#buf = this.#buf.slice(nl + 1);
      let frame: any;
      try {
        frame = JSON.parse(line);
      } catch {
        continue;
      }
      this.#dispatch(frame);
    }
  }

  close() {
    if (this.#closed) return;
    this.#closed = true;
    for (const p of this.#pending.values()) p.reject(new Error("disconnected"));
    this.#pending.clear();
    for (const ac of this.#running.values()) ac.abort();
    this.#running.clear();
  }

  #send(frame: unknown) {
    if (!this.#closed) this.write(encode(frame));
  }

  #dispatch(f: any) {
    if (typeof f?.op === "string" && typeof f.id === "number") {
      const h = this.#handlers.get(f.op);
      if (!h) return this.#send({ re: f.id, ok: false, error: { name: "Error", message: `unknown op: ${f.op}` } });
      const ac = new AbortController();
      this.#running.set(f.id, ac);
      Promise.resolve()
        .then(() => h(f.args, ac.signal))
        .then(
          (value) => this.#send({ re: f.id, ok: true, value }),
          (error) => this.#send({ re: f.id, ok: false, error }),
        )
        .finally(() => this.#running.delete(f.id));
    } else if (typeof f?.re === "number") {
      const p = this.#pending.get(f.re);
      if (!p) return; // cancelled: the late answer has nobody to go to
      this.#pending.delete(f.re);
      if (f.ok) p.resolve(f.value);
      else p.reject(Object.assign(new Error(f.error?.message ?? "error"), { name: f.error?.name ?? "Error" }));
    } else if (typeof f?.cancel === "number") {
      this.#running.get(f.cancel)?.abort();
    } else if (typeof f?.ev === "string") {
      for (const l of this.#listeners) l(f.ev, f.key, f.event);
    }
  }
}
