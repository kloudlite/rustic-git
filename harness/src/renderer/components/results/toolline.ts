/**
 * One muted line per tool call, the way opencode's session view draws them (owner, 2026-09-17):
 * a glyph, a verb, the argument, and what came back — `∗ Grep "homepage" (18 matches)`. A card is
 * what you get when you click it; the LINE is what you read when you are not reading it.
 *
 * Pure, so the mapping is a table test rather than a screenshot: the bug this shape replaces was a
 * transcript of open blocks in which the one line that mattered was three scrolls up.
 */
export type ToolLine = { glyph: string; verb: string; arg: string; count?: string };

const n = (t: string | undefined, one: string, many = `${one}es`) => {
  if (!t) return undefined;
  const lines = t.replace(/\s+$/, "").split("\n").filter(Boolean).length;
  return `${lines} ${lines === 1 ? one : many}`;
};

/** `path/to/file.tsx` from an argument that may be an absolute path in a long home. */
const short = (p: string) => String(p).replace(/^\/home\/kl\/workspaces\/[^/]+\//, "").replace(/^\/+/, "");

export function toolLine(tool: string | undefined, args: Record<string, unknown> = {}, output?: string, state?: { pending?: boolean; ok?: boolean; secs?: number }): ToolLine {
  const s = (k: string) => (typeof args[k] === "string" ? (args[k] as string) : "");
  switch (tool) {
    case "bash":
      return { glyph: "$", verb: "", arg: s("command"), count: state?.pending ? "running" : /\[exit (\d+)\]/.exec(output ?? "")?.[0]?.replace(/[[\]]/g, "") ?? "exit 0" };
    case "read":
      return { glyph: "→", verb: "Read", arg: short(s("path")), count: /(\d+) lines in all/.exec(output ?? "")?.[1] ? `${/(\d+) lines in all/.exec(output ?? "")![1]} lines` : n(output, "line") };
    case "write":
      return { glyph: "✎", verb: "Write", arg: short(s("path")), count: /wrote (\d+) bytes/.exec(output ?? "")?.[1] ? `${/wrote (\d+) bytes/.exec(output ?? "")![1]} bytes` : undefined };
    case "edit":
      return { glyph: "✎", verb: "Edit", arg: short(s("path")), count: `${(args.edits as unknown[] | undefined)?.length ?? 1} edits` };
    case "grep":
      return { glyph: "∗", verb: "Grep", arg: `"${s("pattern")}"`, count: n(output, "match") };
    case "find":
      return { glyph: "∗", verb: "Find", arg: s("pattern"), count: n(output, "file") };
    case "ls":
      return { glyph: "→", verb: "List", arg: short(s("path")) || ".", count: n(output, "entry", "entries") };
    case "process":
      return { glyph: "◐", verb: "Process", arg: `${s("action")} ${s("title") || s("command") || s("id")}`.trim() };
    case "ask":
      // An agent is opencode's subagent row: a tick when it is done, a spinner while it runs, and
      // its title — the report itself arrives as a message, in the person's own thread.
      return args.to === "agent"
        ? { glyph: state?.pending ? "◐" : "✓", verb: "Agent —", arg: String(args.name ?? "").trim() || s("task"), count: state?.pending ? `running${state.secs ? ` ${state.secs}s` : ""}` : "started" }
        : { glyph: "⇢", verb: "ask", arg: `${String(args.to ?? "")}: ${s("task")}`, count: state?.pending ? "sending" : "queued" };
    case "plan":
      return { glyph: "▤", verb: "Plan", arg: args.done ? `done: ${String(args.done)}` : args.doing ? `doing: ${String(args.doing)}` : `${(args.set as unknown[] | undefined)?.length ?? 0} steps` };
    case "skill":
      return { glyph: "▤", verb: "Skill", arg: s("name") || "list" };
    case "tool_search":
      return { glyph: "∗", verb: "Search tools", arg: `"${s("query")}"`, count: n(output, "tool") };
    case "memory":
      return { glyph: "▤", verb: "Memory", arg: args.forget ? `forget ${String(args.forget)}` : String((args.save as { name?: string } | undefined)?.name ?? "") };
    default: {
      if (tool?.startsWith("kl_")) return { glyph: "~", verb: tool.slice(3).replace(/_/g, " "), arg: String(args.id ?? args.name ?? args.workspace ?? args.repo ?? "") };
      return { glyph: "~", verb: tool ?? "", arg: Object.values(args).filter((v) => typeof v === "string").join(" ") };
    }
  }
}

/** The four an agent may lead its report with; the first word of a reply is the thing to read. */
export const STATUSES = ["DONE_WITH_CONCERNS", "NEEDS_CONTEXT", "BLOCKED", "DONE"] as const;

/**
 * An agent's reply, split into what a person reads at a glance and what they open. The status is
 * the first word by contract (the agent's own identity says so); anything else is all body.
 */
/** `branch fix-login` / `pull ada/api#12` — what an agent left behind for the person to take. */
const LEFT = /\b(?:branch|pushed(?: to)?|pull request|pull|PR)\s+(?:branch\s+)?([\w./#-]{2,60})/i;

export function report(text: string): { status?: string; head: string; body: string; left?: string } {
  const t = String(text).replace(/^\[from agent [^\]]*\]\s*/, "").trim();
  const status = STATUSES.find((s) => t.startsWith(s));
  const rest = (status ? t.slice(status.length).replace(/^[\s—:-]+/, "") : t).trim();
  const [head, ...body] = rest.split("\n");
  return { status, head: head ?? "", body: body.join("\n").trim(), left: LEFT.exec(rest)?.[1] };
}

/** The one line, assembled: `∗ Grep "homepage" (18 matches)`. */
export const render = (l: ToolLine) => `${l.glyph} ${[l.verb, l.arg].filter(Boolean).join(" ")}${l.count ? ` (${l.count})` : ""}`.replace(/\s+/g, " ").trim();
