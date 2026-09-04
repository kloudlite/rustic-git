import { Badge } from "@/components/ui/badge";
import { settled } from "@/lib/settings";
import type { WorkloadDoc } from "@/lib/api";

/** Quiet rollout state — "rolling" only while ready trails desired, "settled" once it hasn't. */
export function RolloutBadge({ w }: { w: Pick<WorkloadDoc, "rolloutState" | "ready" | "desired"> }) {
  return settled(w) ? (
    <Badge variant="outline">settled</Badge>
  ) : (
    <Badge variant="secondary">rolling {w.ready}/{w.desired}</Badge>
  );
}
