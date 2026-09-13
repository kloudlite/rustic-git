/**
 * Pure readings of the bench's rows, kept free of Solid and `window` so
 * `node --test` can hold them (bench/test/renderer-rows.test.ts).
 */
export type SessionRow = { id: string; name: string; seq: number; lastActive?: number; archived?: boolean; file?: string; kind?: string };

/** The sidebar's sessions: a workspace or ephemeral thread is the bench's too, but never listed here. */
export const benchSessions = <T extends SessionRow>(rows: T[]): T[] => rows.filter((r) => (r.kind ?? "bench") === "bench");

/** A process row as the task page reads it: a lost row found gone at start is lost, never running. */
export function procState(p: { ended?: number; code?: number | null; lost?: true }): "running" | "done" | "failed" | "lost" {
  if (p.lost) return "lost";
  if (p.ended === undefined) return "running";
  return p.code === 0 ? "done" : "failed";
}

/** A process's one-word status line. */
export function procLabel(p: { ended?: number; code?: number | null; lost?: true }): string {
  const s = procState(p);
  return s === "running" ? "running" : s === "lost" ? "lost" : `exited ${p.code}`;
}

/** The bench's 409 on delete names what is in flight; anything else is not a confirm. */
export function inFlightItems(message: string): string[] | undefined {
  const at = message.indexOf("in flight: ");
  return at < 0 ? undefined : message.slice(at + 11).split(", ").filter(Boolean);
}

/** pi commands that change what the bench saves; the rest only read or steer a running turn. */
const WRITES = new Set(["prompt", "steer", "follow_up", "new_session", "compact", "set_model"]);

/**
 * Why a pi command must not be sent now, or undefined to send it. One answer
 * for every path (composer, slash, palette, keys), so none fails silently.
 */
export function refusal(cmd: { type: string }, st: { session?: string; connected: boolean; writable: { ok: boolean; reason?: string } }): string | undefined {
  if (!st.session) return "no session yet; start one with /new or the + beside Sessions";
  if (!st.connected) return "not connected to the bench; nothing was sent";
  if (WRITES.has(cmd.type) && !st.writable.ok) return `the bench cannot save right now (${st.writable.reason}); nothing was sent`;
  return undefined;
}
