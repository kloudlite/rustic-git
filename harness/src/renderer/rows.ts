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
