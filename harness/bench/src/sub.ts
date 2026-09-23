// The sub lifecycle (spec §4). Every step is a log row before the next side effect, so a crash
// resumes at the right step on boot: a child row with no clone id is re-spawned by hand, a child
// answer with no parent row is re-delivered by the scheduler, and a closed child is never touched.
import { Platform, PlatformError } from "./platform.ts";
import { remote, text } from "./engine/remote.ts";
import { append, readRows, type Row } from "./rows.ts";
import type { Scheduler } from "./scheduler.ts";
import type { Session } from "./session.ts";
import type { SessionList } from "./sessions.ts";

const NON_FF = /non-fast-forward|fetch first|rejected/;

export class Subs {
  private list: SessionList;
  private sched: Scheduler;
  private platform: Platform;
  private backendFor: (ws: string) => Promise<void>;

  constructor(list: SessionList, sched: Scheduler, platform: Platform, backendFor: (ws: string) => Promise<void>) {
    this.list = list;
    this.sched = sched;
    this.platform = platform;
    this.backendFor = backendFor;
  }

  async delegate(from: Session, target: string, instruction: string): Promise<string> {
    if (from.row.tier === "top") return this.toMain(from, target, instruction);
    if (target) {
      const child = this.list.bySeq(Number(target));
      if (!child || child.parent !== from.row.seq || child.state !== "open") {
        const open = this.list.children(from.row.seq).filter((c) => c.state === "open").map((c) => c.seq).join(", ");
        return `error: no open child ${target}; open children: ${open || "none"}`;
      }
      this.sched.get(child.seq)!.receive(from.row.seq, instruction);
      append(from.file, { kind: "delegate", ts: Date.now(), turn: from.current ?? -1, child: child.seq, instruction });
      this.sched.kick();
      return `delegated to ${child.seq}, waiting`;
    }
    const mainWs = from.row.workspace!;
    let branch: string, clone: string;
    try {
      await this.backendFor(mainWs);
      branch = text(await remote(mainWs, "exec", { cmd: "git rev-parse --abbrev-ref HEAD" })).trim();
      const seq = this.list.all().length + 1; // ponytail: the row's real seq is assigned on create; the clone name only needs to be unique
      clone = await this.platform.clone(mainWs, `sub-${seq}-${Date.now().toString(36)}`);
    } catch (e) {
      return `error: ${e instanceof PlatformError ? e.body : (e as Error).message}`; // a refused clone is the answer, verbatim (spec §5)
    }
    const row = this.list.create(from.row.model, { tier: "sub", state: "open", parent: from.row.seq, workspace: clone, target: branch, kind: "workspace" });
    await this.backendFor(clone);
    const child = this.sched.get(row.seq) ?? (this.sched.kick(), this.sched.get(row.seq)!);
    child.onAnswer = (c, end) => this.finish(c, end);
    child.receive(from.row.seq, `${instruction}\n\nWork on branch sub/${row.seq}; commit your changes there. Your result is pushed into the main workspace when you finish.`);
    append(from.file, { kind: "delegate", ts: Date.now(), turn: from.current ?? -1, child: row.seq, instruction });
    this.sched.kick();
    return `delegated to ${row.seq}, waiting`;
  }

  private toMain(from: Session, name: string, instruction: string) {
    const mains = this.list.all().filter((s) => s.tier === "main" && s.state !== "closed");
    const m = mains.find((s) => s.workspace === name || s.name === name);
    if (!m) return `error: no main named ${name}; mains: ${mains.map((s) => s.workspace).join(", ") || "none"}`;
    this.sched.get(m.seq)!.receive(from.row.seq, instruction);
    append(from.file, { kind: "delegate", ts: Date.now(), turn: from.current ?? -1, child: m.seq, instruction });
    this.sched.kick();
    return `delegated to ${m.workspace}, waiting`;
  }

  async finish(child: Session, end: Extract<Row, { kind: "turn.end" }>) {
    const row = child.row;
    const parent = this.list.bySeq(row.parent!)!;
    const clone = row.workspace!, mainWs = parent.workspace!;
    try {
      const mainIp = (await this.platform.tools(mainWs)).replace(/:\d+$/, "");
      // /home/kl is the workspace's own volume; the tree is /home/kl/workspace, never the id or name
      const push = async () => text(await remote(clone, "exec", { cmd: `git push ssh://kl@${mainIp}/home/kl/workspace HEAD:${row.target}`, timeout_ms: 120_000 }));
      let out = await push();
      const retried = readRows(child.file).some((r) => r.kind === "user" && r.text.startsWith("rebase onto main"));
      if (NON_FF.test(out) && !retried) {
        // main moved under us: one retry through the child, then it reports failure itself
        child.receive(row.parent!, `rebase onto main and push again: the push was rejected.\n${out}`);
        child.onAnswer = (c, e) => this.finish(c, e);
        this.sched.kick();
        return;
      }
      const commit = text(await remote(clone, "exec", { cmd: "git rev-parse HEAD" })).trim();
      const pushed = !/exit \d/.test(out);
      const answer = pushed ? `${end.answer}\n\npushed ${commit} to ${row.target}` : `${end.answer}\n\npush failed:\n${out}`;
      this.sched.get(parent.seq)!.receive(row.seq, answer, end.turn); // step 4 before step 5: the answer is never lost to a crash
    } catch (e) {
      this.sched.get(parent.seq)!.receive(row.seq, `${end.answer}\n\npush failed:\n${(e as Error).message}`, end.turn);
    }
    try {
      await this.platform.remove(clone);
    } catch (e) {
      this.sched.get(parent.seq)!.receive(row.seq, `clone ${clone} could not be deleted: ${(e as Error).message}`);
    }
    this.list.update(row.id, { state: "closed" });
    this.sched.kick();
  }
}
