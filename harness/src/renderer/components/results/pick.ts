/**
 * Which card draws a tool's answer. Pure and on its own so it can be tested without a DOM: the
 * components below it only lay out what this hands them.
 *
 * Every `kl_*` tool answers the api's own JSON, and the desktop renders THAT (spec §8) rather than
 * printing the document — the model is told not to repeat what a card shows, so the card has to
 * show it. Anything unrecognised falls back to the generic answer block that was here before: a
 * new route must never turn into a blank panel.
 */
export type Card =
  | { kind: "workspace"; data: Record<string, unknown> }
  | { kind: "environment"; data: Record<string, unknown> }
  | { kind: "quota"; data: Record<string, unknown> }
  | { kind: "history"; data: Record<string, unknown>[] }
  | { kind: "ask"; data: { workspace: string } }
  | { kind: "processes"; data: { id: string; state: string; cmd: string }[] }
  | { kind: "packages"; data: string[] }
  | { kind: "capabilities"; data: { group: string; tools: { name: string; effect: string; summary: string }[] }[] };

const json = (t: string): unknown => {
  try {
    return JSON.parse(t);
  } catch {
    return undefined;
  }
};
const isRow = (v: unknown): v is Record<string, unknown> => !!v && typeof v === "object" && !Array.isArray(v);
/** One document, whether the route answered it alone or as a list of one. */
const one = (v: unknown) => (isRow(v) ? v : Array.isArray(v) && v.length === 1 && isRow(v[0]) ? (v[0] as Record<string, unknown>) : undefined);

const WORKSPACE = /^kl_workspace(_(create|start|stop|push|clone|restore))?$/;
const ENVIRONMENT = /^kl_environment(_(create|start|stop|push|clone|restore|service_add|service_rm))?$/;

export function pickRenderer(tool: string | undefined, output: string | undefined, args?: Record<string, unknown>): Card | undefined {
  if (!tool || !output) return undefined;
  // An ask answers a sentence, not a document: its card is the exchange's own state, which the
  // renderer already has from the bench's `exchange` events.
  if (tool === "kl_workspace_ask") return typeof args?.workspace === "string" ? { kind: "ask", data: { workspace: args.workspace } } : undefined;
  if (tool === "kl_capabilities") return { kind: "capabilities", data: capabilities(output) };
  if (tool === "process") {
    const rows = processes(output);
    return rows.length ? { kind: "processes", data: rows } : undefined;
  }
  const v = json(output);
  if (v === undefined) return undefined;
  if (tool === "kl_pkg_list") return Array.isArray(v) && v.every((x) => typeof x === "string") ? { kind: "packages", data: v as string[] } : undefined;
  if (tool === "kl_quota") return isRow(v) && isRow(v.limit) && isRow(v.used) ? { kind: "quota", data: v } : undefined;
  if (tool === "kl_volume_history" || (tool === "kl_volumes" && Array.isArray(v) && (v as Record<string, unknown>[]).some((r) => isRow(r) && "snapshot" in r)))
    return Array.isArray(v) ? { kind: "history", data: v.filter(isRow) } : undefined;
  const doc = one(v);
  if (!doc) return undefined;
  if (WORKSPACE.test(tool) && ("state" in doc || "packages" in doc)) return { kind: "workspace", data: doc };
  if (ENVIRONMENT.test(tool) && ("services" in doc || "state" in doc)) return { kind: "environment", data: doc };
  return undefined;
}

/** `p1 running npm run dev`, as the process tool prints it. */
export function processes(text: string): { id: string; state: string; cmd: string }[] {
  return text
    .split("\n")
    .map((l) => /^(\S+) (running|exited)(?: \(exit (-?\d+)\))? (.*)$/.exec(l.trim()))
    .filter((m): m is RegExpExecArray => !!m)
    .map((m) => ({ id: m[1], state: m[3] === undefined ? m[2] : `exited ${m[3]}`, cmd: m[4] }));
}

/** The catalogue as `kl_capabilities` prints it: `group:` then `  name [effect] — summary`. */
export function capabilities(text: string): { group: string; tools: { name: string; effect: string; summary: string }[] }[] {
  const out: { group: string; tools: { name: string; effect: string; summary: string }[] }[] = [];
  for (const line of text.split("\n")) {
    const head = /^(\S[^:]*):$/.exec(line);
    if (head) {
      out.push({ group: head[1], tools: [] });
      continue;
    }
    const row = /^\s+(\S+) \[(\w+)\] — (.*)$/.exec(line);
    if (row && out.length) out[out.length - 1].tools.push({ name: row[1], effect: row[2], summary: row[3] });
    else if (line.trim() && out.length && !out[out.length - 1].tools.length) out[out.length - 1].tools.push({ name: line.trim(), effect: "", summary: "" });
  }
  return out.filter((g) => g.tools.length);
}
