import http from "node:http";
import { WebSocketServer, type WebSocket } from "ws";
import type { Bench } from "./bench.ts";
import { Idle } from "./idle.ts";

/**
 * harness-bench's surface. Where it listens is main's choice: the pod IP
 * behind the platform's gateway-only NetworkPolicy, or loopback on a laptop.
 * Each session's RPC is pi's own JSONL framing, one message per frame; ids are
 * the client's and are rewritten only for the trip through pi (RpcChild mints
 * its own), so two devices can both send id "1".
 */
const status = (e: Error) => (/no session/.test(e.message) ? 404 : /read-only|not writable|in flight|only open session/.test(e.message) ? 409 : 400);

async function body(req: http.IncomingMessage): Promise<Record<string, unknown>> {
  let s = "";
  for await (const c of req) s += c;
  return s ? (JSON.parse(s) as Record<string, unknown>) : {};
}

export function serve(bench: Bench, port: number, host = "127.0.0.1", idle = new Idle(() => bench.busy())): Promise<{ port: number; close(): Promise<void> }> {
  const send = (res: http.ServerResponse, code: number, v?: unknown) => {
    res.writeHead(code, v === undefined ? {} : { "content-type": "application/json" });
    res.end(v === undefined ? undefined : JSON.stringify(v));
  };
  const server = http.createServer(async (req, res) => {
    const u = new URL(req.url ?? "/", "http://bench");
    const p = u.pathname.split("/").filter(Boolean).map(decodeURIComponent);
    // after= is a message (or exchange) index, never a timestamp.
    const n = (k: string) => (u.searchParams.has(k) ? Number(u.searchParams.get(k)) : undefined);
    const m = req.method ?? "GET";
    try {
      if (m === "GET" && u.pathname === "/healthz") {
        const readOnly = (bench as unknown as { opts: { readOnly: boolean } }).opts.readOnly;
        return send(res, 200, { ok: true, readOnly, writable: bench.writable.ok(), reason: bench.writable.reason(), ...idle.state() });
      }
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
      if (m === "GET" && p[0] === "workspaces" && p.length === 3 && p[2] === "messages") return send(res, 200, bench.exchanges.byWorkspace(p[1], n("after")));
      if (m === "GET" && u.pathname === "/tasks") return send(res, 200, bench.tasks.all());
      if (m === "GET" && u.pathname === "/procs") return send(res, 200, bench.procs.all());
      if (m === "POST" && u.pathname === "/import") {
        const b = await body(req);
        return send(res, 200, bench.import((b.items ?? []) as never, (b.loose ?? []) as never));
      }
      send(res, 404, { error: `no route ${m} ${u.pathname}` });
    } catch (e) {
      send(res, status(e as Error), { error: (e as Error).message });
    }
  });

  const wss = new WebSocketServer({ noServer: true });
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
    const p = new URL(req.url ?? "/", "http://bench").pathname.split("/").filter(Boolean).map(decodeURIComponent);
    const rpc = p.length === 3 && p[0] === "sessions" && p[2] === "rpc" ? p[1] : undefined;
    if (!rpc && !(p.length === 1 && p[0] === "events")) return void socket.destroy();
    wss.handleUpgrade(req, socket, head, (w) => {
      // A connected device holds the bench up whichever socket it holds.
      idle.opened();
      w.on("close", () => idle.closed());
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
