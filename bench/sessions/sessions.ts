// Warm workspace sessions: one long-lived Agent SDK session per workspace, fed over streaming
// input so the process and context stay warm between turns instead of paying cold-start cost on
// every message. `t:`/`a:`/`u:`/`r:`/`s:` line prefixes are the demo mod's convention, kept so the
// bench UI and any existing tooling parse the same shape. Built-ins (Read/Write/Edit/.../WebFetch)
// are disallowed because the workspace's own tools (named `mcp__kloudlite__<name>` by the mod
// host) are the only sanctioned way to touch the workspace filesystem/network from here; the
// session itself runs outside the workspace container. `query` is injected (not imported here) so
// tests can fake the SDK without spawning a real subprocess — this class is pure state, no I/O
// beyond calling the injected `query` and (via the caller) persisting `toJSON()`.
//
// Idle contract: `busy()` reflects whether any workspace has a message in flight or queued. The
// agent's readiness/idle probe (`bins/agent/.../bench.rs:122`) polls this indirectly through
// main.ts's `/idle` so it knows when it's safe to treat the bench as quiesced.

export const DISABLED = ["Read", "Write", "Edit", "MultiEdit", "Bash", "Grep", "Glob", "NotebookEdit", "WebFetch"];

const MOD_TOOL_PREFIX = "mcp__kloudlite__";

type SavedSession = { lines: string[]; sessionId?: string };

type Agent = { id: string; label: string; status: "running" | "done"; lines: string[] };

type QueryFn = (args: { prompt: AsyncGenerator<unknown>; options: Record<string, unknown> }) => AsyncIterable<any>;

const primary = (input: unknown): string => {
  if (!input || typeof input !== "object") return "";
  for (const v of Object.values(input as Record<string, unknown>)) {
    if (typeof v === "string" && v.length) return v;
  }
  return "";
};

const stripToolName = (name: string) => (name.startsWith(MOD_TOOL_PREFIX) ? name.slice(MOD_TOOL_PREFIX.length) : name);

class Session {
  lines: string[] = [];
  busy = false;
  sessionId: string | undefined;
  queue: string[] = [];
  agents = new Map<string, Agent>();
  inflight = false;
  private wake: (() => void) | undefined;
  private settledWaiters: Array<() => void> = [];
  // true once we've already retried without resume after a failed resume, so we only retry once
  private resumeRetried = false;

  constructor(prior: SavedSession | undefined) {
    this.lines = prior?.lines ?? [];
    this.sessionId = prior?.sessionId;
  }

  // Ruling 4: the first send to an idle session goes in flight synchronously (no queue hop), so
  // `queued` only ever lists messages stuck behind it. `deliver` is that one in-flight slot;
  // `queue` is strictly the overflow behind it.
  private deliver: string | undefined;

  push(text: string) {
    this.busy = true;
    if (!this.inflight) {
      this.inflight = true;
      this.deliver = text;
    } else {
      this.queue.push(text);
    }
    this.wake?.();
  }

  private async *input(): AsyncGenerator<unknown> {
    while (true) {
      if (this.deliver !== undefined) {
        const text = this.deliver;
        this.deliver = undefined;
        this.lines.push(`u:${text}`);
        yield { type: "user", message: { role: "user", content: text } };
        continue;
      }
      await new Promise<void>((r) => (this.wake = r));
    }
  }

  private checkSettled() {
    if (!this.busy && this.queue.length === 0) {
      const waiters = this.settledWaiters;
      this.settledWaiters = [];
      for (const w of waiters) w();
    }
  }

  settled(): Promise<void> {
    if (!this.busy && this.queue.length === 0) return Promise.resolve();
    return new Promise((r) => this.settledWaiters.push(r));
  }

  // One run loop per session, started once in Sessions.get(). Mirrors the demo's message
  // handling, split into: parent lines vs. per-subagent lines (keyed by parent_tool_use_id), and
  // the resume-once-then-fresh recovery on a dead session id.
  async run(query: QueryFn, ws: string, home: string, modDir: string, onSettle: () => void) {
    try {
      const q = query({
        prompt: this.input(),
        options: {
          cwd: `${home}/sessions/${ws}`,
          env: { ...process.env, KL_WORKSPACE: ws },
          plugins: [{ type: "local", path: modDir }],
          disallowedTools: DISABLED,
          permissionMode: "bypassPermissions",
          includePartialMessages: true,
          ...(this.sessionId ? { resume: this.sessionId } : {}),
        },
      });
      await this.drain(q, onSettle);
    } catch (err) {
      if (this.sessionId && !this.resumeRetried) {
        // Dead/unknown session id: say why once, drop it, and restart fresh rather than wedge
        // this workspace forever on a resume that can never succeed.
        this.lines.push(`s:(error) resume failed: ${String(err).slice(0, 200)}`);
        this.sessionId = undefined;
        this.resumeRetried = true;
        await this.run(query, ws, home, modDir, onSettle);
        return;
      }
      this.lines.push(`s:(error) ${String(err).slice(0, 200)}`);
      this.busy = false;
      this.inflight = false;
      onSettle();
      this.checkSettled();
      throw err; // tells Sessions.get() to delete this session so the next send restarts it
    }
  }

  private async drain(q: AsyncIterable<any>, onSettle: () => void) {
    let draft = "";
    let open = false;
    for await (const m of q) {
      if (m.type === "system" && m.subtype === "init") this.sessionId = m.session_id;

      const target = this.lineTarget(m);

      if (m.type === "stream_event") {
        const ev = m.event;
        if (ev?.type === "content_block_delta" && ev.delta?.type === "text_delta") {
          draft += ev.delta.text;
          if (open) target.lines[target.lines.length - 1] = `a:${draft}`;
          else {
            open = true;
            target.lines.push(`a:${draft}`);
          }
        }
        if (ev?.type === "content_block_stop") {
          draft = "";
          open = false;
        }
      }

      if (m.type === "assistant") {
        for (const b of m.message.content) {
          if (b.type === "tool_use") {
            // The Agent/Task tool launches a subagent: b.id becomes the parent_tool_use_id on
            // every message that subagent emits, so pre-register it here with its label (the
            // `description` the caller gave it) before any child message arrives.
            if (target === this && (b.name === "Agent" || b.name === "Task") && b.id) {
              if (!this.agents.has(b.id)) {
                this.agents.set(b.id, { id: b.id, label: b.input?.description ?? b.id, status: "running", lines: [] });
              }
            }
            const line = `t:${stripToolName(b.name)}(${primary(b.input).slice(0, 120)})`;
            target.lines.push(line);
          } else if (b.type === "text") {
            // Non-streaming assistant text (no stream_event deltas, e.g. a test double): append
            // directly rather than relying on partial-message deltas.
            target.lines.push(`a:${b.text}`);
          }
        }
      }

      if (m.type === "user" && Array.isArray(m.message?.content)) {
        for (const b of m.message.content) {
          if (b.type === "tool_result") {
            const t = typeof b.content === "string" ? b.content : (b.content ?? []).map((p: any) => (p.type === "text" ? p.text : "")).join(" ");
            const first = String(t).split("\n")[0]?.slice(0, 100);
            if (first) target.lines.push(`r:${first}`);
          }
        }
      }

      if (m.type === "result") {
        if (m.parent_tool_use_id) {
          const a = this.agents.get(m.parent_tool_use_id);
          if (a) a.status = "done";
        } else {
          this.inflight = false;
          if (this.queue.length) {
            this.inflight = true;
            this.deliver = this.queue.shift();
            this.wake?.();
          } else {
            this.busy = false;
          }
          onSettle();
        }
      }

      this.lines = this.lines.slice(-100);
      this.checkSettled();
    }
  }

  // Messages with a non-null parent_tool_use_id belong to a subagent: route them to
  // `agents[id].lines` instead of the parent session's own lines (test 3's requirement).
  private lineTarget(m: any): Session | Agent {
    const parentId: string | null | undefined = m.parent_tool_use_id;
    if (!parentId) return this;
    let a = this.agents.get(parentId);
    if (!a) {
      a = { id: parentId, label: parentId, status: "running", lines: [] };
      this.agents.set(parentId, a);
    }
    return a;
  }
}

export class Sessions {
  private query: QueryFn;
  private home: string;
  private modDir: string;
  private saved: Record<string, SavedSession>;
  private sessions = new Map<string, Session>();

  constructor({ query, home, modDir, saved }: { query: QueryFn; home: string; modDir: string; saved: Record<string, SavedSession> }) {
    this.query = query;
    this.home = home;
    this.modDir = modDir;
    this.saved = saved;
  }

  private get(ws: string): Session {
    let s = this.sessions.get(ws);
    if (!s) {
      s = new Session(this.saved[ws]);
      this.sessions.set(ws, s);
      void s.run(this.query, ws, this.home, this.modDir, () => {}).catch(() => {
        // Run loop threw past the resume-once retry: drop the session so the next send restarts
        // it fresh (demo behaviour), matching the comment in sessions.ts's run().
        this.sessions.delete(ws);
      });
    }
    return s;
  }

  // POST /send. `agentId` addresses a running subagent, but the SDK has no way to deliver a
  // message to one directly — only the parent session accepts input — so a send aimed at a
  // subagent is delivered to the parent with a prefix naming which subagent it was meant for
  // (controller ruling 3).
  send(ws: string, text: string, agentId?: string) {
    const s = this.get(ws);
    if (agentId) {
      const a = s.agents.get(agentId);
      const label = a?.label ?? agentId;
      s.push(`[for subagent ${label}] ${text}`);
      return;
    }
    s.push(text);
  }

  state() {
    const out: Record<string, { lines: string[]; busy: boolean; queued: string[]; agents: Agent[] }> = {};
    for (const [ws, v] of Object.entries(this.saved)) {
      if (!this.sessions.has(ws)) out[ws] = { lines: v.lines ?? [], busy: false, queued: [], agents: [] };
    }
    for (const [ws, s] of this.sessions) {
      out[ws] = { lines: s.lines, busy: s.busy, queued: [...s.queue], agents: [...s.agents.values()] };
    }
    return out;
  }

  busy(): boolean {
    for (const s of this.sessions.values()) if (s.busy || s.queue.length) return true;
    return false;
  }

  settled(ws: string): Promise<void> {
    const s = this.sessions.get(ws);
    if (!s) return Promise.resolve();
    return s.settled();
  }

  toJSON(): Record<string, SavedSession> {
    const out: Record<string, SavedSession> = {};
    for (const [ws, s] of this.sessions) out[ws] = { lines: s.lines, sessionId: s.sessionId };
    for (const [ws, v] of Object.entries(this.saved)) if (!this.sessions.has(ws)) out[ws] = v;
    return out;
  }
}
