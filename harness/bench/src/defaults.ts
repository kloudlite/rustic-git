import path from "node:path";
import { readJson, replaceJson } from "./log.ts";

export type Thinking = "off" | "minimal" | "low" | "medium" | "high" | "xhigh";
export type Effort = "low" | "medium" | "high" | "max";
export type Triple = { model?: string; thinking?: Thinking; effort?: Effort };
export const THINKING: Thinking[] = ["off", "minimal", "low", "medium", "high", "xhigh"];
export const EFFORT: Effort[] = ["low", "medium", "high", "max"];

/**
 * The general default: the last pick the person made ANYWHERE (spec §1.2). A dispatch that names a
 * model never writes here; only a person's pick does. One small file, read once, replaced on write.
 */
export class Defaults {
  private file: string;
  private cur: Triple;
  constructor(dir: string) {
    this.file = path.join(dir, "defaults.json");
    this.cur = readJson<Triple>(this.file, {});
  }
  get(): Triple {
    return { ...this.cur };
  }
  /** A key given as undefined is a CLEAR, not a no-op: that is how the person turns effort off. */
  set(t: Partial<Triple>): Triple {
    const next: Triple = { ...this.cur, ...t };
    for (const k of Object.keys(next) as (keyof Triple)[]) if (next[k] === undefined) delete next[k];
    this.cur = next;
    replaceJson(this.file, next);
    return this.get();
  }
}
