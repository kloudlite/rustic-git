//! Tools that start another session: `workspace_ask` (main hands a workspace's own session a goal;
//! fire-and-forget, the answer comes back later as a `[from <ws>] ...` message prompted, or followed
//! up when busy, into the CALLER's session so main never blocks) and `main_tell` (a workspace session
//! tells main it is done, blocked, or needs something; the only way a workspace speaks to main).
//! Delegated sessions ask through `deps.permit`, which routes to whichever client is connected to the
//! CALLER's session now (clients.ts), but each request names the delegated session
//! (`PermissionRequest.session`), so the TUI shows the card in that workspace's view.
//! The answer is the last assistant text seen before `agent_end`: Claude sessions emit
//! `agent_end` with an empty message list, so the events are tracked instead. It only counts after
//! the asked user message was seen (a busy Claude ends its CURRENT run before the followUp runs).
//! A `workspace_ask` is saved to disk (asks.ts) while it is in flight and resent after a bench
//! restart (`resumeAsks`), at most twice.
import { randomBytes } from "node:crypto";
import type { ToolDef } from "@kloudlite-tui/tools";
import type { PermissionRequest, Decision, SessionHandle, SessionOpts } from "./index.ts";
import { asksDir, dropAsk, listAsks, saveAsk, type PendingAsk } from "./asks.ts";

export type DelegateDeps = {
  /** Open sessions by key (LocalBackend's map). */
  live: Map<string, SessionHandle>;
  /** Keys mid-turn right now. */
  busy: Set<string>;
  open(key: string, opts: SessionOpts): Promise<SessionHandle>;
  /** Ask the client connected to `key` (the caller's session) for a decision; waits for one. */
  permit(key: string, req: PermissionRequest, signal: AbortSignal): Promise<Decision>;
  /** Where pending asks live; tests pass a temp dir. */
  asks?: string;
  /** ws key -> key of the session that last asked it: where its `main_tell` goes. */
  lastCaller?: Map<string, string>;
  /** ws keys that already sent done/blocked during the current ask: the final answer is then not
   * delivered a second time. */
  reported?: Set<string>;
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
    model: a.model,
    thinkingLevel: a.thinkingLevel,
    autoCompact: a.autoCompact,
    codemode: a.codemode,
    tools: [],
    permission: (req, s) => deps.permit(callerKey, { ...req, session: req.session ?? key }, s),
  };
}

/** Hand `reply` to the caller's session: a view of the live one, or (after a bench restart, when
 * nobody has it open) a freshly opened one. Prompt when idle, followUp when busy; main can start a
 * turn between the check and the call, so a refused prompt retries as a followUp instead of losing
 * the reply. Not awaited: a prompt lasts the caller's whole turn. */
async function deliver(
  deps: DelegateDeps,
  to: string,
  o: Pick<SessionOpts, "model" | "thinkingLevel" | "autoCompact" | "codemode">,
  reply: string,
  from: string,
): Promise<void> {
  try {
    const c = await deps.open(to, { model: o.model, thinkingLevel: o.thinkingLevel, autoCompact: o.autoCompact, codemode: o.codemode, tools: [] });
    void (deps.busy.has(to) ? c.followUp(reply) : c.prompt(reply))
      .catch(() => c.followUp(reply))
      .catch((e) => console.error("reply lost", to, from, e))
      .finally(() => void c.dispose());
  } catch (e) {
    console.error("reply lost", to, from, e);
  }
}

/** Task board stub: Task 2 turns this into the "next ready tasks" note appended to a done/blocked tell. */
const boardNote = (..._: unknown[]) => "";

/** Run one ask to its delivery. Saved BEFORE the workspace is opened and dropped right before the
 * reply is delivered: a restart between the drop and the caller's session file loses one reply
 * (accepted); a restart before it resends the ask. */
export async function dispatchAsk(deps: DelegateDeps, a: PendingAsk): Promise<void> {
  const dir = deps.asks ?? asksDir();
  saveAsk(dir, a);
  // main_tell answers to whoever asked last; a fresh ask starts a fresh report
  deps.lastCaller?.set(a.key, a.callerKey);
  deps.reported?.delete(a.key);
  let h: SessionHandle | undefined;
  let reply: string;
  try {
    h = await deps.open(a.key, askOpts(deps, a.callerKey, a, a.key));
    const s = h;
    reply = `[from ${a.key}] ${await answer(s, a.text, () => (deps.busy.has(a.key) ? s.followUp(a.text) : s.prompt(a.text)))}`;
  } catch (err) {
    reply = `[from ${a.key}] failed: ${err instanceof Error ? err.message : String(err)}`;
  }
  // the workspace already told main done/blocked during this ask: that tell is the report, so the
  // final answer would be a second copy of it
  const told = deps.reported?.delete(a.key) ?? false;
  dropAsk(dir, a.id);
  if (!told) await deliver(deps, a.callerKey, a, reply, a.key);
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
        "Tell the main session something. done: the task is finished (tests pass, branch pushed); blocked: you cannot go on, say what would unblock you; need: a fact or an action from another workspace or the person, then keep working on what you can. Name the task id when the work came with one. After done or blocked, end your turn: your report is the answer.",
      inputSchema: {
        type: "object",
        properties: { kind: { type: "string", enum: ["done", "blocked", "need"] }, task: { type: "string" }, text: { type: "string" } },
        required: ["kind", "text"],
      },
      async run(i: { kind: "done" | "blocked" | "need"; task?: string; text: string }) {
        const to = deps.lastCaller?.get(ws!) ?? "main";
        let msg = `[from ${ws}]${i.task ? ` [task ${i.task}]` : ""} ${i.kind}: ${i.text}`;
        if (i.kind !== "need") {
          deps.reported?.add(ws!);
          msg += boardNote(deps, ws!, i);
        }
        await deliver(deps, to, caller, msg, ws!);
        return `told ${to}`;
      },
    };
    return [tell];
  }

  const ask: ToolDef = {
    name: "workspace_ask",
    description:
      "Hand a workspace's own session a goal: the person's words plus context only you have (environment, decisions, facts from another workspace's answer), never paths, libraries or steps. Returns at once; the workspace's answer arrives later as a `[from <ws>] ...` message. Never wait or poll for it. The only way main gets work done in a workspace.",
    inputSchema: { type: "object", properties: { workspace: { type: "string" }, request: { type: "string" } }, required: ["workspace", "request"] },
    async run(input: { workspace: string; request: string }) {
      const key = input.workspace;
      // Not awaited: main must stay free for the person while the workspace works.
      void dispatchAsk(deps, {
        id: hex(),
        callerKey,
        key,
        text: `[from main session] ${input.request}`,
        tries: 0,
        model: caller.model,
        codemode: caller.codemode,
        thinkingLevel: caller.thinkingLevel,
        autoCompact: caller.autoCompact,
      });
      return `sent to ${key}; its session is working on it. Its answer will arrive here as a message from ${key}; do not wait or poll for it.`;
    },
  };
  return [ask];
}
