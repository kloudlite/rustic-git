/**
 * When a pi event means the platform's own state may have moved under us.
 *
 * The desktop renders workspaces and environments from `/v1`, so anything a tool did there — a
 * package installed, an environment attached, a workspace started — is invisible until the next
 * read. The bench's file and shell tools are excluded on purpose: they change the workspace's
 * disk, not the objects this window lists, and a refresh per `read` would be a poll.
 *
 * Kept free of Solid and `window` so `node --test` can hold it (bench/test/renderer-refresh.test.ts).
 */
export function shouldRefreshOn(ev: { type?: string; toolName?: unknown }): boolean {
  if (ev.type !== "tool_execution_end" || typeof ev.toolName !== "string") return false;
  return /^(kl_|workspace_|environment_)/.test(ev.toolName);
}
