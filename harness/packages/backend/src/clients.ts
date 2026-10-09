//! Who answers a session's permission cards and TUI-owned tools. The agent lives in the bench
//! daemon and outlives the client that opened it, so those callbacks cannot be bound to the first
//! opener: they route by session key to the NEWEST connected view that can serve the call. With
//! no client connected the call WAITS (a turn that needs a card pauses until the person
//! reconnects, instead of dying with the client). A client whose connection dies mid-call fails
//! with Error("disconnected") (wire.ts Peer.close); that one is skipped and the next is tried.
//! Any other error from the call propagates.
import type { SessionOpts } from "./index.ts";

export type Client = { permission?: SessionOpts["permission"]; tools: Map<string, (input: unknown) => Promise<string>> };

type Entry = { key: string; c: Client };

export class Clients {
  #list: Entry[] = [];
  #waiters = new Set<() => void>();

  /** Register a view's client for `key`; newest wins. Returns the unregister. */
  add(key: string, c: Client): () => void {
    const e = { key, c };
    this.#list.push(e);
    for (const w of [...this.#waiters]) w();
    return () => {
      this.#list = this.#list.filter((x) => x !== e);
    };
  }

  /** Run `f` on the newest client of `key` that `has`; none yet = wait until one is added (or the
   * signal aborts → reject Error("aborted")). */
  async route<T>(key: string, has: (c: Client) => boolean, f: (c: Client) => Promise<T>, signal?: AbortSignal): Promise<T> {
    const tried = new Set<Entry>();
    for (;;) {
      if (signal?.aborted) throw new Error("aborted");
      const e = [...this.#list].reverse().find((x) => x.key === key && !tried.has(x) && has(x.c));
      if (e) {
        try {
          return await f(e.c);
        } catch (err) {
          if (!(err instanceof Error) || err.message !== "disconnected") throw err;
          tried.add(e);
          continue;
        }
      }
      await new Promise<void>((resolve, reject) => {
        const done = () => {
          this.#waiters.delete(done);
          signal?.removeEventListener("abort", onAbort);
          resolve();
        };
        const onAbort = () => {
          this.#waiters.delete(done);
          reject(new Error("aborted"));
        };
        this.#waiters.add(done);
        signal?.addEventListener("abort", onAbort, { once: true });
      });
    }
  }
}
