/**
 * Pure readings of the bench's rows, kept free of Solid and `window` so
 * `node --test` can hold them (bench/test/renderer-rows.test.ts).
 */
export type SessionRow = { id: string; name: string; seq: number; lastActive?: number; archived?: boolean; file?: string; kind?: string; workspace?: string; tree?: string };

/**
 * An agent session, as the sidebar nests it under the workspace it works in (spec §4.5): the row
 * is the SESSION, the workspace is the one whose pod it runs in, and the tree is its own working
 * directory inside it. An agent is no longer a clone of the machine — there is nothing under the
 * workspace but its trees — so this replaces the `-eph-` clone matching `nestWorkspaces` did.
 *
 * The state is read from the ask it is answering: an open exchange is work in flight, a settled one
 * is what became of it. Nothing here invents a state the bench has not written.
 */
export type AgentRow = { id: string; agent: string; workspace: string; tree?: string; task: string; state: "running" | "waiting" | "done" | "failed" };
export function agentRows(
  sessions: readonly SessionRow[],
  exchanges: readonly { session: string; workspace: string; dir: "in" | "out"; text: string; state: string }[] = [],
): AgentRow[] {
  return sessions
    .filter((s) => s.kind === "ephemeral" && s.workspace && !s.archived)
    .map((s) => {
      const agent = s.id.replace(/^e-/, "");
      // The ask was recorded against the AGENT's name, which is what routes a report back to it.
      const ask = exchanges.filter((e) => e.dir === "out" && e.workspace === agent).at(-1);
      const state: AgentRow["state"] =
        ask?.state === "failed" ? "failed" : ask?.state === "done" ? "done" : ask?.state === "running" ? "running" : "waiting";
      return { id: s.id, agent, workspace: s.workspace!, ...(s.tree ? { tree: s.tree } : {}), task: ask?.text?.split("\n")[0]?.slice(0, 80) ?? agent, state };
    });
}

/** What an agent's row is called in the sidebar: its name and what became of its ask (spec §4.5). */
export const agentLabel = (a: { agent: string; state: string }): string => `${a.agent} · ${a.state}`;

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
 * The `/model` dialog's one grouped list (opencode's `/models` shape): a provider is a header, its
 * models are the rows beneath it. Pure, so the rules that confused the owner are testable:
 *  - only providers that are WIRED and have models are offered — Settings is where a key is added;
 *  - the filter narrows MODELS across every provider, and a header left with none disappears;
 *  - a header is never selectable, so the cursor can only ever rest on a model.
 */
export type PickerRow = { kind: "header"; label: string } | { kind: "model"; id: string; name: string };
export function pickerRows(
  providers: readonly { id: string; label: string; wired: boolean; models: readonly { id: string; name: string }[] }[],
  filter = "",
): PickerRow[] {
  const has = (s: string) => s.toLowerCase().includes(filter.toLowerCase());
  const out: PickerRow[] = [];
  for (const p of providers) {
    if (!p.wired || !p.models.length) continue;
    const models = p.models.filter((m) => !filter || has(m.name) || has(m.id));
    if (!models.length) continue;
    out.push({ kind: "header", label: p.label });
    for (const m of models) out.push({ kind: "model", id: `${p.id}/${m.id}`, name: m.name });
  }
  return out;
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

/**
 * A tree row's two facts, read in the tool server's own words (`crates/ide/src/fs/tree.rs:14`):
 * `kind` is `dir` / `file` / `symlink`, and `ignored` is what the global gitignore and `.git`
 * already cover. Reading a `dir` boolean we had invented is why every entry drew as a file
 * (owner, 2026-09-18).
 */
export const isDir = (e: { kind?: string }): boolean => e.kind === "dir";

/**
 * What a workspace's own ignore rules cover, for a tool server that did not say so itself. An
 * ignored entry is DIMMED where it sits; it is never grouped or hidden ("why showing ignored
 * separately" — owner, 2026-09-18).
 */
const NOISE = new Set([".git", ".cache", "graft", ".direnv", "node_modules", ".pnpm-store", "dist", "target", ".venv"]);
export const dimmed = (e: { name: string; ignored?: boolean }): boolean => e.ignored === true || NOISE.has(e.name);

/**
 * A change, as the tool server writes it (`crates/ide/src/fs/git.rs:12`): two columns, the index
 * and the worktree, each a git porcelain letter, plus `renamed_from`. It does NOT send a `status`
 * field — reading one is why the CHANGES tab was empty for a workspace with real changes
 * (owner, 2026-09-18).
 */
export type FsChange = { path: string; index?: string; worktree?: string; renamed_from?: string };

/** One letter for a row: the worktree column when it says something, else the index's. */
export function changeLetter(c: FsChange): string {
  const worktree = (c.worktree ?? ".").trim();
  const index = (c.index ?? ".").trim();
  const said = worktree && worktree !== "." ? worktree : index;
  return said && said !== "." ? said.toUpperCase() : "M";
}

/** What each letter means for colour, in the tokens the CHANGES list already uses. */
export const STATUS_TONE: Record<string, string> = {
  M: "text-modified",
  A: "text-created",
  D: "text-deleted line-through",
  R: "text-accent",
  "?": "text-created/70",
  U: "text-warning",
};

/** The tone for one row: ignored is dim whatever else it is, then the git letter's own. */
export const rowTone = (letter: string | undefined, ignored?: boolean, committed?: boolean): string =>
  ignored ? "text-subtle" : letter ? (STATUS_TONE[letter] ?? "") : committed ? COMMITTED_TONE : "";

/** The badge a row shows at its end: `?` reads as `U` for untracked, as source control does. */
export const statusBadge = (letter: string | undefined): string | undefined => (letter === "?" ? "U" : letter || undefined);

/**
 * A path that is not different from the branch but WAS changed by one of this session's commits.
 * The tree tints it softer than an uncommitted change: it is history, not work in progress.
 */
export const COMMITTED_TONE = "text-modified/60";
export const committedPaths = (commits: readonly { files: { path: string }[] }[]): Set<string> =>
  new Set(commits.flatMap((c) => c.files.map((f) => f.path)));

/**
 * Paths a listing must show that the tree cannot: a DELETED file is not on disk, so it is
 * synthesised into its own directory's listing from what `/fs/changes` says (owner, 2026-09-18).
 */
export function deletedIn(changes: readonly FsChange[], dir?: string): { name: string; letter: string }[] {
  const prefix = dir ? `${dir}/` : "";
  return changes
    .filter((c) => changeLetter(c) === "D" && c.path.startsWith(prefix) && !c.path.slice(prefix.length).includes("/"))
    .map((c) => ({ name: c.path.slice(prefix.length), letter: "D" }));
}
