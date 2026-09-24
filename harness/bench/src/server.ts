import http from "node:http";
import { WebSocketServer, type WebSocket } from "ws";
import type { Bench } from "./bench.ts";
import { Idle } from "./idle.ts";
import { holdFrames, spliceShell } from "./pty.ts";
import { spliceWatch } from "./watch.ts";
import { handleOperationControl, operationControlError, type OperationControlOptions } from "./operations/control.ts";
import { reachable } from "./engine/index.ts";

/**
 * harness-bench's surface. Where it listens is main's choice: the pod IP
 * behind the platform's gateway-only NetworkPolicy, or loopback on a laptop.
 * Sessions run the in-process sys-1 engine; there is no pi RPC socket any more.
 */
const status = (e: Error) => (/no session/.test(e.message) ? 404 : /read-only|not writable|in flight|only open session|belongs to|is closed/.test(e.message) ? 409 : 400);

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
  if ((p[0] === "sessions" || p[0] === "workspaces") && p[1] !== undefined && (!/^[A-Za-z0-9._-]+$/.test(p[1]) || p[1] === "." || p[1] === ".."))
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
  // Tests pass a fake; production reads the bench's own Platform (main.ts's Platform.fromEnv()).
  opts: { resolveTools?: (ws: string, fresh?: boolean) => Promise<{ address: string; token?: string }> } & OperationControlOptions = {},
): Promise<{ port: number; close(): Promise<void>; server: http.Server; sweepOnce(): void }> {
  const resolveAuth = opts.resolveTools ?? ((ws: string, fresh?: boolean) => bench.platform!.tools(ws, fresh));
  /**
   * Where a scope's tool server is AND the token every `/tools/*`, `/fs/*` and `/stream/*` call
   * must carry. The bench's own container (`bench`) is loopback in the same pod and takes none.
   * The token is a header and nothing else: never a log line, never an answer to the renderer.
   */
  const toolsFor = async (scope: string): Promise<{ address: string; token?: string }> => {
    if (scope === "bench") return { address: LOCAL_TOOLS };
    return resolveAuth(scope);
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
    // after= is a message index, never a timestamp.
    const n = (k: string) => (u.searchParams.has(k) ? Number(u.searchParams.get(k)) : undefined);
    const m = req.method ?? "GET";
    try {
      const p = segments(u.pathname);
      if (await handleOperationControl(req, res, u, p, () => body(req), opts)) return;
      // `model` is the bench's DEFAULT model. A window that opens before any session row has loaded
      // still has to name what will answer; without it the composer said "no model" (owner, 2026-09-17).
      if (m === "GET" && u.pathname === "/healthz") return send(res, 200, { ok: true, model: bench.model, readOnly: bench.readOnly, writable: bench.writable.ok(), reason: bench.writable.reason(), ...idle.state() });
      if (m === "GET" && u.pathname === "/models") {
        const providers = ["anthropic", "deepseek"].filter((p) => !reachable(`${p}/x`));
        const defaultProvider = bench.model.split("/")[0];
        if (!providers.includes(defaultProvider) && !reachable(bench.model)) providers.push(defaultProvider);
        return send(res, 200, { default: bench.model, providers });
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
        if (p.length === 3 && m === "GET" && p[2] === "children") return send(res, 200, await bench.children(p[1]));
        if (p.length === 3 && m === "POST" && p[2] === "send") return send(res, 200, await bench.send(p[1], String((await body(req)).text ?? "")));
        if (p.length === 3 && m === "POST" && p[2] === "abort") {
          await bench.abort(p[1]);
          return send(res, 204);
        }
        if (p.length === 3 && m === "POST" && p[2] === "model") {
          if (!bench.sessions.get(p[1])) return send(res, 404, { error: `no session ${p[1]}` });
          const model = String((await body(req)).model ?? "");
          const why = reachable(model);
          if (why) return send(res, 422, { error: why });
          return send(res, 200, bench.setModel(p[1], model));
        }
      }
      if (p[0] === "workspaces") {
        // A thread never opened has no history yet, which is an empty one, not a missing route.
        const thread = (id: string) => (bench.sessions.get(id) ? bench.messages(id, n("after"), n("limit")) : Promise.resolve({ messages: [], total: 0 }));
        if (p.length === 3 && p[2] === "session" && m === "POST") return send(res, 200, await bench.openWorkspace(p[1]));
        if (p.length === 3 && p[2] === "messages" && m === "GET") return send(res, 200, await bench.workspaceMessages(p[1], n("after"), n("limit")));
        if (p.length === 5 && p[2] === "eph" && p[4] === "session" && m === "POST") return send(res, 200, await bench.openEphemeral(p[1], p[3]));
        if (p.length === 5 && p[2] === "eph" && p[4] === "messages" && m === "GET") return send(res, 200, await thread(`e-${p[3]}`));
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
          messages: session ? await bench.messages(session, undefined, undefined) : undefined,
          session,
        });
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
          const fileBody = Buffer.from(await r.arrayBuffer());
          if (r.status >= 400) {
            let why: unknown = fileBody.toString("utf8");
            try {
              why = JSON.parse(String(why));
            } catch {
              /* a tool server that answered text answers text */
            }
            return send(res, r.status, why as never);
          }
          // Text is sent as text; anything else is named and measured, never streamed into a view.
          const text = /^text\/|json|javascript|xml|^application\/(x-)?(sh|toml|yaml)/.test(mime) ? fileBody.toString("utf8") : undefined;
          return send(res, 200, { etag: tag, mime, bytes: fileBody.length, ...(text === undefined ? { binary: true } : { text }) });
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
  const unsubscribe = bench.onEvent((ev) => {
    idle.check();
    const frame = JSON.stringify(ev);
    for (const w of events) w.send(frame);
  });

  server.on("upgrade", (req, socket, head) => {
    let p: string[], u: URL;
    try {
      u = new URL(req.url ?? "/", "http://bench");
      p = segments(u.pathname);
    } catch {
      return void socket.destroy();
    }
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
      // No session name: a terminal is a live socket to the pod's shell and nothing more.
    }
    if (scope === undefined && watch === undefined && !(p.length === 1 && p[0] === "events")) return void socket.destroy();
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
          return resolveAuth(scope!).then(
            (a) => {
              // The tool server's address, with the port swapped: same pod, the shell beside it.
              spliceShell(w, a.address, first);
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
      events.add(w);
      // The standard ws heartbeat. A socket the edge or a dead laptop left HALF-OPEN still looks
      // writable, so events were being sent into it forever; the sweep closes it instead.
      alive.set(w, true);
      w.on("pong", () => alive.set(w, true));
      w.on("close", () => (events.delete(w), alive.delete(w)));
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
