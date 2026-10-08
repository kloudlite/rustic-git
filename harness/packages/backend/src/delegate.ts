//! Tools that start another session and wait for its answer: `workspace_ask` (main asks a
//! workspace's own session) and `subagent` (a throwaway session with the pod tools only).
//! Delegated sessions use the CALLER's permission callback, so their cards land in the user's TUI.
//! The answer is the last assistant text seen before `agent_end`: Claude sessions emit
//! `agent_end` with an empty message list, so the events are tracked instead.
import { randomBytes } from "node:crypto";
import type { ToolDef } from "@kloudlite-tui/tools";
import type { SessionHandle, SessionOpts } from "./index.ts";

export type DelegateDeps = {
  /** Open sessions by key (LocalBackend's map). */
  live: Map<string, SessionHandle>;
  /** Keys mid-turn right now. */
  busy: Set<string>;
  open(key: string, opts: SessionOpts): Promise<SessionHandle>;
};

const textOf = (m: any): string =>
  (Array.isArray(m?.content) ? m.content : [])
    .filter((b: any) => b?.type === "text")
    .map((b: any) => b.text)
    .join("\n")
    .trim();

/** Send, then resolve at the next `agent_end` with the last assistant text. */
function answer(h: SessionHandle, send: () => Promise<void>): Promise<string> {
  return new Promise((resolve, reject) => {
    let last = "";
    const off = h.subscribe((e: any) => {
      if (e.type === "message_end" && e.message?.role === "assistant") last = textOf(e.message) || last;
      if (e.type === "agent_end") {
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

export function delegateTools(kind: "main" | "workspace", ws: string | undefined, deps: DelegateDeps, caller: SessionOpts): ToolDef[] {
  // same model, same gate; nothing of the TUI's own tools goes along
  const opts = (extra: Partial<SessionOpts> = {}): SessionOpts => ({ ...caller, tools: [], ...extra });

  const subagent: ToolDef = {
    name: "subagent",
    description:
      "Run a self-contained code task (search, read, edit, run) in a fresh throwaway session inside a workspace and get its final answer; use it to keep bulk exploration out of your own context.",
    inputSchema: {
      type: "object",
      properties: { ...(kind === "main" ? { workspace: { type: "string" } } : {}), task: { type: "string" } },
      required: [...(kind === "main" ? ["workspace"] : []), "task"],
    },
    async run(input: { workspace?: string; task: string }) {
      const target = kind === "workspace" ? ws! : input.workspace;
      if (!target) return "error: workspace is required";
      const h = await deps.open(`${target}:agent-${randomBytes(4).toString("hex")}`, opts({ fresh: true }));
      try {
        return await answer(h, () => h.prompt(input.task));
      } finally {
        await h.dispose();
      }
    },
  };
  if (kind === "workspace") return [subagent];

  const ask: ToolDef = {
    name: "workspace_ask",
    description:
      "Ask a workspace's own session to do something inside that workspace (edit code, run commands, answer about its files) and get its answer; the main session has no filesystem of its own.",
    inputSchema: { type: "object", properties: { workspace: { type: "string" }, request: { type: "string" } }, required: ["workspace", "request"] },
    async run(input: { workspace: string; request: string }) {
      const key = input.workspace;
      const text = `[from main session] ${input.request}`;
      const existing = deps.live.get(key);
      const h = existing ?? (await deps.open(key, opts()));
      try {
        return await answer(h, () => (existing && deps.busy.has(key) ? h.followUp(text) : h.prompt(text)));
      } finally {
        if (!existing) await h.dispose();
      }
    },
  };
  return [ask, subagent];
}
