// One actor per session row (spec §2). It owns exactly one append-only log and runs one turn at a
// time; everything it knows between turns is re-read from the rows, so a restart loses nothing but
// the turn that was in flight, which boot marks `interrupted` and never resumes.
import { TOOLS, type Tool, type User } from "./engine/index.ts";
import { append, readRows, unread, nextTurn, openTurn, type Row } from "./rows.ts";
import type { SessionRow } from "./sessions.ts";

export type TurnCtx = { prompt: string; cwd: string; tools: Tool[]; user: User; readOnly: boolean; log: (step: string) => void; signal: AbortSignal; history: Row[] };
export type Turn = (ctx: TurnCtx) => Promise<string>;
export type Hooks = {
  delegate: (from: Session, target: string, instruction: string) => Promise<string>;
  push?: (from: Session) => Promise<string>;
  tell: (from: Session, text: string) => void;
  askPerson: (from: Session, question: string) => Promise<string>;
};

// Tiers are tool lists, not prompt text (ruling 3). Main reads and reviews; only a sub changes a tree.
const TALK = ["tell_user", "ask_user", "think", "recall"];
export const TIER_TOOLS: Record<"top" | "main" | "sub", string[] | "all"> = {
  top: [...TALK, "delegate"],
  main: [...TALK, "delegate", "push", "read", "glob", "grep", "bash_output"],
  sub: "all",
};

const benchTool = (name: string, description: string, params: { name: string; description: string }[], run: Tool["run"]): Tool =>
  ({ name, description, brief: description.slice(0, 60), params: params.map((p) => ({ ...p, kind: "free" as const })), outcomes: ["done", "failed"], run });

export class Session {
  running = false;
  onAnswer?: (s: Session, end: Extract<Row, { kind: "turn.end" }>) => Promise<void>;
  private ctl?: AbortController;
  readonly row: SessionRow;
  readonly file: string;
  private turnFn: Turn;
  private hooks: Hooks;
  constructor(row: SessionRow, file: string, turnFn: Turn, hooks: Hooks) {
    this.row = row;
    this.file = file;
    this.turnFn = turnFn;
    this.hooks = hooks;
  }

  rows() { return readRows(this.file); }
  hasUnread() { return unread(this.rows()).length > 0; }
  receive(from: "person" | number, text: string, childTurn?: number) { append(this.file, { kind: "user", ts: Date.now(), from, text, ...(childTurn === undefined ? {} : { childTurn }) }); }

  tools(): Tool[] {
    const tier = this.row.tier ?? "main";
    const own: Tool[] = [
      benchTool("delegate", "Hand an instruction to another session and carry on; its answer arrives as a later message. target: a main's workspace name (from top), empty for a new sub or an open child's seq (from main).",
        [{ name: "target", description: "workspace name, child seq, or empty" }, { name: "instruction", description: "what the child should do" }],
        (_cwd, a) => this.hooks.delegate(this, a.target ?? "", a.instruction ?? "")),
      benchTool("recall", "Earlier turns of this session, oldest first.", [{ name: "turns", description: "how many turns back" }],
        async (_cwd, a) => this.rows().filter((r) => r.kind === "user" || r.kind === "turn.end").slice(-2 * (Number(a.turns) || 5)).map((r) => (r.kind === "user" ? `> ${r.text}` : r.answer ?? r.error ?? "")).join("\n")),
    ];
    if (tier === "main") own.push(benchTool("push", "Push this workspace's working branch to the platform repo.", [],
      () => this.hooks.push ? this.hooks.push(this) : Promise.reject(new Error("push is not wired for this bench"))));
    const allow = TIER_TOOLS[tier];
    const engine = TOOLS.filter((t) => t.name !== "recall" && (allow === "all" || allow.includes(t.name)));
    const ownAllowed = own.filter((t) => allow === "all" ? t.name !== "delegate" && t.name !== "push" : allow.includes(t.name));
    return [...ownAllowed, ...engine];
  }

  async turn() {
    if (this.running) return;
    const rows = this.rows();
    const pending = unread(rows);
    if (pending.length === 0) return;
    this.running = true;
    this.ctl = new AbortController();
    const turn = nextTurn(rows);
    append(this.file, { kind: "turn.start", ts: Date.now(), turn });
    const prompt = pending.map((r) => (r.from === "person" ? r.text : `[from session ${r.from}]\n${r.text}`)).join("\n\n");
    const user: User = { tell: (m) => this.hooks.tell(this, m), ask: (q) => this.hooks.askPerson(this, q) };
    let end: Extract<Row, { kind: "turn.end" }>;
    try {
      const answer = await this.turnFn({ prompt, cwd: this.row.workspace ?? `bench-${this.row.seq}`, tools: this.tools(), user, readOnly: this.row.tier !== "sub", log: (step) => append(this.file, { kind: "turn.step", ts: Date.now(), turn, step }), signal: this.ctl.signal, history: rows });
      if (this.ctl.signal.aborted) return;
      end = { kind: "turn.end", ts: Date.now(), turn, answer };
    } catch (e) {
      if (this.ctl.signal.aborted) return;
      end = { kind: "turn.end", ts: Date.now(), turn, error: (e as Error).message };
    } finally {
      this.running = false;
    }
    append(this.file, end);
    if (end.answer !== undefined && this.onAnswer) await this.onAnswer(this, end);
  }

  abort() {
    const turn = openTurn(this.rows());
    if (turn !== undefined) append(this.file, { kind: "interrupted", ts: Date.now(), turn });
    this.ctl?.abort();
    this.running = false;
  }
}
