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

/** Throws with the reason; returns the pair when both are a shell's. */
export function checkPty(id: unknown, scope: unknown): { id: string; scope: string } {
  if (typeof id !== "string" || !ID.test(id)) throw new Error("not a terminal id");
  return { id, scope: checkScope(scope) };
}
