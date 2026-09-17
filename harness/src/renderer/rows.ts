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
export function procsOf<T extends { session?: string }>(rows: readonly T[], session: string): T[] {
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
/** Provider slugs as a person writes them; anything else is titlecased from the slug. */
const PROVIDERS: Record<string, string> = { deepseek: "DeepSeek", anthropic: "Anthropic", openai: "OpenAI", google: "Google", openrouter: "OpenRouter", ollama: "Ollama" };

export function displayModel(id: string | undefined): string {
  const t = (id ?? "").trim();
  // "not started" is pi's own status before a child is up: it is not a model, and saying it where
  // the model goes told the owner his session had none (2026-09-17).
  if (!t || /^not started$/i.test(t)) return "no model";
  return MODEL_NAMES[t] ?? t.split("/").pop() ?? t;
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

/** The status row's parts, so each can carry its own weight and colour. */
export type ModeParts = { mode: string; model: string; provider?: string; level?: string };
export function modeParts(mode: string, model: string | undefined, level?: string): ModeParts {
  return {
    mode: mode === "accept-edits" ? "Accept edits" : mode[0].toUpperCase() + mode.slice(1),
    model: displayModel(model),
    provider: displayProvider(model),
    level,
  };
}


/**
 * `Build · DeepSeek V4 Flash · low` — ONE string, wherever the mode and the model are shown. The
 * turn footer said "✻ Accept edits · no model" while the composer said "⏵⏵ accept edits on
 * (⇧tab to cycle) · no model" (owner, 2026-09-17): two formats for one fact, and a model that was
 * there all along in the session's own row.
 */
export function modeLine(mode: string, model: string | undefined, level?: string): string {
  const p = modeParts(mode, model, level);
  return [p.mode, [p.model, p.provider].filter(Boolean).join(" "), p.level].filter(Boolean).join(" · ");
}
