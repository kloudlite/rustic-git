import http from "node:http";
import { WebSocketServer, type WebSocket } from "ws";
import type { Bench } from "./bench.ts";
import { Idle } from "./idle.ts";
import { listProviders, removeProvider, setProvider } from "./providers.ts";
import { holdFrames, spliceShell } from "./pty.ts";
import { spliceWatch } from "./watch.ts";
import { handleOperationControl, operationControlError, type OperationControlOptions } from "./operations/control.ts";

/**
 * harness-bench's surface. Where it listens is main's choice: the pod IP
 * behind the platform's gateway-only NetworkPolicy, or loopback on a laptop.
 * Each session's RPC is pi's own JSONL framing, one message per frame; ids are
 * the client's and are rewritten only for the trip through pi (RpcChild mints
 * its own), so two devices can both send id "1".
 */
const status = (e: Error) => (/no session/.test(e.message) ? 404 : /read-only|not writable|in flight|only open session|belongs to/.test(e.message) ? 409 : 400);

const TOO_LARGE = "request body too large";
const MAX_BODY = 64 * 1024 * 1024;

/**
 * The operation envelope is for `/operations/*` only: a `not_found`/`forbidden`/`stale_revision`
 * thrown by an ordinary route (a session, a workspace) must still answer this server's own
 * `{error: "<message>"}`, not the operation control's `{error:{code,message}}` — never throws,
 * so a malformed URL (a bad `%` escape) falls through to the ordinary catch instead of crashing it.
 */
function isOperationRoute(req: http.IncomingMessage): boolean {
  try {
    const u = new URL(req.url ?? "/", "http://bench");
    return u.pathname.split("/").filter(Boolean)[0] === "operations";
  } catch {
    return false;
  }
}

/** Split and decode a path; a bad escape or an id that could walk out of a folder (`..%2F`) is a 400, never a crash or a read elsewhere. */
function segments(pathname: string): string[] {
  const p = pathname.split("/").filter(Boolean).map(decodeURIComponent);
  if ((p[0] === "sessions" || p[0] === "workspaces" || p[0] === "proposals" || p[0] === "agents" || p[0] === "procs" || p[0] === "memory") && p[1] !== undefined && (!/^[A-Za-z0-9._-]+$/.test(p[1]) || p[1] === "." || p[1] === ".."))
    throw new Error(`bad id ${JSON.stringify(p[1])}`);
  return p;
}

/** A body that is not a JSON object: refused, never read as an empty one (D10). */
class BadBody extends Error {
  constructor() {
    super("the body is not JSON");
    this.name = "BadBody";
  }
}

const SCOPE_RE = /^ws-[0-9a-f]{16}$/;

/** The tool server in this pod's workspace container; same pod, so no NetworkPolicy and no token. */
const LOCAL_TOOLS = "127.0.0.1:7788";
/**
 * This pod's own address, for its own shell sidecar. The sessions container cannot fork a shell —
 * that is the whole point of §3 — so the bench's terminal is the `shell` container beside it, and
 * a sidecar is reached on the POD IP, not on loopback's tool-server port. `KL_POD_IP` comes from
 * the downward API (`status.podIP`); with none set, loopback is the honest fallback for a bench
 * running on somebody's laptop.
 */
const ownPod = (): string => `${process.env.KL_POD_IP ?? "127.0.0.1"}:7788`;

export function serve(
  bench: Bench,
  port: number,
  host = "127.0.0.1",
  idle = new Idle(() => bench.busy()),
  maxBody = MAX_BODY,
  // Tests pass a fake; the real one is loaded lazily so the bench's own routes never pull pi's SDK in.
  opts: { resolveTools?: (ws: string) => Promise<string> } & OperationControlOptions = {},
): Promise<{ port: number; close(): Promise<void>; server: http.Server; sweepOnce(): void }> {
  const resolveTools = opts.resolveTools ?? ((ws: string) => import("../../pi/workspace-tools.ts").then((m) => m.resolveFromApi(ws)));
  /**
   * Where a scope's tool server is AND the token every `/tools/*`, `/fs/*` and `/stream/*` call
   * must carry. The bench's own container (`bench`) is loopback in the same pod and takes none.
   * The token is a header and nothing else: never a log line, never an answer to the renderer.
   */
  const toolsFor = async (scope: string): Promise<{ address: string; token?: string }> => {
    if (scope === "bench") return { address: LOCAL_TOOLS };
    const mod = await import("../../pi/workspace-tools.ts");
    if (opts.resolveTools) {
      const address = await opts.resolveTools(scope);
      const token = await mod.toolsAuth(scope).then((a) => a.token, () => undefined);
      return { address, ...(token ? { token } : {}) };
    }
    return mod.toolsAuth(scope);
  };
  const bearer = (t?: string): Record<string, string> => (t ? { authorization: `Bearer ${t}` } : {});
  const body = async (req: http.IncomingMessage): Promise<Record<string, unknown>> => {
    let s = "";
    let size = 0;
    for await (const c of req) {
      size += (c as Buffer).length;
      if (size > maxBody) throw new Error(TOO_LARGE);
      s += c;
    }
    if (!s) return {};
    try {
      const v = JSON.parse(s) as unknown;
      // A JSON scalar is not a body either: `"x"` would read as an object with no fields.
      if (typeof v !== "object" || v === null || Array.isArray(v)) throw new Error("not an object");
      return v as Record<string, unknown>;
    } catch {
      throw new BadBody();
    }
  };
  const send = (res: http.ServerResponse, code: number, v?: unknown) => {
    res.writeHead(code, v === undefined ? {} : { "content-type": "application/json" });
    res.end(v === undefined ? undefined : JSON.stringify(v));
  };
  const server = http.createServer(async (req, res) => {
    const u = new URL(req.url ?? "/", "http://bench");
    // after= is a message (or exchange) index, never a timestamp.
    const n = (k: string) => (u.searchParams.has(k) ? Number(u.searchParams.get(k)) : undefined);
    const m = req.method ?? "GET";
    try {
      const p = segments(u.pathname);
      if (await handleOperationControl(req, res, u, p, () => body(req), opts)) return;
      // `model` is the bench's DEFAULT model. A window that opens before any session row has loaded
      // still has to name what will answer; without it the composer said "no model" (owner, 2026-09-17).
      if (m === "GET" && u.pathname === "/healthz") return send(res, 200, { ok: true, model: bench.model, readOnly: bench.readOnly, writable: bench.writable.ok(), reason: bench.writable.reason(), ...idle.state() });
      if (p[0] === "sessions") {
        if (p.length === 1 && m === "GET") return send(res, 200, bench.sessions.all());
        // A broken body was read as an EMPTY one, so `{not json` created a real session (D10).
        if (p.length === 1 && m === "POST") return send(res, 201, await bench.create(await body(req)));
        if (p.length === 2 && m === "DELETE") {
          const b = await body(req);
          try {
            await bench.remove(p[1], b.stop === true);
            return send(res, 204);
          } catch (e) {
            const msg = (e as Error).message;
            if (msg.startsWith("in flight: ")) return send(res, 409, { error: msg, items: msg.slice(11).split(", ") });
            throw e;
          }
        }
        if (p.length === 3 && m === "POST" && p[2] === "archive") return send(res, 200, await bench.archive(p[1]));
        if (p.length === 3 && m === "POST" && p[2] === "restore") return send(res, 200, await bench.restore(p[1]));
        // What this session can call AND where those calls run. Read by the fleet probe that holds
        // a bench session to its own workspace's hands (`bench.tools.own_hands`); pi's own RPC has
        // no tool listing.
        if (p.length === 3 && m === "GET" && p[2] === "tools") return send(res, 200, bench.tools(p[1]));
        if (p.length === 3 && m === "GET" && p[2] === "messages") return send(res, 200, await bench.messages(p[1], n("after"), n("limit"), n("tail")));
        if (p.length === 3 && m === "POST" && p[2] === "btw") return send(res, 200, await bench.btw(p[1], String((await body(req)).question ?? "")));
        if (p.length === 3 && m === "GET" && p[2] === "btw") return send(res, 200, bench.listBtw(p[1]));
        // The person's pick for one session. `default: false` keeps the general default where it is
        // — a dispatch naming a model is not a person changing their mind (spec §1.2).
        if (p.length === 3 && m === "POST" && p[2] === "model") {
          try {
            return send(res, 200, await bench.setModel(p[1], await body(req)));
          } catch (e) {
            // A model the provider does not carry: named, with what it does carry, so the picker
            // can say so rather than leaving a session that answers nothing (D1).
            if ((e as Error).name === "NoSuchModel") return send(res, 409, { error: (e as Error).message, known: (e as { known?: string[] }).known ?? [] });
            throw e;
          }
        }
        /**
         * What `tool_search` has turned on for this session, kept where it outlives the pi child:
         * a found tool stays found for the rest of the session, across a bench restart. GET arms a
         * starting session; POST records what a search just found and answers the whole set.
         */
        if (p.length === 3 && p[2] === "found" && (m === "GET" || m === "POST")) {
          if (!bench.sessions.get(p[1])) return send(res, 404, { error: `no session ${p[1]}` });
          if (m === "GET") return send(res, 200, { found: bench.sessions.found(p[1]) });
          const b = await body(req);
          const names = Array.isArray(b.names) ? (b.names as unknown[]).map(String) : [];
          return send(res, 200, { found: bench.sessions.remember(p[1], names) });
        }
      }
      if (m === "GET" && u.pathname === "/exchanges") {
        const s = u.searchParams.get("session"), w = u.searchParams.get("workspace");
        if (!s === !w) return send(res, 400, { error: "exactly one of session= or workspace=" });
        // `200 []` for a session that does not exist reads as "nothing asked yet" (D11).
        if (s && !bench.sessions.get(s)) return send(res, 404, { error: `no session ${s}` });
        return send(res, 200, s ? bench.exchanges.bySession(s, n("after")) : bench.exchanges.byWorkspace(w!, n("after")));
      }
      if (p[0] === "workspaces") {
        // A thread never opened has no history yet, which is an empty one, not a missing route.
        const thread = (id: string) => (bench.sessions.get(id) ? bench.messages(id, n("after"), n("limit")) : Promise.resolve({ messages: [], total: 0 }));
        if (p.length === 3 && p[2] === "session" && m === "POST") return send(res, 200, await bench.openWorkspace(p[1]));
        // The bench's own extension asking a workspace to do something. Loopback only, like every
        // other route here; `from` is the asking session, handed to its child at spawn as KL_SESSION.
        if (p.length === 3 && p[2] === "ask" && m === "POST") {
          const b = await body(req);
          if (typeof b.from !== "string" || !bench.sessions.get(b.from)) return send(res, 400, { error: `not a live session: ${JSON.stringify(b.from ?? null)}` });
          // A question is answered beside the work, never in front of it.
          if (b.kind === "info") return send(res, 202, await bench.infoAsk(p[1], String(b.text ?? ""), b.from));
          return send(res, 202, await bench.ask(p[1], String(b.text ?? ""), b.from));
        }
        if (p.length === 3 && p[2] === "messages" && m === "GET") return send(res, 200, await thread(`w-${p[1]}`));
        if (p.length === 5 && p[2] === "eph" && p[4] === "session" && m === "POST") return send(res, 200, await bench.openEphemeral(p[1], p[3]));
        if (p.length === 5 && p[2] === "eph" && p[4] === "messages" && m === "GET") return send(res, 200, await thread(`e-${p[3]}`));
      }
      if (p[0] === "providers") {
        if (p.length === 1 && m === "GET") return send(res, 200, listProviders());
        // The key travels in the body and goes nowhere else: never a path segment, never logged, never read back.
        if (p.length === 2 && m === "PUT") {
          setProvider(p[1], (await body(req)).apiKey);
          return send(res, 204);
        }
        if (p.length === 2 && m === "DELETE") {
          removeProvider(p[1]);
          return send(res, 204);
        }
      }
      // The proposal a tool is waiting on: the extension long-polls the wait, the desktop answers.
      if (p[0] === "proposals" && p.length >= 2) {
        const cap = Math.min(Number(u.searchParams.get("cap")) || 600_000, 600_000);
        if (p.length === 3 && p[2] === "wait" && m === "GET") {
          const ac = new AbortController();
          req.on("close", () => ac.abort());
          // `session` says WHICH card: the same tool-call id can be open in two sessions, and the
          // bench will not guess between them (R-D27).
          const who = u.searchParams.get("session") ?? undefined;
          return send(res, 200, { answer: await bench.waitProposal(p[1], cap, ac.signal, who) });
        }
        if (p.length === 2 && m === "POST") {
          const b = await body(req);
          // yes / no for a proposal; a question's answer is the person's own words.
          if (typeof b.answer !== "string" || !b.answer.trim()) return send(res, 400, { error: "answer is yes, no, or what the person chose" });
          // A card answered after its tool stopped waiting: 409, and the desktop says so in the
          // composer footer. Never a prompt — a late answer is not something to tell the model.
          try {
            return send(res, 200, bench.answerProposal(p[1], b.answer));
          } catch (e) {
            if ((e as Error).name === "AlreadyAnswered") return send(res, 409, { error: (e as Error).message, answer: (e as { answer?: string }).answer });
            const msg = (e as Error).message;
            if (msg.startsWith("no proposal ")) return send(res, 409, { error: "that question is no longer waiting for an answer" });
            throw e;
          }
        }
      }
      if (m === "GET" && u.pathname === "/proposals") return send(res, 200, bench.openProposals());
      // An agent: a fresh ephemeral session in a workspace, given one task.
      if (p[0] === "agents") {
        if (p.length === 1 && m === "POST") {
          const b = await body(req);
          if (typeof b.from !== "string" || !bench.sessions.get(b.from)) return send(res, 400, { error: `not a live session: ${JSON.stringify(b.from ?? null)}` });
          try {
            return send(res, 202, await bench.agent(String(b.workspace ?? ""), String(b.task ?? ""), String(b.name ?? ""), b.from, b.model ? String(b.model) : undefined));
          } catch (e) {
            // A tree that could not be cut is the whole dispatch: there is no session to report
            // into, so the caller's tool answers the sentence rather than a half-started agent.
            return send(res, 409, { error: (e as Error).message });
          }
        }
        // Closing one is removing its session: an agent's transcript is its own and goes with it,
        // and so does its TREE — through `/v1`, by the bench, because nothing else holds the name.
        if (p.length === 2 && m === "DELETE") {
          const where = bench.treeOf(p[1]);
          // Let it stop its own turn first; then its session and its working directory go.
          await bench.abortAgent(p[1]).catch(() => undefined);
          await bench.remove(`e-${p[1]}`, true).catch(() => undefined);
          await bench.dropTree(p[1]).catch(() => undefined);
          return send(res, 200, { closed: p[1], ...(where ?? {}) });
        }
      }
      // The person's memory. A workspace session has no bench filesystem, so it saves through here
      // and the bench writes the file — one door, whoever is asking.
      if (p[0] === "memory") {
        if (p.length === 1 && m === "GET") return send(res, 200, bench.memories.all());
        if (p.length === 1 && m === "POST") {
          const b = await body(req);
          return send(res, 200, bench.memories.save({ name: String(b.name ?? ""), description: String(b.description ?? ""), type: b.type as never, body: String(b.body ?? "") }));
        }
        if (p.length === 2 && m === "GET") return send(res, 200, { name: p[1], text: bench.memories.read(p[1]) });
        if (p.length === 2 && m === "DELETE") return send(res, 200, bench.memories.forget(p[1]));
      }
      /**
       * Everything a window needs to open, in ONE request. Each request over the tunnel opens its
       * own gateway WebSocket — a TLS handshake at the edge — so six calls to paint a thread cost
       * six handshakes and the bench's own answers were never the slow part (measured).
       */
      if (m === "GET" && u.pathname === "/bootstrap") {
        const session = u.searchParams.get("session") ?? undefined;
        return send(res, 200, {
          model: bench.model,
          sessions: bench.sessions.all(),
          plans: bench.plans.all(),
          procs: bench.procs.all(),
          tasks: bench.tasks.all(),
          exchanges: bench.exchanges.recent(200),
          proposals: bench.openProposals(),
          memory: bench.memories.all(),
          messages: session ? await bench.messages(session, undefined, undefined, n("tail") ?? 60) : undefined,
          session,
        });
      }
      /**
       * The architecture document (§24): what runs where, and what talks to what. Read by every
       * session and by the desktop's own page; written by the `architecture` tool and by the
       * `contracts:` line of a work reply.
       */
      if (u.pathname === "/architecture") {
        if (m === "GET") return send(res, 200, { text: bench.architecture.read(), contracts: bench.architecture.contracts() });
        if (m === "PUT") {
          const b = await body(req);
          if (typeof b.section === "string" && typeof b.text === "string")
            return send(res, 200, { text: bench.architecture.setSection(b.section, b.text) });
          if (typeof b.text === "string") return send(res, 200, { text: bench.architecture.write(b.text) });
          return send(res, 400, { error: "say `text`, or `section` and `text`" });
        }
      }
      // The picker's catalogue, and the general default it opens on.
      if (m === "GET" && u.pathname === "/models") return send(res, 200, await bench.models());
      if (m === "GET" && u.pathname === "/defaults") return send(res, 200, bench.defaults.get());
      if (m === "GET" && u.pathname === "/plans") return send(res, 200, bench.plans.all());
      if (m === "GET" && u.pathname === "/tasks") return send(res, 200, bench.tasks.all());
      if (m === "GET" && u.pathname === "/procs") return send(res, 200, bench.procs.all());
      // A process's log, followed from a byte offset: the desktop's detail view reads this while it runs.
      if (p[0] === "procs" && p.length === 3 && p[2] === "output" && m === "GET")
        // Both cursors: stdout's and stderr's. A reader that sends only `since` re-reads stderr from
        // the start on every poll, which is what made a followed log look like it began again.
        return send(res, 200, await bench.procOutput(p[1], Number(u.searchParams.get("since")) || 0, Number(u.searchParams.get("sinceErr")) || 0));
      // A person stopping a process or cancelling a task, straight from the desktop. Both used to
      // travel as `/proc-stop`/`/cancel` PROMPTS, which put them in pi's context and its session
      // file; as ordinary HTTP the model never sees them at all.
      if (p[0] === "procs" && p.length === 3 && p[2] === "stop" && m === "POST") {
        const b = await body(req);
        if (typeof b.session !== "string" || !bench.sessions.get(b.session)) return send(res, 400, { error: `not a live session: ${JSON.stringify(b.session ?? null)}` });
        await bench.killProc(b.session, p[1]);
        return send(res, 200, { stopped: p[1] });
      }
      if (p[0] === "tasks" && p.length === 3 && p[2] === "cancel" && m === "POST") return send(res, 200, bench.cancelTask(p[1]));
      // Tell this session about lines of a running process that match a pattern.
      if (p[0] === "procs" && p.length === 3 && p[2] === "watch" && m === "POST") {
        const b = await body(req);
        if (typeof b.from !== "string" || !bench.sessions.get(b.from)) return send(res, 400, { error: `not a live session: ${JSON.stringify(b.from ?? null)}` });
        bench.watchProc(b.from, p[1], String(b.pattern ?? ""));
        return send(res, 202, { watching: p[1], pattern: b.pattern });
      }
      /**
       * A workspace session reporting on an ask it holds (spec §3.8): `progress` is its decision and
       * settles nothing, `done`/`blocked` settle it. Loopback only, like every other extension route.
       */
      if (m === "POST" && u.pathname === "/reports") {
        const b = await body(req);
        if (typeof b.from !== "string" || !bench.sessions.get(b.from)) return send(res, 400, { error: `not a live session: ${JSON.stringify(b.from ?? null)}` });
        const kind = b.kind === "done" || b.kind === "blocked" ? b.kind : "progress";
        try {
          return send(res, 202, await bench.report(b.from, String(b.ask ?? ""), kind, String(b.text ?? "")));
        } catch (e) {
          return send(res, 400, { error: (e as Error).message });
        }
      }
      if (m === "POST" && u.pathname === "/import") {
        const b = await body(req);
        return send(res, 200, bench.import(b.items ?? [], b.loose ?? []));
      }
      /**
       * `GET /fs/against-main?scope=&tree=` — the ONE exec the desktop may ask for: what a
       * subagent's tree has done that the workspace's main branch has not (spec §4.5). It is a
       * fixed argv, never a command from the client: a general exec proxy here would be a shell
       * for anything with the bench's port, which is the boundary §3.1 exists to hold.
       */
      if (p[0] === "fs" && m === "GET" && p[1] === "against-main" && p.length === 2) {
        const scope = u.searchParams.get("scope") ?? "";
        const tree = u.searchParams.get("tree") ?? "";
        if (!SCOPE_RE.test(scope) || !/^[a-z0-9-]{1,32}$/.test(tree)) return send(res, 400, { error: "a workspace and one of its trees" });
        const at = await toolsFor(scope);
        const r = await fetch(`http://${at.address}/tools/exec`, {
          method: "POST",
          headers: { "content-type": "application/json", ...bearer(at.token) },
          body: JSON.stringify({ tree, cmd: ["git", "diff", "main...HEAD"], timeout_ms: 30_000 }),
        }).catch(() => undefined);
        if (!r) return send(res, 502, { error: "the workspace's tools did not answer" });
        const out = (await r.json().catch(() => ({}))) as { stdout?: string; stderr?: string; exit_code?: number; error?: string };
        if (!r.ok) return send(res, r.status, { error: out.error ?? "the tool server refused" });
        return send(res, 200, { diff: out.stdout ?? "", ...(out.exit_code ? { error: (out.stderr ?? "").trim().split("\n").slice(-1)[0] } : {}) });
      }
      /**
       * A workspace's own files, read-only, proxied from its tool server's `/fs/*`
       * (`crates/ide/src/fs/`): the console renders a workspace from these, and nothing here
       * interprets them. The desktop's Files tab showed nothing because nobody ever asked
       * (owner, 2026-09-17).
       */
      if (p[0] === "fs" && m === "GET" && p.length >= 2) {
        const scope = u.searchParams.get("scope") ?? "";
        if (scope !== "bench" && !SCOPE_RE.test(scope)) return send(res, 400, { error: `bad scope ${JSON.stringify(scope)}` });
        const at = await toolsFor(scope);
        const rest = p.slice(1).join("/");
        // `etag` is ours, not the tool server's: it becomes the conditional header, so a file the
        // desktop already holds costs a 304 and no bytes.
        const etag = u.searchParams.get("etag") ?? undefined;
        const q = new URLSearchParams([...u.searchParams].filter(([k]) => k !== "scope" && k !== "etag")).toString();
        const r = await fetch(`http://${at.address}/fs/${rest}${q ? `?${q}` : ""}`, {
          headers: { ...(etag ? { "if-none-match": etag } : {}), ...bearer(at.token) },
        }).catch((e: Error) => ({ ok: false, status: 502, headers: new Headers(), text: async () => e.message, arrayBuffer: async () => new ArrayBuffer(0) }) as unknown as Response);
        /**
         * `/fs/file` answers the FILE — bytes with a content type and an ETag — while every other
         * route answers JSON. One envelope carries either over the tunnel, so the desktop has the
         * text, what kind of file it is, and the tag to ask again with.
         */
        if (rest.split("?")[0] === "file") {
          const tag = r.headers?.get?.("etag") ?? undefined;
          if (r.status === 304) return send(res, 200, { notModified: true, etag: tag });
          const mime = r.headers?.get?.("content-type") ?? "application/octet-stream";
          const body = Buffer.from(await r.arrayBuffer());
          if (r.status >= 400) {
            let why: unknown = body.toString("utf8");
            try {
              why = JSON.parse(String(why));
            } catch {
              /* a tool server that answered text answers text */
            }
            return send(res, r.status, why as never);
          }
          // Text is sent as text; anything else is named and measured, never streamed into a view.
          const text = /^text\/|json|javascript|xml|^application\/(x-)?(sh|toml|yaml)/.test(mime) ? body.toString("utf8") : undefined;
          return send(res, 200, { etag: tag, mime, bytes: body.length, ...(text === undefined ? { binary: true } : { text }) });
        }
        const text = await r.text();
        let data: unknown = text;
        try {
          data = text ? JSON.parse(text) : null;
        } catch {
          /* a tool server that answered text answers text */
        }
        return send(res, r.status || 502, data as never);
      }
      send(res, 404, { error: `no route ${m} ${u.pathname}` });
    } catch (e) {
      if (isOperationRoute(req) && operationControlError(res, e)) return;
      const msg = (e as Error).message;
      if (msg === TOO_LARGE) {
        // The rest of the upload is never read; closing is the only way to stop it.
        res.setHeader("connection", "close");
        send(res, 413, { error: msg });
        return void res.on("finish", () => req.destroy());
      }
      send(res, status(e as Error), { error: msg });
    }
  });

  // The client pool keeps a connection warm for 15 s (`keepAliveMsecs`), but Node's server default
  // is 5 s — so the pooled socket was dropped every ~6 s and the next REST call paid a fresh token
  // mint and handshake. The server must outlive the client's idle, not the other way round.
  server.keepAliveTimeout = 65_000;
  server.headersTimeout = 70_000;

  const wss = new WebSocketServer({ noServer: true, maxPayload: maxBody });
  const events = new Set<WebSocket>();
  /** Whether each events socket answered the last sweep's ping. */
  const alive = new Map<WebSocket, boolean>();
  /** One heartbeat round; the timer below is just this on a schedule, and a test can step it. */
  const sweepOnce = () => {
    for (const w of events) {
      // It missed a whole round: terminate rather than keep writing into a socket nobody reads.
      if (alive.get(w) === false) {
        events.delete(w);
        alive.delete(w);
        w.terminate();
        continue;
      }
      alive.set(w, false);
      try {
        w.ping();
      } catch {
        /* the close handler cleans up */
      }
    }
  };
  const sweep = setInterval(sweepOnce, 30_000);
  sweep.unref?.();
  const rpcClients = new Map<string, Set<WebSocket>>();
  const unsubscribe = bench.onEvent((ev) => {
    idle.check();
    // A btw fork streams to nobody: its answer replays whole from POST/GET btw.
    if (ev.pi?.startsWith("btw-")) return;
    const frame = JSON.stringify(ev);
    for (const w of events) w.send(frame);
    // Responses go only to their sender, through the awaited rpc below.
    if (!ev.pi || ev.type === "response") return;
    for (const w of rpcClients.get(ev.pi) ?? []) w.send(frame);
  });

  server.on("upgrade", (req, socket, head) => {
    let p: string[], u: URL;
    try {
      u = new URL(req.url ?? "/", "http://bench");
      p = segments(u.pathname);
    } catch {
      return void socket.destroy();
    }
    const rpc = p.length === 3 && p[0] === "sessions" && p[2] === "rpc" ? p[1] : undefined;
    let scope: string | undefined;
    // The workspace's file-system watch, scope-resolved exactly like a shell.
    let watch: string | undefined;
    if (p.length === 1 && p[0] === "watch") {
      watch = u.searchParams.get("scope") ?? "";
      if (watch !== "bench" && !SCOPE_RE.test(watch)) return void socket.end("HTTP/1.1 400 Bad Request\r\nconnection: close\r\n\r\n");
    }
    if (p.length === 1 && p[0] === "pty") {
      scope = u.searchParams.get("scope") ?? "";
      // A scope that is neither the bench nor a workspace id is refused before anything is opened or dialled.
      if (scope !== "bench" && !SCOPE_RE.test(scope)) return void socket.end("HTTP/1.1 400 Bad Request\r\nconnection: close\r\n\r\n");
      // No session name: a terminal is a live socket to the pod's shell and nothing more. tmux
      // sessions and their reattach went with the tool server's PTY (spec §2.3).
    }
    if (!rpc && scope === undefined && watch === undefined && !(p.length === 1 && p[0] === "events")) return void socket.destroy();
    wss.handleUpgrade(req, socket, head, (w) => {
      // A connected device holds the bench up whichever socket it holds.
      idle.opened();
      w.on("close", () => idle.closed());
      // An oversized frame (maxPayload) or a torn socket errors before it closes; unheard, it would crash the bench.
      w.on("error", () => undefined);
      if (watch !== undefined) {
        void toolsFor(watch).then(
          (a) => spliceWatch(w, a.address, undefined, a.token),
          (e: Error) => {
            w.send(JSON.stringify({ error: e.message }));
            w.close();
          },
        );
        return;
      }
      if (scope !== undefined) {
        // 80x24 is the fallback, never the shell a client that spoke gets.
        const held = holdFrames(w, 2_000);
        void held.first.then((first) => {
          // The bench's own shell is the sidecar in ITS pod, reached by the pod IP the downward
          // API gives us: the sessions container has no shell of its own to fork (spec §2.2).
          if (scope === "bench") {
            spliceShell(w, ownPod(), first);
            return held.release();
          }
          return resolveTools(scope!).then(
            (a) => {
              // The tool server's address, with the port swapped: same pod, the shell beside it.
              spliceShell(w, a, first);
              held.release();
            },
            (e: Error) => {
              w.send(JSON.stringify({ error: e.message }));
              w.close();
            },
          );
        });
        return;
      }
      if (!rpc) {
        events.add(w);
        // The standard ws heartbeat. A socket the edge or a dead laptop left HALF-OPEN still looks
        // writable, so events were being sent into it forever; the sweep closes it instead.
        alive.set(w, true);
        w.on("pong", () => alive.set(w, true));
        w.on("close", () => (events.delete(w), alive.delete(w)));
        return;
      }
      if (!rpcClients.has(rpc)) rpcClients.set(rpc, new Set());
      rpcClients.get(rpc)!.add(w);
      w.on("close", () => rpcClients.get(rpc)?.delete(w));
      w.on("message", async (d) => {
        let cmd: Record<string, unknown>;
        try {
          cmd = JSON.parse(d.toString()) as Record<string, unknown>;
        } catch {
          return void w.send(JSON.stringify({ type: "response", success: false, error: "not JSON" }));
        }
        const { id: clientId, ...rest } = cmd;
        try {
          const r = await bench.rpc(rpc, rest);
          w.send(JSON.stringify({ ...r, id: clientId }));
        } catch (e) {
          w.send(JSON.stringify({ type: "response", id: clientId, command: cmd.type, success: false, error: (e as Error).message }));
        }
      });
    });
  });

  return new Promise((resolve) => {
    server.listen(port, host, () => {
      resolve({
        port: (server.address() as { port: number }).port,
        server,
        sweepOnce,
        close: () =>
          new Promise<void>((r) => {
            unsubscribe();
            clearInterval(sweep);
            for (const w of wss.clients) w.terminate();
            server.closeAllConnections();
            server.close(() => r());
          }),
      });
    });
  });
}
