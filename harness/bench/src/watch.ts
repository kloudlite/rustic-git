import WebSocket from "ws";

/**
 * A workspace's file-system watch, spliced to the desktop.
 *
 * The tool server already watches (`crates/ide/src/watches.rs`): `POST /tools/watch {paths:["."]}`
 * starts a recursive notify watch under the workspace root and answers an id, and
 * `GET /stream/watch/{id}` is every event as one JSON text frame. Nothing here is a second watcher —
 * the desktop reloaded the whole tree on every visit because it had no way to hear about a change
 * (owner: "can't we use what VSCode is using to sync fs?").
 *
 * Two things the tool server does not do, done here rather than in the pod:
 *
 * 1. Its event path is ABSOLUTE (`/home/kl/workspaces/api/src/main.ts`). Everything above speaks
 *    workspace-relative paths, so the root — from `/healthz` — is stripped once per stream.
 * 2. It filters nothing. A build writing into `.cache` or `node_modules` would be thousands of
 *    events a second for rows no tree draws, so those are dropped here, matching what `/fs/tree`
 *    already marks ignored.
 */
export const IGNORED = [".cache", "graft", "node_modules", ".git", "target", ".direnv"];

export type WatchEvent = { path: string; kind: string };

/** Absolute event path → what the desktop names it, or nothing when no view would draw it. */
export function relative(root: string, path: unknown): string | undefined {
  if (typeof path !== "string") return undefined;
  const base = root.replace(/\/+$/, "");
  if (!path.startsWith(`${base}/`)) return undefined;
  const rel = path.slice(base.length + 1);
  if (!rel) return undefined;
  // An ignored DIRECTORY anywhere in the path, not a file that merely starts with the name.
  if (rel.split("/").some((seg) => IGNORED.includes(seg))) return undefined;
  return rel;
}

/**
 * One frame from `/stream/watch` → what crosses to the desktop, or nothing. The stream also carries
 * its own notices (`{state:"stopped"}`, `{dropped_events:N}`); a drop means the desktop's tree may
 * have missed something, so it is passed on as a reason to read everything again.
 */
export function watchFrame(root: string, raw: string): WatchEvent | { resync: true } | undefined {
  let ev: Record<string, unknown>;
  try {
    ev = JSON.parse(raw) as Record<string, unknown>;
  } catch {
    return undefined;
  }
  if (typeof ev.dropped_events === "number" || ev.state === "stopped") return { resync: true };
  const path = relative(root, ev.path);
  if (!path) return undefined;
  return { path, kind: String(ev.kind ?? "other") };
}

type Fetch = typeof fetch;

/**
 * Start a watch on the workspace root and splice its events to `w`. The watch belongs to this
 * socket: the desktop going away stops it, so a workspace never accumulates watchers (the tool
 * server allows 32 and then refuses).
 */
export async function spliceWatch(w: WebSocket, address: string, f: Fetch = fetch, token?: string): Promise<void> {
  // `/healthz` is open; `/tools/*` and `/stream/*` require the workspace's token. It is a header
  // and a socket option, never a log line and never anything the desktop is told.
  const auth: Record<string, string> = token ? { authorization: `Bearer ${token}` } : {};
  const fail = (why: string) => {
    if (w.readyState === WebSocket.OPEN) w.send(JSON.stringify({ error: why }));
    w.close();
  };
  let root: string;
  let id: string;
  try {
    const health = (await (await f(`http://${address}/healthz`)).json()) as { root?: string };
    root = String(health.root ?? "");
    if (!root) throw new Error("the workspace does not say where it is");
    const started = (await (
      await f(`http://${address}/tools/watch`, { method: "POST", headers: { "content-type": "application/json", ...auth }, body: JSON.stringify({ paths: ["."] }) })
    ).json()) as { id?: string; error?: string };
    if (!started.id) throw new Error(started.error ?? "the watch did not start");
    id = started.id;
  } catch (e) {
    return fail((e as Error).message);
  }
  const up = new WebSocket(`ws://${address}/stream/watch/${id}`, { headers: auth });
  const stop = () => {
    up.close();
    // Best effort: a watch whose socket is gone is dead weight in a pod that allows 32.
    void f(`http://${address}/tools/watch_stop`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ id }) }).catch(() => undefined);
  };
  up.on("message", (d: Buffer) => {
    const out = watchFrame(root, d.toString());
    if (out && w.readyState === WebSocket.OPEN) w.send(JSON.stringify(out));
  });
  up.on("error", () => fail("the workspace's watch stream dropped"));
  up.on("close", () => w.close());
  w.on("close", stop);
  w.on("error", stop);
}
