//! What a deleted workspace leaves on the bench: its sessions. Shared by main's
//! `workspace_delete` (local.ts).
import { dropSessions } from "@kloudlite-tui/agent";
import type { SessionHandle } from "./index.ts";

/** Sessions live on the bench, the workspace on its pod: the platform deletes first, and only a
 * delete it accepted closes the workspace's open sessions and drops their history here. */
export const forgetSessions = async (ws: string, live: Map<string, SessionHandle>) => {
  for (const [key, h] of [...live]) if (key === ws || key.startsWith(`${ws}:`)) await h.dispose();
  dropSessions(ws);
};
