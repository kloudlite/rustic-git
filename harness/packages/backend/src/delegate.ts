//! Tools that start another session: `workspace_ask` (main hands a workspace's own session a goal;
//! fire-and-forget, the answer comes back later as a `[from <ws>] ...` message prompted, or followed
//! up when busy, into the CALLER's session so main never blocks) and `main_tell` (a workspace session
//! tells main it is done, blocked, or needs something; the only way a workspace speaks to main).
//! Messages carry only words: a task id never travels in the text and these tools never touch a
//! board (each session's task tools write its own). The message log (messages.ts) is the one
//! place that links a message to the sender's task (`for`) or to the ask it answers (`reply`).
//! Delegated sessions ask through `deps.permit`, the daemon's gate: the card is raised for the caller's key on every connected TUI.
//! The answer is the last assistant text seen before `agent_end`: Claude sessions emit
//! `agent_end` with an empty message list, so the events are tracked instead. It only counts after
//! the asked user message was seen (a busy Claude ends its CURRENT run before the followUp runs).
//! A `workspace_ask` is saved to disk (asks.ts) while it is in flight and resent after a bench
//! restart (`resumeAsks`), at most twice.
import type { Cards } from "./cards.ts";
import { randomBytes } from "node:crypto";
import type { ToolDef } from "@kloudlite-tui/tools";
import type { PermissionRequest, Decision, SessionHandle, SessionOpts } from "./index.ts";
import { messagesFile, recordMessage } from "./messages.ts";
import { asksDir, dropAsk, listAsks, saveAsk, type PendingAsk } from "./asks.ts";

export type DelegateDeps = {
  /** Open sessions by key (LocalBackend's map). */
  live: Map<string, SessionHandle>;
  /** Keys mid-turn right now. */
  busy: Set<string>;
  open(key: string, opts: SessionOpts): Promise<SessionHandle>;
  /** Ask the person (a card on every connected TUI) for a decision; waits for one. */
  permit(key: string, req: PermissionRequest, signal: AbortSignal): Promise<Decision>;
  /** The daemon's cards: the `question` tool raises them. Absent in tests that never ask. */
  cards?: Cards;
  /** Tell the sessions watchers the stored list moved (a forgotten workspace's sessions). */
  changed?: () => void;
  /** Where pending asks live; tests pass a temp dir. */
  asks?: string;
  /** ws key -> key of the session that last asked it: where its `main_tell` goes. */
  lastCaller?: Map<string, string>;
  /** ws keys that already sent done/blocked during the current ask: the final answer is then not
   * delivered a second time. */
  reported?: Set<string>;
  /** Where the session boards live (tasks.ts); tests pass a temp dir. */
  tasks?: string;
  /** Where the message log lives (messages.ts); tests pass a temp file. */
  messages?: string;
  /** ws key -> id of the newest ask message sent to it: what its `main_tell` answers. */
  lastAsk?: Map<string, string>;
  /** Snapshot of the words the person typed into `key` (consent.ts TurnWords). */
  typed?(key: string): string[];
  /** Add words to the turn of `key` as if typed there; a relayed ask lends the caller's. */
  lend?(key: string, words: string[]): void;
};

/** A workspace pod's source folder (crates/workspaces/src/k8s/mod.rs WORKSPACE_DIR). */
export const WORKSPACE_DIR = "/home/kl/workspace";
const hex = () => randomBytes(4).toString("hex");

const textOf = (m: any): string =>
  (typeof m?.content === "string" ? [{ type: "text", text: m.content }] : Array.isArray(m?.content) ? m.content : [])
    .filter((b: any) => b?.type === "text")
    .map((b: any) => b.text)
    .join("\n")
    .trim();

/** Send `text`, then resolve at the `agent_end` of the run that answered IT. Armed only by the user
 * message carrying `text`: a busy Claude emits `agent_end` for its CURRENT run first and only then
 * runs a followUp, so an earlier end would hand back the wrong turn's text. A disposed session
 * (`session_closed`) never ends a turn, so it resolves too. */
function answer(h: SessionHandle, text: string, send: () => Promise<void>): Promise<string> {
  return new Promise((resolve, reject) => {
    let last = "";
    let armed = false;
    const want = text.trim();
    const off = h.subscribe((e: any) => {
      if (e.type === "session_closed") {
        off();
        resolve("(session closed before answering)");
      } else if (!armed) {
        armed = e.type === "message_end" && e.message?.role === "user" && textOf(e.message).includes(want);
      } else if (e.type === "message_end" && e.message?.role === "assistant") last = textOf(e.message) || last;
      else if (e.type === "agent_end") {
        off();
        resolve(last || "(no answer)");
      }
    });
    send().catch((err) => {
      off();
      reject(err);
    });
  });
}

/** What a workspace opened for an ask gets: the caller's model and settings, none of the TUI's own
 * tools, and cards routed to whoever is connected to the CALLER's session (named for the workspace). */
function askOpts(deps: DelegateDeps, callerKey: string, a: PendingAsk, key: string): SessionOpts {
  return {
    initial: { model: a.model, thinkingLevel: a.thinkingLevel, autoCompact: a.autoCompact, codemode: a.codemode },
    tools: [],
    permission: (req, s) => deps.permit(callerKey, { ...req, session: req.session ?? key }, s),
  };
}

/** Hand `reply` to the caller's session: a view of the live one, or (after a bench restart, when
 * nobody has it open) a freshly opened one. Prompt when idle, followUp when busy; main can start a
 * turn between the check and the call, so a refused prompt retries as a followUp instead of losing
 * the reply. Not awaited: a prompt lasts the caller's whole turn. */
/** The caller's model and settings as they are NOW (the daemon's state is the truth), else as it opened. */
function callerSettings(deps: DelegateDeps, callerKey: string, caller: SessionOpts) {
  const s = deps.live.get(callerKey)?.state;
  const i = caller.initial ?? {};
  return { model: s?.model ?? i.model, thinkingLevel: s?.thinkingLevel ?? i.thinkingLevel, autoCompact: s?.autoCompact ?? i.autoCompact, codemode: s?.codemode ?? i.codemode };
}

async function deliver(
  deps: DelegateDeps,
  to: string,
  o: NonNullable<SessionOpts["initial"]>,
  reply: string,
  from: string,
): Promise<void> {
  try {
    const c = await deps.open(to, { initial: { model: o.model, thinkingLevel: o.thinkingLevel, autoCompact: o.autoCompact, codemode: o.codemode }, tools: [] });
    void (deps.busy.has(to) ? c.followUp(reply) : c.prompt(reply))
      .catch(() => c.followUp(reply))
      .catch((e) => console.error("reply lost", to, from, e))
      .finally(() => void c.dispose());
  } catch (e) {
    console.error("reply lost", to, from, e);
  }
}

const base = (key: string) => key.split(":")[0]!;

/** Run one ask to its delivery. Saved BEFORE the workspace is opened and dropped right before the
 * reply is delivered: a restart between the drop and the caller's session file loses one reply
 * (accepted); a restart before it resends the ask. */
export async function dispatchAsk(deps: DelegateDeps, a: PendingAsk): Promise<void> {
  const dir = deps.asks ?? asksDir();
  saveAsk(dir, a);
  // main_tell answers to whoever asked last; a fresh ask starts a fresh report
  deps.lastCaller?.set(a.key, a.callerKey);
  deps.reported?.delete(a.key);
  if (a.msg) deps.lastAsk?.set(a.key, a.msg);
  let h: SessionHandle | undefined;
  let reply: string;
  try {
    h = await deps.open(a.key, askOpts(deps, a.callerKey, a, a.key));
    const s = h;
    // the person typed these into the caller, not here; without them a quote of the person is "not in your messages"
    deps.lend?.(a.key, a.words ?? []);
    reply = `[from ${a.key}] ${await answer(s, a.text, () => (deps.busy.has(a.key) ? s.followUp(a.text) : s.prompt(a.text)))}`;
  } catch (err) {
    reply = `[from ${a.key}] failed: ${err instanceof Error ? err.message : String(err)}`;
  }
  // the workspace already told main done/blocked during this ask: that tell is the report, so the
  // final answer would be a second copy of it
  const told = deps.reported?.delete(a.key) ?? false;
  dropAsk(dir, a.id);
  if (!told) {
    recordMessage(deps.messages ?? messagesFile(), { from: base(a.key), to: base(a.callerKey), text: reply, reply: a.msg });
    await deliver(deps, a.callerKey, a, reply, a.key);
  }
  // our own view goes only after its answer was delivered
  if (h) await h.dispose().catch(() => {});
}

/** Boot: resend what a restart interrupted. `tries` counts resends; the third restart reports the
 * loss to the caller instead. Returns the in-flight dispatches (the daemon does not await them). */
export function resumeAsks(deps: DelegateDeps): Promise<void>[] {
  const dir = deps.asks ?? asksDir();
  return listAsks(dir).map(async (a) => {
    if (a.tries >= 2) {
      dropAsk(dir, a.id);
      return deliver(deps, a.callerKey, a, `[from ${a.key}] failed: lost in ${a.tries + 1} bench restarts; ask again if it still matters`, a.key);
    }
    const text = a.text.startsWith("[resent after restart] ") ? a.text : `[resent after restart] ${a.text}`;
    return dispatchAsk(deps, { ...a, tries: a.tries + 1, text });
  });
}

export function delegateTools(kind: "main" | "workspace", ws: string | undefined, deps: DelegateDeps, caller: SessionOpts, callerKey = "main"): ToolDef[] {
  if (kind === "workspace") {
    const tell: ToolDef = {
      name: "main_tell",
      description:
        "Tell the main session something. Use `done` when the task is finished (tests pass, branch pushed). Use `blocked` when you cannot go on. Say what would unblock you. Use `need` when you need a fact or an action from another workspace or the person. Then keep working on what you can. After `done` or `blocked`, end your turn. Your report is the answer.",
      inputSchema: {
        type: "object",
        properties: { kind: { type: "string", enum: ["done", "blocked", "need"] }, text: { type: "string" } },
        required: ["kind", "text"],
      },
      async run(i: { kind: "done" | "blocked" | "need"; text: string }) {
        const to = deps.lastCaller?.get(ws!) ?? "main";
        const msg = `[from ${ws}] ${i.kind}: ${i.text}`;
        if (i.kind !== "need") deps.reported?.add(ws!);
        recordMessage(deps.messages ?? messagesFile(), { from: base(ws!), to: base(to), text: msg, kind: i.kind, reply: deps.lastAsk?.get(ws!) });
        await deliver(deps, to, callerSettings(deps, ws!, caller), msg, ws!);
        return `told ${to}`;
      },
    };
    return [tell];
  }

  const ask: ToolDef = {
    name: "workspace_ask",
    description:
      "Give a goal to the session of a workspace. The goal is the words of the person plus context that only you have (environment, decisions, facts from the answer of another workspace). Do not give paths, libraries or steps. Pass `for` (the id of your own task this serves) so the Plan panel can link them. It returns at once. The answer of the workspace arrives later as a `[from <ws>] ...` message. Do not wait or poll for it. This is the only way for main to get work done in a workspace.",
    inputSchema: { type: "object", properties: { workspace: { type: "string", description: "The workspace id (`ws-…`, the `id` from workspace_list). Do not use the name." }, request: { type: "string" }, for: { type: "string", description: "The id of your own board task this ask serves." } }, required: ["workspace", "request"] },
    async run(input: { workspace: string; request: string; for?: string }) {
      const key = input.workspace;
      const text = `[from main session] ${input.request}`;
      const msg = recordMessage(deps.messages ?? messagesFile(), { from: base(callerKey), to: base(key), text, for: input.for });
      // Not awaited: main must stay free for the person while the workspace works.
      void dispatchAsk(deps, {
        id: hex(),
        callerKey,
        key,
        text,
        msg: msg.id,
        words: deps.typed?.(callerKey) ?? [],
        tries: 0,
        ...callerSettings(deps, callerKey, caller),
      });
      return `sent to ${key}; its session is working on it. Its answer will arrive here as a message from ${key}; do not wait or poll for it.`;
    },
  };
  return [ask];
}
