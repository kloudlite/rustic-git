import { Type } from "typebox";
import { StringEnum } from "@earendil-works/pi-ai";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { processActionIssues } from "../bench/src/operations/arguments.ts";
import type { JsonValue } from "../bench/src/operations/shape.ts";
import { call, dispatchWithPolicy, propose, tellItWhereItStands, toolResultOf } from "./kloudlite.ts";

/**
 * A session's hands on ONE machine. The agent runs in the bench pod; these seven
 * tools run in that machine, as calls to its tool server (`kl ide serve`,
 * crates/ide). The names and parameters are the agent's own, so the model sees
 * the tools it knows; the work is the tool server's.
 *
 * Two ways in, and never a third — no session can name another workspace here,
 * which is why asking one (`kl_workspace_ask`) is a queue and not a tool call:
 *  - a WORKSPACE session: `KL_TOOLS_WORKSPACE` names the workspace, and the
 *    address comes from /v1, which answers its owner and nobody else, asked
 *    again after a connection error because a restarted pod has a new IP;
 *  - a BENCH session: `KL_TOOLS_ADDRESS` is its own workspace container's tool
 *    server on loopback (the bench pod's two containers share a network
 *    namespace), so the bench has hands on its own machine and no other.
 */
const MAX_EXEC_MS = 600_000;
/** Past this, the model is reminded that it is the only one reading. */
const LONG_OUTPUT_LINES = 40;
const escapeRe = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
const ADDR_RE = /^([A-Za-z0-9.-]+:\d{1,5}|\[[0-9a-fA-F:]+\]:\d{1,5})$/;
function validAddress(addr: string, source: string): string {
  if (!ADDR_RE.test(addr)) throw new Error(`${source} is not a host:port address: ${addr}`);
  return addr;
}

/** A child older than this stops joining the trace it was spawned in: its calls start fresh traces
 *  at the tool server, so one long session is not one waterfall forever. */
export const TRACE_MAX_AGE_S = 3600;

/** The bench's trace, handed down at spawn (`KL_TRACEPARENT`, `KL_PROBE`). */
export function traceHeaders(env: NodeJS.ProcessEnv = process.env, ageS = process.uptime()): Record<string, string> {
  if (!env.KL_TRACEPARENT || ageS >= TRACE_MAX_AGE_S) return {};
  return { traceparent: env.KL_TRACEPARENT, ...(env.KL_PROBE === "1" ? { "x-kloudlite-probe": "1" } : {}) };
}

/**
 * What a shell in the workspace may not go looking for. The fleet (2026-09-17) watched a model with
 * no tool for a request go behind the tools instead: `read /opt/harness/pi/catalog.ts`, `grep -a`
 * for strings in `/usr/local/bin/kl`, its own `.bench/` session logs grepped for API paths, and a
 * node script reading `KL_TOOL_TOKEN_FILE` to call `/v1` by hand. Prose in a system prompt did not
 * stop it. This does, and it is deliberately about the PLATFORM's own back doors only — ordinary
 * work, `kl pkg list` and every other CLI included, is untouched.
 */
const REFUSAL = "refused: the platform is reached only through kl_* tools; if none fits, say so to the person";
const BACK_DOORS: RegExp[] = [
  /KL_TOOL_TOKEN_FILE|KL_API_URL/,
  /\/etc\/kloudlite|\/opt\/harness/,
  // The `kl` BINARY, not the command: `kl pkg list` is ordinary use, reading it as bytes is not.
  /\/usr\/local\/bin\/kl\b/,
  /\b(strings|xxd|od|hexdump)\b[^|;&]*\bkl\b/,
  /\bgrep\b[^|;&]*\s-\w*a\w*\b[^|;&]*\bkl\b/,
  // Its own session store: the transcripts hold every API path the harness ever used, and the token.
  /(^|[\s"'`=:/])\.bench\//,
  // A `/v1/` URL at a REAL hostname is the platform's api. A service inside an environment is a
  // bare name (CoreDNS), so `curl http://api:8080/v1/orders` is a person's own backend, not this one.
  /https?:\/\/[^\s"'`/]*\.[^\s"'`/]*\/v1\//,
];

/**
 * A command that only waits. The lifecycle tools poll the platform themselves now, so a model that
 * sleeps is burning a turn to learn nothing — it was doing `sleep 20; echo waited` after every
 * create (the fleet, 2026-09-17). A sleep INSIDE real work (`npm test; sleep 1; curl …`) is not
 * this: only a command whose every step is a timer or an echo is refused.
 */
const WAITING = "refused: tools wait for you; ask the tool again instead of sleeping";
export function onlyWaits(cmd: string): boolean {
  const steps = cmd.split(/&&|\|\||;/).map((x) => x.trim()).filter(Boolean);
  if (!steps.length || !steps.some((x) => /^(sleep|timeout)\b/.test(x))) return false;
  return steps.every((x) => /^(sleep|timeout)\b/.test(x) || /^echo\b/.test(x) || /^(true|:)$/.test(x) || /^date\b/.test(x));
}

/**
 * `kl env switch`, `kl pkg add`, `kl container build` — the CLI a person uses in their own shell,
 * which a model reaches for because it is the verb it knows. It stays ALLOWED (it is their machine
 * and their CLI), but the answer carries one line pointing at the tool that does the same thing
 * with a card, a proposal and a wait attached.
 */
const KL_VERB = /(^|[\s;&|])kl\s+(env|pkg|container|workspace|snapshot|repo)\b\s*(\w+)?/;
export function shellNote(cmd: string): string | undefined {
  const m = KL_VERB.exec(cmd);
  return m ? `note: tool_search '${[m[2], m[3]].filter(Boolean).join(" ")}' has a tool for this` : undefined;
}

/** The reason a command or path is refused, or undefined when there is none. */
export function forbidden(s: unknown): string | undefined {
  const t = Array.isArray(s) ? s.join(" ") : typeof s === "string" ? s : "";
  if (!t) return undefined;
  let host = "";
  try {
    host = new URL(process.env.KL_API_URL ?? "").host;
  } catch {
    /* no api url configured: nothing to name */
  }
  if (host && t.includes(host)) return REFUSAL;
  if (BACK_DOORS.some((re) => re.test(t))) return REFUSAL;
  return onlyWaits(t) ? WAITING : undefined;
}

/** Everything a call could reach with: what a shell would run, and what a path-taking tool would open. */
const reaches = (name: string, p: Record<string, any>): unknown[] =>
  name === "bash" || (name === "process" && p.action === "start") ? [p.command] : [p.path, p.pattern, p.glob];

export type IdeCall = { tool: string; args: Record<string, any> };
/** Tool-server calls whose effect the `/procs` table has to be re-read after. */
const PROCESS_TOOLS = new Set(["process_kill", "process_write", "process_list"]);

/**
 * What a person should see instead of an argv. The model may give a title; otherwise the command
 * is read: the directory it runs in and the program it runs — "svelte-app: npm run dev" — because
 * "cd /home/kl/workspaces/svelte-app && npm run dev" tells a person nothing they wanted.
 */
export function procTitle(cmd: string, title?: string): string {
  if (title?.trim()) return title.trim().slice(0, 60);
  const c = String(cmd).trim();
  const cd = /^cd\s+([^\s;&|]+)\s*(?:&&|;)\s*(.*)$/s.exec(c);
  const where = cd ? cd[1].replace(/\/+$/, "").split("/").filter(Boolean).pop() : "";
  const what = (cd ? cd[2] : c).trim().split("\n")[0];
  return (where ? `${where}: ${what}` : what).slice(0, 60);
}

/** A row of the bench's `/procs` table, as `harness:procs` carries it (ledger.ts's `ProcRow`). */
type ProcLine = { id: string; name: string; command: string; started: number; ended?: number; code?: number | null };

/**
 * `harness:procs` is the one fire-and-forget channel an extension has to the harness, and the
 * bench folds it straight into the table the desktop's Processes panel draws. The tool server is
 * the truth: this publishes its whole list, so a process that exited between calls settles too.
 */
/** Titles this session gave its own processes, by id: the tool server keeps the command, not the name. */
const titles = new Map<string, string>();

async function publishProcs(server: ToolServer, ctx: { ui?: { setWidget?: (k: string, lines: string[]) => void } } | undefined, signal?: AbortSignal) {
  if (!ctx?.ui?.setWidget) return;
  try {
    const r = await server.call({ tool: "process_list", args: {} }, signal);
    const rows: ProcLine[] = ((r.body?.processes ?? []) as { id: string; cmd: string; started_at?: string; state?: string; exit_code?: number | null }[]).map((x) => ({
      id: x.id,
      name: procTitle(String(x.cmd), titles.get(x.id)),
      command: String(x.cmd),
      started: Date.parse(x.started_at ?? "") || Date.now(),
      ...(x.state === "exited" ? { ended: Date.now(), code: x.exit_code ?? null } : {}),
    }));
    ctx.ui.setWidget("harness:procs", [JSON.stringify(rows)]);
  } catch {
    /* the table is a view; a failed refresh is not a failed tool call */
  }
}
type Result = { content: { type: "text"; text: string }[]; isError: boolean };
const text = (t: string, isError = false): Result => ({ content: [{ type: "text", text: t || "(no output)" }], isError });

export function toIde(name: string, p: Record<string, any>): IdeCall {
  switch (name) {
    case "read":
      return { tool: "read", args: { path: p.path, offset: p.offset, limit: p.limit } };
    case "write":
      return { tool: "write", args: { path: p.path, content: p.content } };
    case "edit":
      return { tool: "edit", args: { path: p.path, edits: (p.edits ?? []).map((e: { oldText: string; newText: string }) => ({ old: e.oldText, new: e.newText })) } };
    case "bash":
      // A background command is a PROCESS on the tool server: it outlives the turn and the answer
      // is its id, not its output. Same tool, because "run this in the background" is the same wish.
      return p.background
        ? { tool: "exec", args: { cmd: p.command, detach: true } }
        : { tool: "exec", args: { cmd: p.command, timeout_ms: Math.min(MAX_EXEC_MS, (p.timeout ?? 120) * 1000) } };
    // Code and containers, run in THIS machine: a clone puts the work where the hands are, and a
    // build runs where the context is. All of it is the workspace's own shell, not a second path.
    case "kl_repo_clone": {
      const [owner, name] = String(p.repo).split("/");
      if (!owner || !name || name.includes("/")) throw new Error(`repository ${p.repo} is not owner/name`);
      const host = gitSshHost();
      // argv, never a shell string: a repo or a directory can then never splice a second command.
      return { tool: "exec", args: { cmd: ["git", "clone", `ssh://git@${host}/${owner}/${name}.git`, ...(p.dir ? [String(p.dir)] : [])], timeout_ms: MAX_EXEC_MS } };
    }
    case "kl_container_build":
      return { tool: "exec", args: { cmd: ["kl", "container", "build", "-t", String(p.tag), ...(p.dockerfile ? ["-f", String(p.dockerfile)] : []), String(p.context ?? ".")], detach: true } };
    case "kl_container_push":
      return { tool: "exec", args: { cmd: ["kl", "container", "push", String(p.from), String(p.to)], timeout_ms: MAX_EXEC_MS } };
    case "kl_images":
      return { tool: "exec", args: { cmd: ["kl", "container", "images", ...(p.owner ? [String(p.owner)] : [])], head: 200 } };
    case "process":
      switch (p.action) {
        case "start":
          return { tool: "exec", args: { cmd: p.command, detach: true } };
        case "list":
          return { tool: "process_list", args: {} };
        case "logs":
          // ONE number for the model, two cursors underneath: stdout's and stderr's (81621d02).
          // `next` is the pair packed back together, so the model hands back what it was given and
          // neither stream is re-read — a build's stderr used to come back whole on every poll.
          return { tool: "process_output", args: { id: p.id, ...splitCursor(p.since) } };
        case "stop":
          return { tool: "process_kill", args: { id: p.id, signal: p.signal } };
        case "write":
          return { tool: "process_write", args: { id: p.id, data: p.data } };
        case "watch":
          // Handled by the harness, not the tool server: the bench polls and tells this session.
          return { tool: "process_list", args: {} };
        default:
          throw new Error(`no process action ${p.action}`);
      }
    case "grep":
      return { tool: "grep", args: { pattern: p.literal ? escapeRe(p.pattern) : p.pattern, cwd: p.path, glob: p.glob, ignore_case: p.ignoreCase, context: p.context, max: p.limit } };
    case "find":
      return { tool: "glob", args: { pattern: p.pattern, cwd: p.path } };
    case "ls":
      return { tool: "exec", args: { cmd: ["ls", "-1Ap", "--", p.path ?? "."], head: p.limit ?? 500 } };
    default:
      throw new Error(`no workspace tool ${name}`);
  }
}

/**
 * The two output offsets as ONE number the model carries. A process has a cursor per stream
 * (`since`/`next` for stdout, `since_err`/`next_err` for stderr, 81621d02), and asking a model to
 * keep two would be two things to get wrong — so stderr's rides in the high half. The packing is
 * ours alone: `next` is opaque to the model, which only ever hands back what it was given.
 */
const ERR_SHIFT = 2 ** 32;
export const joinCursor = (out: number, err: number): number => err * ERR_SHIFT + out;
export const splitCursor = (cursor: unknown): { since: number; since_err: number } => {
  const n = Math.max(0, Math.floor(Number(cursor) || 0));
  return { since: n % ERR_SHIFT, since_err: Math.floor(n / ERR_SHIFT) };
};

/**
 * Where the git fleet answers ssh. `WS_GIT_SSH_HOST` is the agent's own name for it and the pod is
 * given `KL_GIT_SSH_HOST` — if a build has not got it yet this refuses by NAME rather than guessing
 * a hostname, because a clone from the wrong host is a confusing failure, not an obvious one.
 */
export function gitSshHost(env: NodeJS.ProcessEnv = process.env): string {
  const h = env.KL_GIT_SSH_HOST || env.WS_GIT_SSH_HOST;
  if (!h) throw new Error("this machine was not told where git lives (KL_GIT_SSH_HOST); clone by hand with the URL from the repository page, or ask an admin to set it");
  return h;
}

export function fromIde(name: string, status: number, body: any, limit?: number): Result {
  // The tool server's own refusal is the whole answer: never the body around it.
  if (status >= 400) return text(String(body?.error ?? `the tool server answered ${status}`), true);
  switch (name) {
    case "read":
      return body.binary ? text(`${body.path}: binary, ${body.size} bytes`) : text(body.content + (body.truncated ? `\n[${body.total_lines} lines in all; page with offset]` : ""));
    case "write":
      return text(`wrote ${body.bytes} bytes to ${body.path}`);
    case "edit":
      return text(`applied ${body.applied} edit(s) to ${body.path}`);
    case "kl_repo_clone":
    case "kl_container_push":
    case "kl_images":
    case "kl_container_build":
    case "process":
    case "bash":
    case "ls": {
      // Every detached shape answers by its own fields, so one branch reads them all.
      if (body.id && body.exit_code === undefined) return text(`started in the background as process ${body.id}; read it with process logs`);
      if (Array.isArray(body.processes))
        return text(body.processes.map((x: { id: string; state: string; exit_code?: number | null; cmd: string }) => `${x.id} ${x.state}${x.exit_code === null || x.exit_code === undefined ? "" : ` (exit ${x.exit_code})`} ${x.cmd}`).join("\n") || "nothing running");
      if (body.next !== undefined) {
        const out = [body.stdout, body.stderr].filter(Boolean).join("\n").trim();
        const dropped = Number(body.dropped ?? 0) + Number(body.dropped_err ?? 0);
        const next = joinCursor(Number(body.next ?? 0), Number(body.next_err ?? 0));
        return text(`${out}\n[${body.state}${body.exit_code === null || body.exit_code === undefined ? "" : ` exit ${body.exit_code}`}; next ${next}${dropped ? `, ${dropped} bytes dropped` : ""}]`.trim());
      }
      if (body.bytes !== undefined && body.path === undefined) return text(`wrote ${body.bytes} bytes to its stdin`);
      if (body.state !== undefined && body.exit_code === undefined && body.stdout === undefined) return text(`the process is ${body.state}`);
      const out = [body.stdout, body.stderr].filter(Boolean).join("\n").trim();
      if (body.timed_out) return text(`${out}\n[timed out]`.trim(), true);
      return body.exit_code === 0 ? text(out) : text(`${out}\n[exit ${body.exit_code}]`.trim(), true);
    }
    case "grep":
      return text(
        (body.matches ?? [])
          .map((m: { path: string; line: number; text: string; context?: string }) => `${m.path}:${m.line}: ${m.text}` + (m.context ? `\n${m.context}` : ""))
          .join("\n") + (body.truncated ? "\n[truncated]" : ""),
      );
    case "find":
      return text((body.paths ?? []).slice(0, limit ?? 1000).join("\n"));
    default:
      return text(JSON.stringify(body));
  }
}

/**
 * One workspace's tool server, as a client. pi runs the sibling tool calls of an assistant message
 * CONCURRENTLY (extensions.md: "in the default parallel tool execution mode, sibling tool calls
 * from the same assistant message are preflighted sequentially, then executed concurrently"), so
 * nothing here may serialise them: there is no queue and no in-flight promise, only the address
 * cache, which two concurrent calls may fill twice and that costs one extra /v1 GET.
 */
export class ToolServer {
  private workspace: string;
  private resolve: (ws: string) => Promise<string>;
  private address?: string;
  /** The workspace's token, resolved beside its address and sent on every call. Never logged. */
  private auth?: ToolsAuth;
  /** The tree of that workspace this session works in; absent is the workspace itself. */
  private tree?: string;
  constructor(workspace: string, resolve: (ws: string) => Promise<string>, tree?: string) {
    this.workspace = workspace;
    this.resolve = resolve;
    this.tree = tree;
  }
  async call(c: IdeCall, signal?: AbortSignal): Promise<{ status: number; body: any }> {
    // PINNED, not defaulted: the bench decides which tree a session acts on, and a call whose
    // arguments name another one — main included — is rewritten before it is sent (spec §4.4).
    // The tool server confines for itself; this is the half that says WHICH tree to confine to.
    if (this.tree) c = { ...c, args: { ...c.args, tree: this.tree } };
    // A stale address (a restarted pod, or a workspace not yet ready) surfaces as either a
    // connection failure here or a 409 from the tool server itself; both clear the cached
    // address and ask /v1 once more before giving up, same as a 409 from /v1's own answer
    // (thrown by resolve()) does.
    for (let attempt = 0; ; attempt++) {
      try {
        // The injected `resolve` is what tests and the bench pass; it answers an address only, so
        // the token is resolved beside it and the two are kept together.
        if (!this.address) {
          this.address = await this.resolve(this.workspace);
          // The token comes from the SAME answer `resolveFromApi` already read — `toolsAuth` caches
          // it — so this costs no second request. A resolver that answers an address only (a test,
          // a fixed KL_TOOLS_ADDRESS) simply leaves the cache empty and no header is sent.
          this.auth = authCached(this.workspace);
        }
      } catch (e) {
        this.address = undefined;
        this.auth = undefined;
        if (attempt > 0) throw e;
        continue;
      }
      const at = this.address;
      try {
        // The bench's trace, handed down at spawn: pi's RPC protocol has no field for it per call.
        const headers: Record<string, string> = { "content-type": "application/json", ...traceHeaders(), ...toolsHeader(this.auth) };
        const r = await fetch(`http://${at}/tools/${c.tool}`, { method: "POST", headers, body: JSON.stringify(c.args), signal });
        // The keys beat re-mints the workspace token, so a 401 is a STALE token, not a refusal:
        // resolve once more and try again. A second 401 is a plain error, and never the token.
        if (r.status === 401 && attempt === 0) {
          await r.body?.cancel().catch(() => undefined);
          forgetToolsAuth(this.workspace);
          this.auth = await toolsAuth(this.workspace, true).catch(() => undefined);
          continue;
        }
        const body = await r.json().catch(() => ({ error: `the tool server answered ${r.status} without JSON` }));
        if (r.status === 401) return { status: 401, body: { error: "the workspace refused this call; its token was not accepted" } };
        if (r.status === 409) {
          this.address = undefined;
          if (attempt > 0) return { status: r.status, body };
          continue;
        }
        return { status: r.status, body };
      } catch (e) {
        if (signal?.aborted) throw e;
        this.address = undefined;
        // Never the address and never fetch's own words: the model repeated "did not answer at
        // 127.0.0.1:7788: fetch failed" to the person (owner, 2026-09-18). The detail is stderr's.
        if (attempt > 0) {
          console.error(`workspace ${this.workspace} did not answer at ${at}: ${(e as Error).message}`);
          throw new Error(`the workspace's tools did not answer; is it running?`);
        }
      }
    }
  }
}

/**
 * Where a workspace's tool server is, and the token it now requires. Every `/tools/*`, `/fs/*` and
 * `/stream/*` call carries `Authorization: Bearer <token>`; `/healthz` is open, and ttyd (7790) is
 * a separate server that takes none.
 *
 * The token NEVER reaches the model, the renderer, a log line or a tool result — it lives in this
 * cache and goes out as a header, nowhere else. `token` is optional so the harness works against a
 * tool server that does not require one yet.
 */
export type ToolsAuth = { address: string; token?: string };

const authCache = new Map<string, ToolsAuth>();

/** The header a tool-server call carries, and nothing when there is no token to send. */
export const toolsHeader = (a: ToolsAuth | undefined): Record<string, string> => (a?.token ? { authorization: `Bearer ${a.token}` } : {});

/**
 * `{address, token}` for a workspace, cached. `fresh` re-asks `/v1` — the keys beat re-mints the
 * token, so a 401 is answered by resolving once more rather than by failing the call.
 */
export async function toolsAuth(ws: string, fresh = false): Promise<ToolsAuth> {
  if (!fresh) {
    const had = authCache.get(ws);
    if (had) return had;
  }
  const at = await resolveAuthFromApi(ws);
  authCache.set(ws, at);
  return at;
}

/** What is already known, without asking `/v1`: a resolve has usually just filled this. */
export const authCached = (ws: string): ToolsAuth | undefined => authCache.get(ws);

/** Drops a workspace's cached address and token: the next call resolves again. */
export const forgetToolsAuth = (ws: string) => void authCache.delete(ws);

async function resolveAuthFromApi(ws: string): Promise<ToolsAuth> {
  // A laptop points every workspace session at one tool server, such as the local end of
  // `kl-connect ws ide`; it carries its own token in the env when it needs one.
  if (process.env.KL_TOOLS_ADDRESS)
    return { address: validAddress(process.env.KL_TOOLS_ADDRESS, "KL_TOOLS_ADDRESS"), ...(process.env.KL_TOOLS_TOKEN ? { token: process.env.KL_TOOLS_TOKEN } : {}) };
  if (!process.env.KL_TEAM) throw new Error("KL_TEAM is not set");
  const r = await call("GET", `/v1/workspaces/${encodeURIComponent(ws)}/tools?team=${encodeURIComponent(process.env.KL_TEAM)}`);
  const d = r.data as { address?: string; token?: string; error?: string } | string | null;
  if (r.status === 200 && d && typeof d === "object" && d.address)
    return { address: validAddress(d.address, "workspace address"), ...(typeof d.token === "string" && d.token ? { token: d.token } : {}) };
  throw new Error(d && typeof d === "object" && d.error ? d.error : `workspace ${ws}: ${typeof d === "string" ? d : r.status}`);
}

export async function resolveFromApi(ws: string): Promise<string> {
  // One answer serves both: the address this returns and the token cached beside it.
  const at = await resolveAuthFromApi(ws);
  authCache.set(ws, at);
  return at.address;
}

/**
 * What a session's HANDS ask about first.
 *
 * A `kl_*` write has always been proposed — the card in place of the composer, the person's yes —
 * and the tools that change this machine's own files and run its commands did not (owner,
 * 2026-09-18: "it should follow the same rules when mutating states and editing files"). They do
 * now, through the same path, so one rule covers a workspace created and a file written.
 *
 * Reads never ask (`read`, `grep`, `find`, `ls`, and `process logs`/`list`): nothing changes, and a
 * card in front of every read is a card nobody reads. Plan mode does not reach here at all — those
 * tools are not active in it (`PLAN_TOOLS`), which is a refusal before a call is made.
 */
const ASKS = new Set(["write", "edit", "patch", "bash", "kl_repo_clone", "kl_container_build", "kl_container_push"]);
/** Of `process`, only what starts, stops or writes to something; reading its logs is a read. */
const PROCESS_ASKS = new Set(["start", "stop", "write", "kill"]);

export function mutates(name: string, p: Record<string, any>): boolean {
  if (name === "process") return PROCESS_ASKS.has(String(p.action ?? ""));
  return ASKS.has(name);
}

/** The line the person reads on the card: the path for a file, the command for anything that runs. */
export function askLine(ws: string, name: string, p: Record<string, any>): string {
  switch (name) {
    case "write":
      return `Write ${p.path} in ${ws}`;
    case "edit":
      return `Edit ${p.path} in ${ws}`;
    case "patch":
      return `Patch ${ws}`;
    case "bash":
      return `Run in ${ws}: ${String(p.command ?? "").split("\n")[0].slice(0, 120)}`;
    case "process":
      if (p.action === "start") return `Run in ${ws}: ${String(p.command ?? "").split("\n")[0].slice(0, 120)}`;
      return `${p.action === "write" ? "Write to" : "Stop"} process ${p.id} in ${ws}`;
    default:
      return `${name} in ${ws}`;
  }
}

/** What the card shows under that line: enough to judge it by, never the whole file. */
export function askPreview(name: string, p: Record<string, any>): string | undefined {
  const cut = (t: string, n = 2_000) => (t.length > n ? `${t.slice(0, n)}\n…` : t);
  if (name === "write") return cut(String(p.content ?? ""));
  if (name === "edit")
    return cut(
      ((p.edits as { oldText?: string; newText?: string }[] | undefined) ?? [])
        .map((e) => `${String(e.oldText ?? "").split("\n").map((l) => `- ${l}`).join("\n")}\n${String(e.newText ?? "").split("\n").map((l) => `+ ${l}`).join("\n")}`)
        .join("\n\n"),
    );
  if (name === "patch") return cut(String(p.diff ?? p.patch ?? ""));
  if (name === "bash" || (name === "process" && p.action === "start")) return String(p.command ?? "");
  return undefined;
}

export default function (pi: ExtensionAPI) {
  // A fixed address is a machine of its own (the bench's); `KL_WORKSPACE_ID` is only its name.
  const ws = process.env.KL_TOOLS_WORKSPACE ?? (process.env.KL_TOOLS_ADDRESS ? (process.env.KL_WORKSPACE_ID ?? "this machine") : undefined);
  if (!ws) return;
  // A workspace session's identity line is this one; the bench's is `kloudlite.ts`'s, which knows
  // about the rest of its hands and would otherwise be replaced by whichever extension loaded last.
  if (process.env.KL_TOOLS_WORKSPACE) {
    tellItWhereItStands(
      pi,
      [
        `You are the Kloudlite harness, working inside workspace ${ws}.`,
        "",
        ...(process.env.KL_FORK === "1"
          ? [
              // A read-only fork: it answers one question about this workspace and is discarded.
              "You are answering ONE question about this workspace, from a read-only copy of it. You can read, grep, find and list; you cannot change anything, and nothing you do affects the session you were copied from.",
              "Answer in the short standup shape: the answer first, then where you found it (file names, not contents). No code, no command output, at most 8 lines.",
              "",
            ]
          : []),
        ...(process.env.KL_EPHEMERAL === "1"
          ? [
              // An agent answers once, to somebody who cannot see what it did. The status is the
              // first thing they read, and it is the difference between "take it" and "look again".
              "You are an AGENT: one task, given in full at the start, and one report at the end. Whoever sent it cannot see your work — only your final message.",
              // The numbers are the tool server's, handed to every command it runs as `PORT` and
              // `KL_PORT_RANGE` (spec §4.6); repeating them here would be a second copy to go stale.
              "You have a block of ports of your own: a command you run is given PORT and KL_PORT_RANGE, and anything outside that block belongs to somebody else. Bind what $PORT says, never a number you picked.",
              "You have your own working directory, cut from this workspace's: your own copy of every file, with the caches already warm. Do the task there. When done, commit on a branch named after you and push it (or open a pull request through the tools), then report with the branch or pull. Your directory and this transcript stay until the person closes you, so nothing you did is lost if you are blocked.",
              "End with a report in this shape, leading with one of these four:",
              "DONE — it is done and verified. DONE_WITH_CONCERNS — done, but say what worries you. NEEDS_CONTEXT — you cannot finish without something only they have; say exactly what. BLOCKED — something stops you; say what and what you tried.",
              "Then: one line on what you did, the commits or files if any, a one-line test summary, and concerns. A thing you changed but could not verify is \"changed, unverified\".",
              "",
            ]
          : []),
        // Spec §3.5, verbatim: a session knows its tree, not the container it is served from.
        "You work in one working directory. Every path you give or receive is relative to it. Do not explore, describe or depend on where that directory sits on a machine, what is beside it, or how the machine is laid out; none of that is yours, and tools refuse it. If a task seems to need a path outside your directory, say so in your reply instead. Never repeat a path a tool printed that starts with a slash.",
        "",
        "Packages are nixpkgs attributes, not language names: rustc and cargo for Rust, nodejs_22 for Node, go, python3, bun, pnpm, jdk21, gcc. `attr@version` pins one.",
        "",
        // §3.7: the asking session holds what things ARE; this one holds how they are done. The
        // harness shapes a reply either way, so writing it in the shape is writing it once.
        "When you answer an ask, answer in four fragments and nothing else: the outcome (done / blocked / needs the person), what changed FOR THE PERSON in capability terms (\"GET /version returns the service version\"), the `contracts:` line, and what is needed next if anything. Never a file, a path, a command, a digest or a line of code — the asking session cannot act on those and they stay here in your own transcript.",
        "",
        // §3.8: the asking session waits between reports and does not poll, so the decision has to
        // be said out loud or it is waiting on silence.
        "Every message you send another session is terse: their words, your fragment, nothing else. No preamble, no restating what they already hold — the asking session has the architecture and the contracts, you have the tree.",
        "An ask is a conversation. Your FIRST report on one is the decision — `report {ask, kind: \"progress\", text: \"going ahead with: add GET /version, bump the version, build, push\"}` — which tells them what is happening and does not answer it. Report a milestone the same way. Your LAST is `done` or `blocked`, which answers it, in the shape above.",
        "Work asked of you arrives tagged `[ask <id> from <session>]`. Several may be waiting; work through them in whatever order makes sense and answer each one. When a turn answers a particular ask, START that answer with `[reply <id>]` so it reaches whoever asked it — without the tag, the oldest one waiting is taken as the one you answered.",
      ].join("\n"),
      true,
      "workspace",
    );
  }
  const server = new ToolServer(ws, resolveFromApi, process.env.KL_TREE || undefined);
  const reg = (name: string, label: string, description: string, parameters: ReturnType<typeof Type.Object>) =>
    pi.registerTool({
      name,
      label,
      description: `${description} Runs in workspace ${ws}.`,
      parameters,
      async execute(toolCallId, params, signal, _update, ctx) {
        const p = params as Record<string, any>;
        // The same policy-bearing dispatch a platform tool uses: argument/scope checks first, then
        // the person's asynchronous answer, and only then the tool server. A refusal never runs.
        const outcome = await dispatchWithPolicy({
          capability: name,
          effect: mutates(name, p) ? "write" : "read",
          approval: mutates(name, p)
            ? { required: "user", obtain: () => propose(`p-${toolCallId}`, name, p, ctx, signal, askLine(ws, name, p), askPreview(name, p)) }
            : { required: "none" },
          inspect: () => {
            // An action and its required fields are one argument: `start` without a command and
            // `logs`/`stop` without an id are refused here, before the card and before the server.
            if (name === "process") {
              const issues = processActionIssues(p as Record<string, JsonValue>);
              if (issues.length) return { code: "invalid_args", reason: issues.map((issue) => issue.message).join("; ") };
            }
            // Before the call, not after: a refused read must never reach the tool server at all.
            for (const r of reaches(name, p)) {
              const no = forbidden(r);
              if (no) return { code: "scope_denied", reason: no };
            }
            return undefined;
          },
          run: async () => {
            // A watch is the harness's own: nothing to run in the workspace, only a standing request.
            if (name === "process" && p.action === "watch") {
              const w = await fetch(`${process.env.KL_BENCH_URL ?? "http://127.0.0.1:7789"}/procs/${encodeURIComponent(String(p.id))}/watch`, {
                method: "POST",
                headers: { "content-type": "application/json" },
                body: JSON.stringify({ pattern: p.pattern ?? ".", from: process.env.KL_SESSION }),
              });
              return w.ok ? text(`watching ${p.id} for /${p.pattern ?? "."}/; matching lines arrive as messages`) : text(`the bench would not watch ${p.id}`, true);
            }
            const c = toIde(name, p);
            const r = await server.call(c, signal);
            // The title belongs to the id the tool server just minted.
            if (c.args.detach && r.body?.id) titles.set(String(r.body.id), procTitle(String(p.command ?? ""), p.title));
            // A CLI verb that a tool covers: the work is done, and the model is told where the tool is.
            const note = name === "bash" || (name === "process" && p.action === "start") ? shellNote(String(p.command ?? "")) : undefined;
            // The harness's own table of what is running: mirrored from the tool server after every
            // call that could have changed it, because nothing else tells the bench a process exists.
            if (PROCESS_TOOLS.has(c.tool) || c.args.detach) await publishProcs(server, ctx, signal);
            const out = fromIde(name, r.status, r.body, p.limit);
            // Only the model sees this. A person watching reads the ROW, not the 400 lines behind it,
            // so an answer that says "as you can see above" is an answer to nobody (§17.5).
            const long = out.content.map((c) => c.text).join("\n").split("\n").length > LONG_OUTPUT_LINES;
            const trailers = [note, long ? "only you see this output; relay what the person needs" : undefined].filter(Boolean) as string[];
            return trailers.length ? { ...out, content: [...out.content, ...trailers.map((t) => ({ type: "text" as const, text: t }))] } : out;
          },
          error: (e) => text((e as Error).message, true),
        });
        // pi's `AgentToolResult` carries a required `details` slot; these tools have no structured
        // details to add, and a JSON envelope drops an undefined one, so nothing else changes.
        return { ...toolResultOf(outcome), details: undefined };
      },
    });
  reg("read", "Read", "Read a text file with line numbers. offset (1-based line) and limit page it.", Type.Object({ path: Type.String(), offset: Type.Optional(Type.Number()), limit: Type.Optional(Type.Number()) }));
  reg("write", "Write", "Create or overwrite a file; parent directories are created.", Type.Object({ path: Type.String(), content: Type.String() }));
  reg("edit", "Edit", "Exact replacements in one file, all or nothing. Each oldText must occur exactly once.", Type.Object({ path: Type.String(), edits: Type.Array(Type.Object({ oldText: Type.String(), newText: Type.String() })) }));
  reg(
    "bash",
    "Bash",
    "Run a shell command in the workspace dir and return its output. background: true starts it and answers a process id instead, for a dev server or a watcher.",
    Type.Object({ command: Type.String(), timeout: Type.Optional(Type.Number({ description: "Seconds, at most 600" })), background: Type.Optional(Type.Boolean()) }),
  );
  reg(
    "process",
    "Process",
    "Long-running commands: start one (command), list them, read its logs since an offset, write to its stdin, stop it, or WATCH it for a pattern. They outlive a turn; you are told when one ends and when a watched line appears, so never poll.",
    Type.Object({
      action: StringEnum(["start", "list", "logs", "stop", "write", "watch"]),
      command: Type.Optional(Type.String({ description: "action=start" })),
      id: Type.Optional(Type.String({ description: "the process, for logs/stop/write" })),
      title: Type.Optional(Type.String({ description: "short name people will see, e.g. \"svelte dev server\"" })),
      since: Type.Optional(Type.Number({ description: "action=logs: where to read from — 0 for the start, or the `next` a previous read answered with, which is how you get only what is new" })),
      data: Type.Optional(Type.String({ description: "action=write" })),
      signal: Type.Optional(StringEnum(["TERM", "KILL"])),
      pattern: Type.Optional(Type.String({ description: "action=watch: a regular expression; matching lines are sent to you as they appear" })),
    }),
  );
  reg("grep", "Grep", "Regex search, gitignore-aware. path is a directory.", Type.Object({ pattern: Type.String(), path: Type.Optional(Type.String()), glob: Type.Optional(Type.String()), ignoreCase: Type.Optional(Type.Boolean()), literal: Type.Optional(Type.Boolean()), context: Type.Optional(Type.Number()), limit: Type.Optional(Type.Number()) }));
  reg("find", "Find", "Files matching a glob, gitignore-aware, newest first.", Type.Object({ pattern: Type.String(), path: Type.Optional(Type.String()), limit: Type.Optional(Type.Number()) }));
  reg(
    "kl_repo_clone",
    "Clone",
    "Clone a repository (owner/name) into this machine over ssh, with the person's own key. dir is where it lands.",
    Type.Object({ repo: Type.String({ description: "owner/name" }), dir: Type.Optional(Type.String()) }),
  );
  reg(
    "kl_container_build",
    "Build",
    "Build an image from a context in this machine and push it, on the owner's builder. It runs in the background: the answer is a process id, and `process logs` shows how it is going.",
    Type.Object({ context: Type.String({ description: "the build context directory" }), tag: Type.String({ description: "name:tag; it is pushed under your own owner" }), dockerfile: Type.Optional(Type.String()) }),
  );
  reg("kl_container_push", "Push image", "Copy an image the registry already holds to another tag.", Type.Object({ from: Type.String(), to: Type.String() }));
  reg("kl_images", "Images", "Images in the registry, by owner.", Type.Object({ owner: Type.Optional(Type.String()) }));
  reg("ls", "List", "List a directory; directories end in /.", Type.Object({ path: Type.Optional(Type.String()), limit: Type.Optional(Type.Number()) }));
}
