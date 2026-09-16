/**
 * What main will accept as a shell's id and scope. Its own file only so the
 * rule can be tested without importing main.ts (which needs electron).
 *
 * The scope rule is the bench's own (`bench` or `ws-<16 hex>`): anything else
 * is refused here rather than dialled, so a renderer bug cannot make the main
 * process open a socket at an address it invented. A session name is the tool
 * server's own rule, refused here too — it ends up in tmux's argv.
 */
const ID = /^t\d+$/;
const SCOPE = /^(bench|ws-[0-9a-f]{16})$/;
const SESSION = /^[a-z0-9][a-z0-9-]{0,47}$/;

/** Throws with the reason; undefined stays undefined (an unnamed shell is today's one-shot). */
export function checkSession(session: unknown): string | undefined {
  if (session === undefined || session === null) return undefined;
  if (typeof session !== "string" || !SESSION.test(session)) throw new Error("not a session name");
  return session;
}

export function checkScope(scope: unknown): string {
  if (typeof scope !== "string" || !SCOPE.test(scope)) throw new Error("not a shell scope");
  return scope;
}

/** Throws with the reason; returns the pair when both are a shell's. */
export function checkPty(id: unknown, scope: unknown): { id: string; scope: string } {
  if (typeof id !== "string" || !ID.test(id)) throw new Error("not a terminal id");
  return { id, scope: checkScope(scope) };
}
