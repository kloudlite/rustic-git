import path from "node:path";
import { appendLine } from "./log.ts";

/**
 * A bench whose folder stops taking writes must stop taking prompts: work kept
 * only in memory is lost on the next reschedule, silently. Any failed write
 * flips this; the probe beat (main.ts, every 10 s) flips it back.
 */
export class Writable {
  private file: string;
  private onChange: (ok: boolean, reason?: string) => void;
  private why?: string;
  constructor(dir: string, onChange: (ok: boolean, reason?: string) => void) {
    this.file = path.join(dir, ".health");
    this.onChange = onChange;
  }
  ok(): boolean {
    return this.why === undefined;
  }
  reason(): string | undefined {
    return this.why;
  }
  private set(why: string | undefined) {
    const was = this.ok();
    this.why = why;
    if (was !== this.ok()) this.onChange(this.ok(), why);
  }
  run<T>(write: () => T): T {
    try {
      return write();
    } catch (e) {
      this.set((e as Error).message);
      throw e;
    }
  }
  probe(): boolean {
    try {
      appendLine(this.file, { ts: Date.now() });
      this.set(undefined);
      return true;
    } catch (e) {
      this.set((e as Error).message);
      return false;
    }
  }
}
