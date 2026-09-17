import http from "node:http";
import { WebSocketServer, type WebSocket } from "ws";
import type { Bench } from "./bench.ts";
import { Idle } from "./idle.ts";
import { listProviders, removeProvider, setProvider } from "./providers.ts";
import { holdFrames, SESSION_RE, spliceWorkspaceShell, toolSessions } from "./pty.ts";

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

/** Split and decode a path; a bad escape or an id that could walk out of a folder (`..%2F`) is a 400, never a crash or a read elsewhere. */
function segments(pathname: string): string[] {
  const p = pathname.split("/").filter(Boolean).map(decodeURIComponent);
  if ((p[0] === "sessions" || p[0] === "workspaces" || p[0] === "proposals" || p[0] === "agents" || p[0] === "procs" || p[0] === "memory") && p[1] !== undefined && (!/^[A-Za-z0-9._-]+$/.test(p[1]) || p[1] === "." || p[1] === ".."))
    throw new Error(`bad id ${JSON.stringify(p[1])}`);
  return p;
}

const SCOPE_RE = /^ws-[0-9a-f]{16}$/;

/** The tool server in this pod's workspace container; same pod, so no NetworkPolicy and no token. */
const LOCAL_TOOLS = "127.0.0.1:7788";

export function serve(
  bench: Bench,
  port: number,
  host = "127.0.0.1",
  idle = new Idle(() => bench.busy()),
  maxBody = MAX_BODY,
  // Tests pass a fake; the real one is loaded lazily so the bench's own routes never pull pi's SDK in.
  opts: { resolveTools?: (ws: string) => Promise<string> } = {},
): Promise<{ port: number; close(): Promise<void> }> {
  const resolveTools = opts.resolveTools ?? ((ws: string) => import("../../pi/workspace-tools.ts").then((m) => m.resolveFromApi(ws)));
  const body = async (req: http.IncomingMessage): Promise<Record<string, unknown>> => {
    let s = "";
    let size = 0;
    for await (const c of req) {
      size += (c as Buffer).length;
      if (size > maxBody) throw new Error(TOO_LARGE);
      s += c;
    }
    return s ? (JSON.parse(s) as Record<string, unknown>) : {};
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
      // `model` is the bench's DEFAULT model. A window that opens before any session row has loaded
      // still has to name what will answer; without it the composer said "no model" (owner, 2026-09-17).
      if (m === "GET" && u.pathname === "/healthz") return send(res, 200, { ok: true, model: bench.model, readOnly: bench.readOnly, writable: bench.writable.ok(), reason: bench.writable.reason(), ...idle.state() });
      if (p[0] === "sessions") {
        if (p.length === 1 && m === "GET") return send(res, 200, bench.sessions.all());
        if (p.length === 1 && m === "POST") return send(res, 201, await bench.create(await body(req).catch(() => ({}))));
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
        if (p.length === 3 && m === "POST" && p[2] === "model") return send(res, 200, await bench.setModel(p[1], await body(req)));
      }
      if (m === "GET" && u.pathname === "/exchanges") {
        const s = u.searchParams.get("session"), w = u.searchParams.get("workspace");
        if (!s === !w) return send(res, 400, { error: "exactly one of session= or workspace=" });
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
          return send(res, 200, { answer: await bench.waitProposal(p[1], cap, ac.signal) });
        }
        if (p.length === 2 && m === "POST") {
          const b = await body(req);
          // yes / no for a proposal; a question's answer is the person's own words.
          if (typeof b.answer !== "string" || !b.answer.trim()) return send(res, 400, { error: "answer is yes, no, or what the person chose" });
          return send(res, 200, bench.answerProposal(p[1], b.answer));
        }
      }
      if (m === "GET" && u.pathname === "/proposals") return send(res, 200, bench.openProposals());
      // An agent: a fresh ephemeral session in a workspace, given one task.
      if (p[0] === "agents") {
        if (p.length === 1 && m === "POST") {
          const b = await body(req);
          if (typeof b.from !== "string" || !bench.sessions.get(b.from)) return send(res, 400, { error: `not a live session: ${JSON.stringify(b.from ?? null)}` });
          return send(res, 202, await bench.agent(String(b.workspace ?? ""), String(b.task ?? ""), String(b.name ?? ""), b.from, b.clone ? String(b.clone) : undefined, b.model ? String(b.model) : undefined));
        }
        // Closing one is removing its session: an agent's transcript is its own and goes with it.
        // Its clone, if it had one, is scratch — the caller keeps whatever it wanted from it.
        if (p.length === 2 && m === "DELETE") {
          const clone = bench.cloneOf(p[1]);
          // Let it stop its own turn first; then its session and its scratch go.
          await bench.abortAgent(p[1]).catch(() => undefined);
          await bench.remove(`e-${p[1]}`, true).catch(() => undefined);
          bench.forgetClone(p[1]);
          return send(res, 200, { closed: p[1], clone });
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
      if (p[0] === "procs" && p.length === 3 && p[2] === "output" && m === "GET") return send(res, 200, await bench.procOutput(p[1], Number(u.searchParams.get("since")) || 0));
      // Tell this session about lines of a running process that match a pattern.
      if (p[0] === "procs" && p.length === 3 && p[2] === "watch" && m === "POST") {
        const b = await body(req);
        if (typeof b.from !== "string" || !bench.sessions.get(b.from)) return send(res, 400, { error: `not a live session: ${JSON.stringify(b.from ?? null)}` });
        bench.watchProc(b.from, p[1], String(b.pattern ?? ""));
        return send(res, 202, { watching: p[1], pattern: b.pattern });
      }
      if (m === "POST" && u.pathname === "/import") {
        const b = await body(req);
        return send(res, 200, bench.import(b.items ?? [], b.loose ?? []));
      }
      // The two session routes are plain proxies of the tool server's own, scope-resolved like /pty.
      if (p[0] === "pty" && p[1] === "sessions" && (p.length === 2 || p.length === 3)) {
        const scope = u.searchParams.get("scope") ?? "";
        if (scope !== "bench" && !SCOPE_RE.test(scope)) return send(res, 400, { error: `bad scope ${JSON.stringify(scope)}` });
        const name = p[2];
        if (name !== undefined && !SESSION_RE.test(name)) return send(res, 400, { error: `bad session ${JSON.stringify(name)}` });
        if ((p.length === 2 && m === "GET") || (p.length === 3 && m === "DELETE")) {
          const addr = scope === "bench" ? LOCAL_TOOLS : await resolveTools(scope);
          const r = await toolSessions(addr, m as "GET" | "DELETE", name);
          return send(res, r.code, r.body);
        }
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
        const addr = scope === "bench" ? LOCAL_TOOLS : await resolveTools(scope);
        const rest = p.slice(1).join("/");
        const q = new URLSearchParams([...u.searchParams].filter(([k]) => k !== "scope")).toString();
        const r = await fetch(`http://${addr}/fs/${rest}${q ? `?${q}` : ""}`).catch((e: Error) => ({ ok: false, status: 502, text: async () => e.message }) as unknown as Response);
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

  const wss = new WebSocketServer({ noServer: true, maxPayload: maxBody });
  const events = new Set<WebSocket>();
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
    let scope: string | undefined, session: string | undefined;
    if (p.length === 1 && p[0] === "pty") {
      scope = u.searchParams.get("scope") ?? "";
      // A scope that is neither the bench nor a workspace id is refused before anything is opened or dialled.
      if (scope !== "bench" && !SCOPE_RE.test(scope)) return void socket.end("HTTP/1.1 400 Bad Request\r\nconnection: close\r\n\r\n");
      session = u.searchParams.get("session") ?? undefined;
      // Refused at the handshake like the scope: a bad name must never reach the far end's tmux argv.
      if (session !== undefined && !SESSION_RE.test(session)) return void socket.end("HTTP/1.1 400 Bad Request\r\nconnection: close\r\n\r\n");
    }
    if (!rpc && scope === undefined && !(p.length === 1 && p[0] === "events")) return void socket.destroy();
    wss.handleUpgrade(req, socket, head, (w) => {
      // A connected device holds the bench up whichever socket it holds.
      idle.opened();
      w.on("close", () => idle.closed());
      // An oversized frame (maxPayload) or a torn socket errors before it closes; unheard, it would crash the bench.
      w.on("error", () => undefined);
      if (scope !== undefined) {
        // 80x24 is the fallback, never the shell a client that spoke gets.
        const held = holdFrames(w, 2_000);
        void held.first.then((first) => {
          // The bench runs in the workspace pod: its own shell is that pod's tool server, one hop over loopback.
          if (scope === "bench") {
            spliceWorkspaceShell(w, LOCAL_TOOLS, first, session);
            return held.release();
          }
          return resolveTools(scope!).then(
            (a) => {
              spliceWorkspaceShell(w, a, first, session);
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
        w.on("close", () => events.delete(w));
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
        close: () =>
          new Promise<void>((r) => {
            unsubscribe();
            for (const w of wss.clients) w.terminate();
            server.closeAllConnections();
            server.close(() => r());
          }),
      });
    });
  });
}
