import WebSocket from "ws";

/**
 * The bench's shells. One socket, one pipe: a closed socket ends this attachment,
 * and whether the shell outlives it is the far end's business — a `session=` name
 * puts it in tmux there, no name is today's bare shell. Binary frames are raw PTY bytes both ways; text frames are control
 * JSON (`{"resize":{"cols":N,"rows":N}}` in, `{"exit":N}` / `{"error":"…"}` out).
 *
 * Two scopes, one protocol, one mechanism: both splice this socket onto a tool
 * server's `/stream/pty` and forward frames unchanged, so the far end owns the
 * PTY and we own nothing but the pipe. `bench` is the workspace container beside
 * us on 127.0.0.1 (the bench IS a workspace pod now, so its shell is the
 * person's zsh with their Nix profile, not a shell forked in this container).
 */
export type Resize = { cols: number; rows: number };

const OPEN_MS = 5_000;

/**
 * A terminal session name. The desktop mints it, the tool server validates it too, and we refuse
 * it here as well: it lands in a URL and then in a `tmux -L kl` argument, so a leading dash (an
 * option to tmux) is out even though the charset would allow it.
 */
export const SESSION_RE = /^[a-z0-9][a-z0-9-]{0,47}$/;

/** Proxy the tool server's session listing/kill; an unreachable pod is a 502, not a crash. */
export async function toolSessions(address: string, method: "GET" | "DELETE", name?: string): Promise<{ code: number; body?: unknown }> {
  let r: Response;
  try {
    r = await fetch(`http://${address}/stream/pty/sessions${name === undefined ? "" : `/${name}`}`, { method });
  } catch (e) {
    return { code: 502, body: { error: `workspace ${address} did not answer: ${(e as Error).message}` } };
  }
  const text = await r.text();
  if (!text) return { code: r.status };
  try {
    return { code: r.status, body: JSON.parse(text) as unknown };
  } catch {
    return { code: 502, body: { error: `workspace ${address} answered ${r.status} with non-JSON` } };
  }
}

/** Never throw out of a send: a socket the peer already closed is the normal end of a shell, not an error. */
function send(w: WebSocket, data: string | Buffer, binary?: boolean): void {
  if (w.readyState !== WebSocket.OPEN) return;
  if (binary === undefined) w.send(data);
  else w.send(data, { binary });
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
 * Frames are HELD from the upgrade until the shell is attached: the first text frame that parses
 * as a resize answers `first` (or the default after `timeoutMs`), and everything — the resize's
 * TCP-chunk neighbours, the bytes a probe types straight after it, a second resize — stays in
 * `pending` until `release()` re-emits it, synchronously, to whatever handlers are attached by
 * then. Re-emitting on a timer instead lost every byte typed before a slow workspace resolve
 * (2026-09-16 `bench.shell.workspace`: the prompt arrived, the command never did, 30 s timeout).
 */
export function holdFrames(w: WebSocket, timeoutMs: number): { first: Promise<Resize>; release(): void } {
  const pending: [Buffer, boolean][] = [];
  let resolveFirst!: (r: Resize) => void;
  let answered = false;
  const answer = (r: Resize) => {
    if (answered) return;
    answered = true;
    clearTimeout(timer);
    resolveFirst(r);
  };
  const first = new Promise<Resize>((res) => (resolveFirst = res));
  const onMessage = (d: Buffer, binary: boolean) => {
    const r = binary || answered ? undefined : parseResize(d);
    if (r) return answer(r);
    pending.push([d, binary]);
  };
  const timer = setTimeout(() => answer({ cols: 80, rows: 24 }), timeoutMs);
  w.on("message", onMessage);
  w.once("close", () => {
    clearTimeout(timer);
    w.off("message", onMessage);
  });
  return {
    first,
    release() {
      w.off("message", onMessage);
      for (const [d, binary] of pending.splice(0)) w.emit("message", d, binary);
    },
  };
}

/** Splice this socket onto a workspace tool server's PTY route; frames cross unchanged, either close closes the other. */
export function spliceWorkspaceShell(w: WebSocket, address: string, first: Resize, session?: string): void {
  // The name names a tmux session on the far end; without one the tool server gives today's bare shell.
  const up = new WebSocket(`ws://${address}/stream/pty${session === undefined ? "" : `?session=${session}`}`);
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
