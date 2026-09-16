import http from "node:http";
import { WebSocketServer, type WebSocket } from "ws";
import type { Bench } from "./bench.ts";
import { Idle } from "./idle.ts";
import { holdFrames, spliceWorkspaceShell } from "./pty.ts";

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
  if ((p[0] === "sessions" || p[0] === "workspaces") && p[1] !== undefined && (!/^[A-Za-z0-9._-]+$/.test(p[1]) || p[1] === "." || p[1] === ".."))
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
      if (m === "GET" && u.pathname === "/healthz") return send(res, 200, { ok: true, readOnly: bench.readOnly, writable: bench.writable.ok(), reason: bench.writable.reason(), ...idle.state() });
      if (p[0] === "sessions") {
        if (p.length === 1 && m === "GET") return send(res, 200, bench.sessions.all());
        if (p.length === 1 && m === "POST") return send(res, 201, await bench.create());
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
        if (p.length === 3 && m === "GET" && p[2] === "messages") return send(res, 200, await bench.messages(p[1], n("after"), n("limit")));
        if (p.length === 3 && m === "POST" && p[2] === "btw") return send(res, 200, await bench.btw(p[1], String((await body(req)).question ?? "")));
        if (p.length === 3 && m === "GET" && p[2] === "btw") return send(res, 200, bench.listBtw(p[1]));
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
        if (p.length === 3 && p[2] === "messages" && m === "GET") return send(res, 200, await thread(`w-${p[1]}`));
        if (p.length === 5 && p[2] === "eph" && p[4] === "session" && m === "POST") return send(res, 200, await bench.openEphemeral(p[1], p[3]));
        if (p.length === 5 && p[2] === "eph" && p[4] === "messages" && m === "GET") return send(res, 200, await thread(`e-${p[3]}`));
      }
      if (m === "GET" && u.pathname === "/tasks") return send(res, 200, bench.tasks.all());
      if (m === "GET" && u.pathname === "/procs") return send(res, 200, bench.procs.all());
      if (m === "POST" && u.pathname === "/import") {
        const b = await body(req);
        return send(res, 200, bench.import(b.items ?? [], b.loose ?? []));
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
    let scope: string | undefined;
    if (p.length === 1 && p[0] === "pty") {
      scope = u.searchParams.get("scope") ?? "";
      // A scope that is neither the bench nor a workspace id is refused before anything is opened or dialled.
      if (scope !== "bench" && !SCOPE_RE.test(scope)) return void socket.end("HTTP/1.1 400 Bad Request\r\nconnection: close\r\n\r\n");
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
            spliceWorkspaceShell(w, LOCAL_TOOLS, first);
            return held.release();
          }
          return resolveTools(scope!).then(
            (a) => {
              spliceWorkspaceShell(w, a, first);
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
