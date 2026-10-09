//! One space poller per bench (not one per TUI): every 5 s, after every turn and after a platform
//! tool. Emits only when the view changes. A failed read re-emits the last good view with `error`
//! set, so a TUI keeps its list and focus through a slow pod read instead of emptying.
import type { SpaceView } from "./index";

export class SpaceWatch {
  #timer?: ReturnType<typeof setInterval>;
  #inflight = false;
  #last?: SpaceView;
  #sent = "";
  constructor(private readonly read: () => Promise<SpaceView>, private readonly emit: (v: SpaceView) => void, private readonly everyMs = 5000) {}

  start(): void {
    if (this.#timer) return;
    this.poke();
    this.#timer = setInterval(() => this.poke(), this.everyMs);
  }
  stop(): void {
    clearInterval(this.#timer);
    this.#timer = undefined;
  }
  last(): SpaceView | undefined {
    return this.#last;
  }
  poke(): void {
    if (this.#inflight) return; // pod reads can outlast the beat; never stack them
    this.#inflight = true;
    this.read()
      .then((v) => {
        this.#last = v;
        this.#send(v);
      })
      .catch((e: any) => {
        const error = String(e?.message ?? e).slice(0, 200);
        this.#send(this.#last ? { ...this.#last, error } : { available: false, error, user: "", workspaces: [], environments: [] });
      })
      .finally(() => (this.#inflight = false));
  }
  #send(v: SpaceView): void {
    const s = JSON.stringify(v);
    if (s === this.#sent) return;
    this.#sent = s;
    this.emit(v);
  }
}
