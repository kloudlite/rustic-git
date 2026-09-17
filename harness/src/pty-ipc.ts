/**
 * What main will accept as a shell's id and scope. Its own file only so the
 * rule can be tested without importing main.ts (which needs electron).
 *
 * The scope rule is the bench's own (`bench` or `ws-<16 hex>`): anything else
 * is refused here rather than dialled, so a renderer bug cannot make the main
 * process open a socket at an address it invented. There is no session name any
 * more: a socket IS the shell in that pod's sidecar (spec §2.3).
 */
const ID = /^t\d+$/;
const SCOPE = /^(bench|ws-[0-9a-f]{16})$/;
export function checkScope(scope: unknown): string {
  if (typeof scope !== "string" || !SCOPE.test(scope)) throw new Error("not a shell scope");
  return scope;
}

/**
 * What the file-system watch will accept. A watch is named by the WORKSPACE it follows and nothing
 * else — there is one per workspace, no tab and no id — so reusing the shell's check refused every
 * open with "not a terminal id" and the Files tab never heard a thing (owner, 2026-09-18).
 */
export function checkWatch(scope: unknown): string {
  return checkScope(scope);
}


/**
 * Closing a socket that has not finished connecting. `ws` THROWS synchronously from `close()` while
 * a socket is CONNECTING ("WebSocket was closed before the connection was established"), and an
 * uncaught throw in the main process is Electron's own crash dialog — which is what the owner saw
 * when a Files tab was left before its watch had opened (2026-09-18). A connecting socket is
 * terminated, an open one is closed, and neither ever throws out of here.
 */
export function closeSocket(w: { readyState: number; close(): void; terminate?: () => void } | undefined): void {
  if (!w) return;
  try {
    // 0 is CONNECTING in every ws implementation and in the browser.
    if (w.readyState === 0 && w.terminate) w.terminate();
    else w.close();
  } catch {
    /* a socket that could not be closed is already going away */
  }
}

/** Throws with the reason; returns the pair when both are a shell's. */
export function checkPty(id: unknown, scope: unknown): { id: string; scope: string } {
  if (typeof id !== "string" || !ID.test(id)) throw new Error("not a terminal id");
  return { id, scope: checkScope(scope) };
}

/**
 * One ttyd frame, read. The opcode is the FIRST BYTE whichever kind of websocket frame carried it:
 * ttyd 1.7 sends output binary and the title and preferences as text, and a text frame passed
 * through whole printed `1/nix/profile/current/bin/zsh -l (ws)2{ "disableLeaveAlert"…` into the
 * owner's terminal (2026-09-18).
 *
 * An opcode this app does not know is `ignore`, never terminal output: whatever ttyd adds next must
 * not land in a person's scrollback. A whole-JSON text frame with no opcode is the BENCH's own
 * control channel, which it speaks before the shell is attached.
 */
export type TtydFrame =
  | { kind: "data"; data: Uint8Array }
  | { kind: "title"; title: string }
  | { kind: "exit"; code?: number; error?: string }
  | { kind: "ignore" };

export function readTtydFrame(d: Uint8Array, isBinary: boolean): TtydFrame {
  if (d.length === 0) return { kind: "ignore" };
  const opcode = String.fromCharCode(d[0]);
  const body = d.subarray(1);
  if (opcode === "0") return { kind: "data", data: body };
  if (opcode === "1") return { kind: "title", title: Buffer.from(body).toString("utf8") };
  if (opcode === "2") return { kind: "ignore" }; // ttyd's own preferences: this app has its own
  if (isBinary) return { kind: "ignore" };
  try {
    const ev = JSON.parse(Buffer.from(d).toString("utf8")) as { exit?: unknown; error?: unknown };
    if (typeof ev.exit === "number") return { kind: "exit", code: ev.exit };
    if (typeof ev.error === "string") return { kind: "exit", error: ev.error };
  } catch {
    /* an opcode nobody here knows */
  }
  return { kind: "ignore" };
}
