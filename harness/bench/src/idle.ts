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
 */
export class Idle {
  private busy: () => boolean;
  private mark?: string;
  private clients = 0;
  private since: number | null = null;

  constructor(busy: () => boolean, dir?: string) {
    this.busy = busy;
    if (dir) this.mark = path.join(dir, ".idle");
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

  /** Re-read busy(); the server calls it on every bench event so the moment tracks work ending, not the next probe. */
  check(): void {
    const was = this.since;
    if (this.clients > 0 || this.busy()) this.since = null;
    else this.since ??= Date.now();
    if (this.mark === undefined || (was === null) === (this.since === null)) return;
    // Only the transitions touch the disk: check() runs on every bench event.
    try {
      if (this.since === null) fs.rmSync(this.mark, { force: true });
      else fs.writeFileSync(this.mark, new Date(this.since).toISOString());
    } catch {
      /* a read-only or vanished folder is not worth failing a request over */
    }
  }

  /** `idle` is the RFC 3339 moment `--ping` and the agent read; absent means a client or work holds it up. */
  state(): { clients: number; busy: boolean; idleSince: number | null; idle?: string } {
    this.check();
    return { clients: this.clients, busy: this.busy(), idleSince: this.since, ...(this.since === null ? {} : { idle: new Date(this.since).toISOString() }) };
  }
}
