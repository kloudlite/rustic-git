import type { Overview } from "@/lib/api";

/** Nothing needs a decision — the landing view's one branch between "queue plus alerts" and
 *  "just the fleet". Pure so the branch is checkable without a fetch. */
export function needsNothing(o: Pick<Overview, "pendingRequests" | "attention">): boolean {
  return o.pendingRequests.length === 0 && o.attention.length === 0;
}
