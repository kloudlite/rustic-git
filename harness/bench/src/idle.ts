import fs from "node:fs";
import path from "node:path";

/**
 * The bench's idle clock. Idle means no open WebSocket (a device watching
 * /events or a session's /rpc) and nothing running; plain HTTP never counts,
 * or a health probe would keep an abandoned bench up forever. The moment is
 * kept from the first look that found it idle, and any client or work clears it.
 *
 * Idleness is SIGNALLED, never acted on: the bench is a container of a workspace
 * pod whose restartPolicy is Always, so exiting on idle would only be restarted
 * by kubelet. Instead the moment is written to `{dir}/.idle` (RFC 3339), which
 * `--ping` reads (exit 2) so the readiness probe carries it, and the agent
 * deletes the pod. Any client or work deletes the file again.
 *
 * The signal waits out `afterMs` of CONTINUOUS idleness, and the wait is here
 * rather than in the agent because the agent's only answer to an unready bench
 * is to delete the pod: a desktop tunnel reconnect drops every socket for about
 * a second, and that must cost the person nothing.
 */
export class Idle {
  private busy: () => boolean;
  private mark?: string;
  private afterMs: number;
  private now: () => number;
  private marked = false;
  private clients = 0;
  private since: number | null = null;

  /** `afterMs` is how long idleness must hold before it is signalled; tests pass `now` as a fake clock. */
  constructor(busy: () => boolean, dir?: string, afterMs = 0, now: () => number = Date.now) {
    this.busy = busy;
    this.afterMs = afterMs;
    this.now = now;
    if (dir) {
      this.mark = path.join(dir, ".idle");
      // A mark from the PREVIOUS pod is not this one's state: `marked` starts false, so nothing
      // below would ever clear it, and a woken bench would read as asleep to any file reader.
      try {
        fs.rmSync(this.mark, { force: true });
      } catch {
        /* same reasoning as the writes below */
      }
    }
    this.check();
  }

  opened(): void {
    this.clients++;
    this.check();
  }

  closed(): void {
    this.clients = Math.max(0, this.clients - 1);
    this.check();
  }

  /**
   * Re-read busy(); the server calls it on every bench event so the moment tracks work ending,
   * not the next probe, and main beats it so the signal appears without one.
   */
  check(): void {
    if (this.clients > 0 || this.busy()) this.since = null;
    else this.since ??= this.now();
    const signal = this.signal();
    const want = signal !== null;
    if (this.mark === undefined || want === this.marked) return;
    // Only the transitions touch the disk: check() runs on every bench event.
    this.marked = want;
    try {
      if (signal === null) fs.rmSync(this.mark, { force: true });
      else fs.writeFileSync(this.mark, signal);
    } catch {
      /* a read-only or vanished folder is not worth failing a request over */
    }
  }

  /** The moment idleness began, once it has held for afterMs; null while a client, work, or the wait holds it up. */
  private signal(): string | null {
    return this.since !== null && this.now() - this.since >= this.afterMs ? new Date(this.since).toISOString() : null;
  }

  /** `idle` is the RFC 3339 moment `--ping` and the agent read; absent means a client, work or the wait holds it up. */
  state(): { clients: number; busy: boolean; idleSince: number | null; idle?: string } {
    this.check();
    const idle = this.signal();
    return { clients: this.clients, busy: this.busy(), idleSince: this.since, ...(idle === null ? {} : { idle }) };
  }
}
