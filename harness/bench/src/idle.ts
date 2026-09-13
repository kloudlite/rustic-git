/**
 * The bench's idle clock. Idle means no open WebSocket (a device watching
 * /events or a session's /rpc) and nothing running; plain HTTP never counts,
 * or a health probe would keep an abandoned bench up forever. The moment is
 * kept from the first look that found it idle, and any client or work clears it.
 */
export class Idle {
  private busy: () => boolean;
  private clients = 0;
  private since: number | null = null;

  constructor(busy: () => boolean) {
    this.busy = busy;
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
    if (this.clients > 0 || this.busy()) this.since = null;
    else this.since ??= Date.now();
  }

  state(): { clients: number; busy: boolean; idleSince: number | null } {
    this.check();
    return { clients: this.clients, busy: this.busy(), idleSince: this.since };
  }
}
