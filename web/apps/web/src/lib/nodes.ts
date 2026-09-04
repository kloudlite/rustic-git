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

/** `decommission` cordons the node, which is what gates deleting its VM by hand — so it appears
 *  only once the node's own status reads the sticky `drained <RFC 3339>`. Offering it mid-drain
 *  would be offering to retire bytes no other node holds yet (and the api 409s it anyway). */
export function nodeVerbs(n: AdminNode): ("drain" | "undrain" | "decommission")[] {
  if (!n.decommission) return ["drain"];
  return isDrained(n.decommissionStatus) ? ["undrain", "decommission"] : ["undrain"];
}
