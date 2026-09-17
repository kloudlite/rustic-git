/**
 * Pure readings of the bench's rows, kept free of Solid and `window` so
 * `node --test` can hold them (bench/test/renderer-rows.test.ts).
 */
export type SessionRow = { id: string; name: string; seq: number; lastActive?: number; archived?: boolean; file?: string; kind?: string };

/** The sidebar's sessions: a workspace or ephemeral thread is the bench's too, but never listed here. */
export const benchSessions = <T extends SessionRow>(rows: T[]): T[] => rows.filter((r) => (r.kind ?? "bench") === "bench");

/** The bench route that opens a tab's thread (idempotent), or undefined: an ephemeral is watched, never driven, so it only reads. */
export const openRoute = (kind: string, ws: string): string | undefined => (kind === "workspace" ? `/workspaces/${ws}/session` : undefined);

/** What a tab says when the bench refuses to open its thread; a 409 `belongs to` is an id clash, not a crash. */
export const openNote = (message: string): string =>
  /belongs to/.test(message) ? `this tab cannot open as a session: ${message}; its history is not shown` : message;

/** A process row as the task page reads it: a lost row found gone at start is lost, never running. */
export function procState(p: { ended?: number; code?: number | null; lost?: true }): "running" | "done" | "failed" | "lost" {
  if (p.lost) return "lost";
  if (p.ended === undefined) return "running";
  return p.code === 0 ? "done" : "failed";
}

/** A process's one-word status line. */
export function procLabel(p: { ended?: number; code?: number | null; lost?: true }): string {
  const s = procState(p);
  return s === "running" ? "running" : s === "lost" ? "lost" : `exited ${p.code}`;
}

/** The bench's 409 on delete names what is in flight; anything else is not a confirm. */
/**
 * The processes of ONE session. `/procs` is the whole bench's table — every session's rows — and
 * the panel drew all of them, so a workspace's dev server appeared under the bench tab. A row with
 * no session at all belongs to nothing and is shown nowhere.
 */
export function procsOf<T extends { session?: string; workspace?: string }>(rows: readonly T[], session: string, workspace?: string): T[] {
  // Processes and background tasks belong to a WORKSPACE, not to a session: every bench session
  // shares the bench's machine, and a workspace's sessions share its own (owner, 2026-09-17). The
  // session is the fallback for a row written before the ledger carried a workspace.
  if (workspace) return rows.filter((p) => (p.workspace ? p.workspace === workspace : p.session === session));
  return rows.filter((p) => p.session === session);
}

/** Which session a selected thing runs its processes under: a workspace tab has its own thread. */
export function sessionOf(sel: { kind: "bench" | "session" | "workspace" | "ephemeral"; id?: string }): string {
  switch (sel.kind) {
    case "session": return sel.id!;
    case "workspace": return `w-${sel.id}`;
    case "ephemeral": return `e-${sel.id}`;
    default: return "bench";
  }
}

export function inFlightItems(message: string): string[] | undefined {
  const at = message.indexOf("in flight: ");
  return at < 0 ? undefined : message.slice(at + 11).split(", ").filter(Boolean);
}

/** pi commands that change what the bench saves; the rest only read or steer a running turn. */
const WRITES = new Set(["prompt", "steer", "follow_up", "new_session", "compact", "set_model"]);

/**
 * Why a pi command must not be sent now, or undefined to send it. One answer
 * for every path (composer, slash, palette, keys), so none fails silently.
 */
export function refusal(cmd: { type: string }, st: { session?: string; connected: boolean; writable: { ok: boolean; reason?: string } }): string | undefined {
  if (!st.session) return "no session yet; start one with /new or the + beside Sessions";
  if (!st.connected) return "not connected to the bench; nothing was sent";
  if (WRITES.has(cmd.type) && !st.writable.ok) return `the bench cannot save right now (${st.writable.reason}); nothing was sent`;
  return undefined;
}


/**
 * A model id as a person says it: `deepseek/deepseek-v4-flash` is not what anybody calls it. The
 * map is short on purpose — anything not in it reads better as its own id than as a guess.
 */
const MODEL_NAMES: Record<string, string> = {
  "deepseek/deepseek-v4-flash": "DeepSeek V4 Flash",
  "deepseek/deepseek-v4": "DeepSeek V4",
  "anthropic/claude-opus-5": "Claude Opus 5",
  "anthropic/claude-sonnet-5": "Claude Sonnet 5",
  "anthropic/claude-haiku-5": "Claude Haiku 5",
  "openai/gpt-5": "GPT-5",
};
/**
 * Names pi itself gave us for `provider/model-id`, filled from `GET /models` when the picker loads
 * (spec §1.3: the owner wants the readable name, not the raw id). A live catalogue beats the static
 * map above, which only ever knew the handful of models that existed when it was written.
 */
const LIVE_MODEL_NAMES = new Map<string, string>();
export function noteModelNames(rows: { id: string; name: string }[]): void {
  for (const r of rows) if (r.id && r.name) LIVE_MODEL_NAMES.set(r.id, r.name);
}

/** Provider slugs as a person writes them; anything else is titlecased from the slug. */
const PROVIDERS: Record<string, string> = { deepseek: "DeepSeek", anthropic: "Anthropic", openai: "OpenAI", google: "Google", openrouter: "OpenRouter", ollama: "Ollama" };

export function displayModel(id: string | undefined): string {
  const t = (id ?? "").trim();
  // "not started" is pi's own status before a child is up: it is not a model, and saying it where
  // the model goes told the owner his session had none (2026-09-17).
  if (!t || /^not started$/i.test(t)) return "no model";
  return LIVE_MODEL_NAMES.get(t) ?? MODEL_NAMES[t] ?? t.split("/").pop() ?? t;
}

/** The provider behind a model id, said as opencode says it — after the name, muted. */
export function displayProvider(id: string | undefined): string | undefined {
  const slug = (id ?? "").split("/")[0].trim();
  if (!slug || !(id ?? "").includes("/")) return undefined;
  return PROVIDERS[slug] ?? slug[0].toUpperCase() + slug.slice(1);
}

/**
 * Which model this thread actually runs, in the order the answer is known: the session's own row
 * first, then the bench's default (`GET /bootstrap`'s `model`, the fleet's `KL_MODEL`), and only
 * with neither is there no model. pi's status line is NOT in the chain — it says "not started"
 * before the child is up, and that is how the bench thread came to claim it had no model
 * (owner, 2026-09-17).
 */
export function modelOfThread(row: string | undefined, benchDefault: string | undefined): string | undefined {
  const clean = (v: string | undefined) => (v && !/^not started$/i.test(v.trim()) ? v.trim() : undefined);
  return clean(row) ?? clean(benchDefault);
}

/**
 * One finished turn's own footer. Everything here is a fact ABOUT THAT TURN, stamped when it
 * ended — never the live line, which made every old message change as soon as the model changed
 * (owner, on the fleet). A turn with no stamp (an older message, a transcript replayed from disk)
 * shows only what it actually has: there is no fallback to whatever is running now.
 */
export type TurnMeta = { mode?: string; model?: string; thinking?: string; effort?: string; duration?: string; interrupted?: true };
export function turnMeta(t: TurnMeta): string {
  const model = t.model ? displayModel(t.model) : undefined;
  return [
    t.mode ? modeParts(t.mode, undefined).mode : undefined,
    model,
    t.model ? displayProvider(t.model) : undefined,
    t.thinking ? `thinking ${t.thinking}` : undefined,
    t.effort ? `effort ${t.effort}` : undefined,
    t.duration,
    t.interrupted ? "Interrupted" : undefined,
  ]
    .filter(Boolean)
    .join(" · ");
}

/** The status row's parts, so each can carry its own weight and colour. */
export type ModeParts = { mode: string; model: string; provider?: string; thinking?: string; effort?: string };
/**
 * Spec §1.3: a segment that does not apply is ABSENT, never shown as `—`. The key is left off
 * entirely rather than set to undefined, so a caller comparing parts sees the same shape the
 * footer draws.
 */
export function modeParts(mode: string, model: string | undefined, thinking?: string, effort?: string): ModeParts {
  const provider = displayProvider(model);
  return {
    mode: mode === "accept-edits" ? "Accept edits" : mode[0].toUpperCase() + mode.slice(1),
    model: displayModel(model),
    ...(provider ? { provider } : {}),
    ...(thinking ? { thinking: `thinking ${thinking}` } : {}),
    ...(effort ? { effort: `effort ${effort}` } : {}),
  };
}


/**
 * `Build · DeepSeek V4 Flash · low` — ONE string, wherever the mode and the model are shown. The
 * turn footer said "✻ Accept edits · no model" while the composer said "⏵⏵ accept edits on
 * (⇧tab to cycle) · no model" (owner, 2026-09-17): two formats for one fact, and a model that was
 * there all along in the session's own row.
 */
export function modeLine(mode: string, model: string | undefined, thinking?: string, effort?: string): string {
  const p = modeParts(mode, model, thinking, effort);
  // The provider is NOT appended to the name: the footer already draws it as its own dim segment,
  // and joining them read as the model said twice — "DeepSeek V4 Flash DeepSeek" (owner, on the
  // fleet). `modeParts().provider` is still there for the callers that draw a segment.
  return [p.mode, p.model, p.thinking, p.effort].filter(Boolean).join(" · ");
}

/**
 * A clone is a CHILD of the workspace it was cut from, not a top-level machine. The api lists both
 * flat, and an agent's clone is named `<parent>-eph-<hex>` — by the parent's ID when the deferred
 * `kl_workspace_clone` path made it, by its NAME when a person did — so both are matched
 * (owner, 2026-09-17: `ws-30b60ec83f5ff77f-eph-5m…` sat at the top level by its id).
 *
 * Pure, and the tree the sidebar and the tab picker both draw.
 */
export type Nested<T> = { row: T; clones: { row: T; agent?: string }[] };

const CLONE = /^(.*)-eph-([a-z0-9]+)$/i;

export function nestWorkspaces<T extends { id: string; name?: string }>(
  rows: readonly T[],
  /** What the bench knows: the agent working in each clone, by the clone's id. */
  agents: Record<string, string> = {},
): Nested<T>[] {
  const byId = new Map(rows.map((w) => [w.id, w] as const));
  const byName = new Map(rows.filter((w) => w.name).map((w) => [w.name!, w] as const));
  const out: Nested<T>[] = [];
  const at = new Map<string, Nested<T>>();
  const parentOf = (w: T): T | undefined => {
    const m = CLONE.exec(w.name ?? w.id) ?? CLONE.exec(w.id);
    if (!m) return undefined;
    const key = m[1];
    const p = byId.get(key) ?? byName.get(key);
    return p && p.id !== w.id ? p : undefined;
  };
  // Parents first, so a clone always has somewhere to go.
  for (const w of rows) {
    if (parentOf(w)) continue;
    const node = { row: w, clones: [] as { row: T; agent?: string }[] };
    at.set(w.id, node);
    out.push(node);
  }
  for (const w of rows) {
    const p = parentOf(w);
    if (!p) continue;
    const node = at.get(p.id);
    const child = { row: w, agent: agents[w.id] };
    // A clone whose parent is not in the list is still a machine of its own, not a lost row.
    if (node) node.clones.push(child);
    else out.push({ row: w, clones: [] });
  }
  return out;
}

/**
 * What a clone's row is called: the AGENT working in it, and nothing else. The owner saw
 * `probe-frontend-ws-nrt…` — the agent's name with the clone's own id trailing it — which is two
 * names for one row and unreadable at any width (2026-09-17).
 */
export function cloneLabel(agent: string | undefined, id: string, parent?: string): string {
  const raw = (agent ?? "").trim();
  const bare = raw
    // The clone's id, however it was appended: `-eph-<hex>`, the parent's id, or the clone's own.
    .replace(/-eph-[a-z0-9]+$/i, "")
    .replace(/-ws-[a-z0-9]{6,}$/i, "")
    .replace(new RegExp(`-?${id.replace(/[.*+?^${}()|[\\]\\\\]/g, "\\\\$&")}$`, "i"), "")
    .replace(parent ? new RegExp(`-?${parent.replace(/[.*+?^${}()|[\\]\\\\]/g, "\\\\$&")}$`, "i") : /$^/, "")
    .replace(/[-_]+$/, "")
    .trim();
  return bare || "clone";
}

/**
 * What a proposal's card is titled: the tool's own verb, in words. "Confirm" said nothing, and the
 * card already carries the sentence underneath (owner's screenshot, 2026-09-17).
 * `kl_workspace_create` → `Create workspace`, `edit` → `Edit`, `bash` → `Run`.
 */
const VERBS: Record<string, string> = { bash: "Run", process: "Run", write: "Write", edit: "Edit", patch: "Patch", read: "Read", ask: "Ask" };
export function proposalHeader(tool: string | undefined, summary = ""): string {
  const t = (tool ?? "").trim();
  if (!t) return summary.split(/[.:]/)[0] || "Confirm";
  if (VERBS[t]) return VERBS[t];
  const parts = t.replace(/^kl_/, "").split("_");
  // `workspace_create` reads as "Create workspace": the verb is last, and it leads.
  const verb = parts.length > 1 ? parts.pop()! : "";
  const subject = parts.join(" ");
  const said = verb ? `${verb} ${subject}` : subject;
  return said.charAt(0).toUpperCase() + said.slice(1);
}

/**
 * The one muted line under a proposal's verb: what it would act on, values only —
 * `new-workspace · nrt · node, bun`. The key names were noise; a person reading
 * "Create workspace" already knows the first value is the name (owner, 2026-09-17).
 */
export function argLine(args: Record<string, unknown> = {}, summary = ""): string {
  const said = Object.entries(args)
    .filter(([k]) => !/^(team|owner|session|from|id)$/.test(k))
    .map(([, v]) => (Array.isArray(v) ? v.join(", ") : v))
    .filter((v) => v !== undefined && v !== null && v !== "" && typeof v !== "object")
    .map(String);
  // Nothing worth showing: the summary minus its own verb, which is already the header.
  return said.length ? said.join(" · ") : summary.replace(/^[A-Z][a-z]+( [a-z]+)? /, "");
}

/**
 * What an exchange row SAYS: who it is with, and the first line of the task. The owner saw
 * `› sent  backend-rust  kl_workspace_create {"name":"backend-rust","packages":["rust"]}` — a
 * platform call published as an exchange, with its arguments as the text (2026-09-17). Platform
 * calls are no longer exchanges at all; this keeps the rows readable whatever reaches them.
 */
export function exchangeText(text: string): string {
  const said = String(text ?? "")
    .replace(/^\[ask \S+ from [^\]]*\] /, "")
    .replace(/^\[reply [^\]]+\]\s*/, "")
    .trim();
  // A tool call that somehow reached the log reads as its verb, never as its JSON.
  const call = /^([a-z_]+)\s+\{[\s\S]*\}$/.exec(said);
  if (call) {
    const verb = call[1].replace(/^kl_/, "").replace(/_/g, " ");
    return verb.charAt(0).toUpperCase() + verb.slice(1);
  }
  return said.split("\n")[0].slice(0, 120);
}

/**
 * A process row's title, derived where it is DRAWN. Rows written before the tool server learned to
 * title them carry the raw command as their name (`cd /home/kl/workspaces/svelte-fro…`), and a
 * ledger written yesterday cannot be fixed by a rule added today (owner, 2026-09-17).
 *
 * Same rule as the tool server's own `procTitle`: a `cd X && Y` reads as `X: Y`.
 */
export function procName(row: { name?: string; command?: string }): string {
  const command = String(row.command ?? "").trim();
  const name = String(row.name ?? "").trim();
  // A name that is just the command again is not a title.
  const titled = name && name !== command && !name.startsWith("cd ");
  if (titled) return name.slice(0, 60);
  const cd = /^cd\s+([^\s;&|]+)\s*(?:&&|;)\s*([\s\S]*)$/.exec(command || name);
  const where = cd ? cd[1].replace(/\/+$/, "").split("/").filter(Boolean).pop() : "";
  const what = (cd ? cd[2] : command || name).trim().split("\n")[0];
  return (where ? `${where}: ${what}` : what).slice(0, 60);
}
