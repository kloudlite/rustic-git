import WebSocket from "ws";
import * as nodePty from "node-pty";

/**
 * The bench's shells. One socket, one shell, one life: no scrollback replay, no
 * session id, no reconnect — a closed socket kills the shell (SIGHUP to the
 * group). Binary frames are raw PTY bytes both ways; text frames are control
 * JSON (`{"resize":{"cols":N,"rows":N}}` in, `{"exit":N}` / `{"error":"…"}` out).
 *
 * Two scopes, one protocol: `bench` forks a shell here, a workspace id splices
 * this socket onto that workspace's tool server `/stream/pty` and forwards
 * frames unchanged, so the far end owns the PTY and we own nothing but the pipe.
 */
export type Resize = { cols: number; rows: number };

const OPEN_MS = 5_000;

/** Never throw out of a send: a socket the peer already closed is the normal end of a shell, not an error. */
function send(w: WebSocket, data: string | Buffer, binary?: boolean): void {
  if (w.readyState === WebSocket.OPEN) w.send(data, binary === undefined ? undefined : { binary });
}

function parseResize(d: Buffer | ArrayBuffer | Buffer[]): Resize | undefined {
  try {
    const v = JSON.parse(d.toString()) as { resize?: { cols?: unknown; rows?: unknown } };
    const r = v.resize;
    if (r && typeof r.cols === "number" && typeof r.rows === "number" && r.cols > 0 && r.rows > 0) return { cols: r.cols, rows: r.rows };
  } catch {
    /* not control JSON */
  }
  return undefined;
}

/**
 * The first frame a client sends is a resize, so the shell never starts at 80x24
 * and reflows; a client that sends none gets the default rather than a hang.
 * Frames that arrived in the same TCP chunk as the resize are replayed on the
 * next-but-one tick, after the caller has attached its own handler.
 * ponytail: a caller that attaches its handler later than one setImmediate (a
 * slow resolve, not a keystroke on an open socket) loses those frames; give it a
 * real buffer if that ever bites.
 */
export function readFirstResize(w: WebSocket, timeoutMs: number): Promise<Resize> {
  return new Promise((resolve) => {
    const pending: [Buffer, boolean][] = [];
    let done = false;
    const finish = (r: Resize) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      setImmediate(() => {
        w.off("message", onMessage);
        resolve(r);
        setImmediate(() => {
          for (const [d, binary] of pending) w.emit("message", d, binary);
        });
      });
    };
    const onMessage = (d: Buffer, binary: boolean) => {
      if (done) return void pending.push([d, binary]);
      const r = binary ? undefined : parseResize(d);
      if (r) finish(r);
      else pending.push([d, binary]);
    };
    const timer = setTimeout(() => finish({ cols: 80, rows: 24 }), timeoutMs);
    w.on("message", onMessage);
    w.once("close", () => {
      done = true;
      clearTimeout(timer);
    });
  });
}

/** A login shell on the bench itself, cwd $HOME. */
export function attachBenchShell(w: WebSocket, env: NodeJS.ProcessEnv, first: Resize): void {
  const e = { ...env, TERM: "xterm-256color", COLORTERM: "truecolor" } as Record<string, string>;
  // A person with a shell here must not be one `cat` away from the platform token; pi's extensions read it, a person does not need it.
  delete e.KL_TOOL_TOKEN_FILE;
  let p: nodePty.IPty;
  try {
    p = nodePty.spawn(e.SHELL ?? "/bin/bash", ["-l"], { name: "xterm-256color", cols: first.cols, rows: first.rows, cwd: e.HOME ?? "/", env: e });
  } catch (err) {
    send(w, JSON.stringify({ error: (err as Error).message }));
    return void w.close();
  }
  let gone = false;
  p.onData((d) => send(w, Buffer.from(d, "utf8"), true));
  p.onExit(({ exitCode }) => {
    gone = true;
    send(w, JSON.stringify({ exit: exitCode }));
    w.close();
  });
  w.on("message", (d: Buffer, binary: boolean) => {
    if (binary) return void p.write(d.toString("utf8"));
    const r = parseResize(d);
    if (r) p.resize(r.cols, r.rows);
  });
  w.on("close", () => {
    if (gone) return;
    try {
      p.kill("SIGHUP");
    } catch {
      /* already reaped */
    }
  });
}

/** Splice this socket onto a workspace tool server's PTY route; frames cross unchanged, either close closes the other. */
export function spliceWorkspaceShell(w: WebSocket, address: string, first: Resize): void {
  const up = new WebSocket(`ws://${address}/stream/pty`);
  let open = false;
  const queue: [Buffer, boolean][] = [];
  const timer = setTimeout(() => {
    if (!open) fail();
  }, OPEN_MS);
  const fail = () => {
    clearTimeout(timer);
    send(w, JSON.stringify({ error: `workspace ${address} did not answer` }));
    w.close();
    up.terminate();
  };
  up.on("open", () => {
    open = true;
    clearTimeout(timer);
    up.send(JSON.stringify({ resize: first }));
    for (const [d, binary] of queue) up.send(d, { binary });
    queue.length = 0;
  });
  up.on("message", (d: Buffer, binary: boolean) => send(w, d, binary));
  up.on("close", () => {
    clearTimeout(timer);
    w.close();
  });
  up.on("error", () => (open ? w.close() : fail()));
  // Anything typed before the far end answered still belongs to that shell.
  w.on("message", (d: Buffer, binary: boolean) => (open ? up.send(d, { binary }) : queue.push([d, binary])));
  w.on("close", () => {
    clearTimeout(timer);
    up.close();
  });
}
