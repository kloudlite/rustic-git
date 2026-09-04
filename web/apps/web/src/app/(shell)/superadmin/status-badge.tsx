import { Badge } from "@/components/ui/badge";
import { settled } from "@/lib/settings";
import { settingsStatusTone } from "@/lib/clusters";
import type { WorkloadDoc, AdminNode } from "@/lib/api";

/** Quiet rollout state — "rolling" only while ready trails desired, "settled" once it hasn't. */
export function RolloutBadge({ w }: { w: Pick<WorkloadDoc, "rolloutState" | "ready" | "desired"> }) {
  return settled(w) ? (
    <Badge variant="outline">settled</Badge>
  ) : (
    <Badge variant="secondary">rolling {w.ready}/{w.desired}</Badge>
  );
}

/** Quiet node state — "active" is the common case and says nothing; a node mid-drain or not
 *  ready is the one worth a person's attention. */
export function NodeBadge({ n }: { n: Pick<AdminNode, "ready" | "decommission" | "decommissionStatus"> }) {
  if (n.decommission) return <Badge variant="secondary">{n.decommissionStatus ?? "draining"}</Badge>;
  if (!n.ready) return <Badge variant="destructive">not ready</Badge>;
  return <Badge variant="outline">active</Badge>;
}

/** A region's active/inactive — the only two the api writes, but rendered defensively since it's
 *  a plain string on the wire. */
export function RegionStatusBadge({ status }: { status: string }) {
  return status === "active" ? <Badge variant="outline">active</Badge> : <Badge variant="secondary">{status}</Badge>;
}

/** `present`/`absent`/`stale (lag N)` — anything else reads as neutral rather than
 *  breaking the row (`lib/clusters.ts::settingsStatusTone`). */
export function SettingsStatusBadge({ status }: { status: string }) {
  const tone = settingsStatusTone(status);
  if (tone === "present") return <Badge variant="outline">present</Badge>;
  if (tone === "stale") return <Badge variant="destructive">stale</Badge>;
  return <Badge variant="secondary">{tone === "absent" ? "absent" : status}</Badge>;
}
