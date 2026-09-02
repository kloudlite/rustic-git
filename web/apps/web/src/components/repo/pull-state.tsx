import { CircleDot, GitMerge, GitPullRequestClosed } from "lucide-react";
import { cn } from "@/lib/utils";
import type { PullState } from "@/lib/api";

/** What state a change is in, said the same way everywhere it appears. Colour is never the
 *  only signal — each state has its own icon and its own word. Exported for the test: the
 *  fallback in `StateBadge` below is the whole point of this map, and asserting it through
 *  rendered JSX would need a DOM for one property lookup. Typed as `Record<PullState, …>` so
 *  a `PullState` added without a matching entry here is a compile error, not a runtime one. */
export const LOOK: Record<PullState, { Icon: typeof CircleDot; label: string; cls: string }> = {
  open: { Icon: CircleDot, label: "Open", cls: "border-success/40 bg-success/10 text-success" },
  merged: { Icon: GitMerge, label: "Merged", cls: "border-primary/40 bg-primary/10 text-primary" },
  closed: { Icon: GitPullRequestClosed, label: "Closed", cls: "border-destructive/40 bg-destructive/10 text-destructive" },
};

export function StateBadge({ state, className }: { state: PullState; className?: string }) {
  // A state the wire grows and this build has not heard of (`draft`) is `undefined` here, and
  // the badge sits inside the pulls list and the PR header — a throw takes both down over a
  // pill. Same fallback the activity feed uses for an unknown event kind.
  const look = LOOK[state] ?? LOOK.open;
  return (
    <span className={cn("inline-flex items-center gap-1.5 border px-2 py-0.5 text-caption font-medium", look.cls, className)}>
      <look.Icon className="size-3.5" />
      {look.label}
    </span>
  );
}
