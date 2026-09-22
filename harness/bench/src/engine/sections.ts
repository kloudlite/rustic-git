import { existsSync, mkdirSync, readFileSync, appendFileSync, renameSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { ACT } from "./tools.ts";

export type Role = "user" | "main" | "tool";
export type Message = { id: number; section: string; role: Role; text: string; at: string };
export type Closed = { id: string; label: string; summary: string; first: number; last: number };
export type Summarise = (messages: string) => Promise<{ label: string; summary: string }>;

// Every message stays in a local log in full, and a finished topic is
// summarised once (a background LLM call) so later turns send its label+summary instead of resending it.
const RECENT = 6;    // newest messages of the open section always sent in full
const OLD_TOOL = 300; // chars of an older tool output still sent; the rest stays in the log

// One long topic never closes, so its old tool outputs would be resent every turn. Past the newest few they go as a head only.
const shrink = (m: Message, old: boolean) => old && m.role === "tool" && m.text.length > OLD_TOOL ? `${m.text.slice(0, OLD_TOOL)}… (${m.text.length - OLD_TOOL} more chars cut: run the tool again if needed)` : m.text;

export class Sections {
  private messagesFile: string;
  private sectionsFile: string;
  private messages: Message[] = [];
  private closed: Closed[] = [];
  private seq = 0;
  private openId = 1;
  private start = 0; // id of the open section's first message
  private pending: Message[] = []; // a section being summarised: sent in full until its summary lands
  private closing = false; // one close in flight at a time: a second new_topic before it lands just keeps the section open
  private summarise: Summarise;

  constructor(dir: string, summarise: Summarise, warn: (text: string) => void = () => {}) {
    this.summarise = summarise;
    mkdirSync(dir, { recursive: true });
    this.messagesFile = join(dir, "messages.jsonl");
    this.sectionsFile = join(dir, "sections.json");
    if (existsSync(this.messagesFile)) {
      try { this.messages = readFileSync(this.messagesFile, "utf8").trim().split("\n").filter(Boolean).map((l) => JSON.parse(l)); }
      catch (err) { warn(`sections: failed to parse ${this.messagesFile}: ${err}`); this.messages = []; }
    }
    if (existsSync(this.sectionsFile)) {
      try { this.closed = JSON.parse(readFileSync(this.sectionsFile, "utf8")); }
      catch (err) { warn(`sections: failed to parse ${this.sectionsFile}: ${err}`); this.closed = []; }
    }
    this.seq = this.messages.length > 0 ? this.messages[this.messages.length - 1].id + 1 : 0;
    this.openId = this.closed.length > 0 ? Number(this.closed[this.closed.length - 1].id.slice(1)) + 1 : 1;
    this.start = this.closed.length > 0 ? this.closed[this.closed.length - 1].last + 1 : 0;
  }

  private sectionId() { return `S${this.openId}`; }
  private open() { return this.messages.filter((m) => m.id >= this.start); }

  log(role: Role, text: string) {
    const m: Message = { id: this.seq++, section: this.sectionId(), role, text, at: new Date().toISOString() };
    this.messages.push(m);
    appendFileSync(this.messagesFile, `${JSON.stringify(m)}\n`);
  }

  private saveSections() {
    const tmp = `${this.sectionsFile}.tmp`;
    writeFileSync(tmp, JSON.stringify(this.closed));
    renameSync(tmp, this.sectionsFile);
  }

  // Closes only on new_topic at >= ACT: a wrong "follow_up" costs tokens, a wrong "new_topic" hides context, so the
  // bar sits on the costlier side. Runs in the background (the caller does not await this before continuing);
  // a summariser failure leaves the section open-ended in full, retried at the next close.
  // Call it before the new line is logged, so that line opens the next section.
  async topic(confidence: number, choice: string): Promise<Closed | undefined> {
    if (choice !== "new_topic" || confidence < ACT || this.closing) return undefined;
    const old = this.open();
    if (old.length === 0) return undefined;
    // The boundary moves now, not when the summary lands: what is logged meanwhile belongs to the new topic.
    const sid = this.sectionId();
    this.closing = true; this.pending = old; this.start = this.seq; this.openId++;
    try {
      const { label, summary } = await this.summarise(old.map((m) => `[${m.role}] ${m.text}`).join("\n\n"));
      const c: Closed = { id: sid, label, summary, first: old[0].id, last: old[old.length - 1].id };
      this.closed.push(c);
      this.saveSections();
      return c;
    } catch { this.start = old[0].id; this.openId--; return undefined; /* summariser failed: the section goes on, in full, and is retried at the next close */ }
    finally { this.closing = false; this.pending = []; }
  }

  closedSections(): Closed[] { return this.closed; }
  hasOpenMessages(): boolean { return this.open().length > 0; }

  context(): string {
    const earlier = this.closed.map((c) => `[${c.id}] ${c.label}: ${c.summary}`).join("\n");
    const open = [...this.pending, ...this.open()].map((m, i, all) => `[${m.role}] ${shrink(m, i < all.length - RECENT)}`).join("\n\n");
    return `${earlier ? `Earlier sections (recall one by its label for the full messages):\n${earlier}\n\n` : ""}Current section:\n${open}`;
  }

  // Labels shown as recall candidates, e.g. "S2 move todo routes to todos.js".
  labels(): string[] { return this.closed.map((c) => `${c.id} ${c.label}`); }

  recall(id: string): string {
    const c = this.closed.find((x) => x.id === id || `${x.id} ${x.label}` === id);
    if (!c) return "error: no such section";
    return this.messages.filter((m) => m.id >= c.first && m.id <= c.last).map((m) => `[${m.role}] ${m.text}`).join("\n\n");
  }
}
