//! Cards: the daemon's pending questions to the person (permission and `question` asks).
//!
//! Every connection hears `ask` and `ask_resolved`; each TUI shows a card only for its active key.
//! First answer wins; a late answer is ignored. An ask ends one of three ways: answered, its
//! signal aborted (turn interrupted), or its key withdrawn (agent disposed, workspace deleted) —
//! the last two resolve with the caller's fallback (`reject` for a permission).
import type { Ask, BenchEvent } from "./index";

type Pending = { ask: Ask; resolve: (choice: string) => void };

export class Cards {
  #pending = new Map<string, Pending>();
  #n = 0;
  constructor(private readonly emit: (e: BenchEvent) => void) {}

  ask(a: Omit<Ask, "id">, signal: AbortSignal, fallback: string): Promise<string> {
    const ask: Ask = { ...a, id: `a${Date.now().toString(36)}${(++this.#n).toString(36)}` };
    return new Promise((resolve) => {
      const done = (choice: string) => {
        if (!this.#pending.delete(ask.id)) return;
        signal.removeEventListener("abort", onAbort);
        this.emit({ type: "ask_resolved", id: ask.id });
        resolve(choice);
      };
      const onAbort = () => done(fallback);
      this.#pending.set(ask.id, { ask, resolve: done });
      if (signal.aborted) return onAbort();
      signal.addEventListener("abort", onAbort);
      this.emit({ type: "ask", ask });
    });
  }

  answer(id: string, choice: string): void {
    this.#pending.get(id)?.resolve(choice);
  }

  /** Withdraw every ask of a key: its agent is gone, nobody waits on the answer. */
  withdrawKey(key: string, fallback = "reject"): void {
    for (const p of [...this.#pending.values()]) if (p.ask.key === key) p.resolve(fallback);
  }

  pending(): Ask[] {
    return [...this.#pending.values()].map((p) => p.ask);
  }
}
