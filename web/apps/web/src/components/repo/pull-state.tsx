import { CircleDot, GitMerge, GitPullRequestClosed } from "lucide-react";
import { cn } from "@/lib/utils";
import type { PullState } from "@/lib/api";

/** What state a change is in, said the same way everywhere it appears. Colour is
 *  never the only signal — each state has its own icon and its own word. */
export function StateBadge({ state, className }: { state: PullState; className?: string }) {
  const look = {
    open: { Icon: CircleDot, label: "Open", cls: "border-success/40 bg-success/10 text-success" },
    merged: { Icon: GitMerge, label: "Merged", cls: "border-primary/40 bg-primary/10 text-primary" },
    closed: { Icon: GitPullRequestClosed, label: "Closed", cls: "border-border bg-muted text-muted-foreground" },
  }[state];
  return (
    <span className={cn("inline-flex items-center gap-1.5 border px-2 py-0.5 text-caption font-medium", look.cls, className)}>
      <look.Icon className="size-3.5" />
      {look.label}
    </span>
  );
}
