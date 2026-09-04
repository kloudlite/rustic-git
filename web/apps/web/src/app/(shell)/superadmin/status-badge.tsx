import { Badge } from "@/components/ui/badge";
import { settled } from "@/lib/settings";
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
