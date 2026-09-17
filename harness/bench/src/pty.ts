import WebSocket from "ws";

/**
 * The person's shell, spliced. Every pod carries a `shell` sidecar running `ttyd` on 7790 with the
 * home mounted and nothing else (spec §2): the terminal is a socket to THAT, never a PTY forked in
 * a container that holds code or a token. The tool server's own `/stream/pty` is retired with this.
 *
 * ttyd's protocol is one byte of opcode then the payload, subprotocol `tty`:
 *
 * | direction | opcode | payload |
 * |---|---|---|
 * | client → | `0` | input bytes |
 * | client → | `1` | `{"columns":N,"rows":N}` |
 * | client → | (first frame) | `{"AuthToken":"","columns":N,"rows":N}` |
 * | → client | `0` | output bytes |
 * | → client | `1` | the title |
 * | → client | `2` | ttyd's preferences JSON |
 *
 * Frames cross unchanged: the shell is the far end's, and this owns nothing but the pipe. A dropped
 * socket is a finished shell — there is no session table, no replay and no reattach (§2.3).
 */
export type Resize = { cols: number; rows: number };

const OPEN_MS = 5_000;
/** ttyd's own subprotocol name; it refuses a socket that does not ask for it. */
export const TTYD_SUBPROTOCOL = "tty";
/** The shell sidecar's port, in every pod (spec §2.2). */
export const SHELL_PORT = 7790;

/** `10.42.3.190:7788` → `10.42.3.190:7790`: same pod, the sidecar beside the tool server. */
export const shellAddress = (address: string): string => `${address.replace(/:\d+$/, "")}:${SHELL_PORT}`;

/** ttyd's first client frame: an empty token (the NetworkPolicy is the fence, §2.3) and the size. */
export const authFrame = (first: Resize): string => JSON.stringify({ AuthToken: "", columns: first.cols, rows: first.rows });

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

/**
 * Splice this socket onto a pod's ttyd. The desktop speaks the same protocol on its side, so the
 * frames cross UNCHANGED in both directions and neither end re-encodes what the other typed; the
 * one thing added here is ttyd's opening frame, built from the size the client asked for.
 */
export function spliceShell(w: WebSocket, address: string, first: Resize): void {
  const up = new WebSocket(`ws://${shellAddress(address)}/ws`, [TTYD_SUBPROTOCOL]);
  let open = false;
  const queue: [Buffer, boolean][] = [];
  const timer = setTimeout(() => {
    if (!open) fail();
  }, OPEN_MS);
  const fail = () => {
    clearTimeout(timer);
    send(w, JSON.stringify({ error: `shell ${shellAddress(address)} did not answer` }));
    w.close();
    up.terminate();
  };
  up.on("open", () => {
    open = true;
    clearTimeout(timer);
    up.send(authFrame(first));
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
