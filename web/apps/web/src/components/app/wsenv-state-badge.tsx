import { CircleCheck, CircleDashed, CircleX, Loader2, Square } from "lucide-react";
import { cn } from "@/lib/utils";
import type { EnvState, WsState } from "@/lib/api";

/** Same idiom as `pull-state.tsx`'s `StateBadge`: one look per state, said the
 *  same way everywhere a workspace or environment's state is shown. Workspace
 *  and environment states overlap everywhere but `running`/`ready`, so one map
 *  covers both rather than two near-identical components. */
const LOOK: Record<WsState | EnvState, { Icon: typeof CircleCheck; label: string; cls: string }> = {
  creating: { Icon: Loader2, label: "Creating", cls: "border-warning/40 bg-warning/10 text-warning" },
  ready: { Icon: CircleCheck, label: "Ready", cls: "border-success/40 bg-success/10 text-success" },
  running: { Icon: CircleCheck, label: "Running", cls: "border-success/40 bg-success/10 text-success" },
  stopped: { Icon: Square, label: "Stopped", cls: "border-border bg-muted text-muted-foreground" },
  error: { Icon: CircleX, label: "Error", cls: "border-destructive/40 bg-destructive/10 text-destructive" },
  deleted: { Icon: CircleDashed, label: "Deleted", cls: "border-border bg-muted text-muted-foreground" },
};

export function WsEnvStateBadge({ state, className }: { state: WsState | EnvState; className?: string }) {
  const look = LOOK[state];
  return (
    <span className={cn("inline-flex items-center gap-1.5 border px-2 py-0.5 text-caption font-medium", look.cls, className)}>
      <look.Icon className={cn("size-3.5", state === "creating" && "animate-spin")} />
      {look.label}
    </span>
  );
}
