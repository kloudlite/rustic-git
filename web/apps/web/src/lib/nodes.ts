import type { Tone } from "@/lib/console";
import type { AdminNode } from "@/lib/api";
import { isDrained } from "@/lib/clusters";

/** A draining node is `info`, not `warn`: whatever runs there keeps running (CLAUDE.md), so a
 *  planned drain is a state to watch, never a problem. A node that is simply not ready IS the
 *  problem — its running worktrees are interrupted. */
export function nodeTone(n: AdminNode): Tone {
  if (!n.ready) return "critical";
  return n.decommission ? "info" : "ok";
}

/** The last verb appears only once the node's own status reads the sticky `drained <RFC 3339>`:
 *  that stamp is the gate on retiring the machine, and offering it earlier would be offering to
 *  discard bytes no other node holds yet. */
export function nodeVerbs(n: AdminNode): ("drain" | "undrain" | "decommission" | "delete-vm")[] {
  if (!n.decommission) return ["drain", "decommission"];
  return isDrained(n.decommissionStatus) ? ["undrain", "delete-vm"] : ["undrain", "decommission"];
}
