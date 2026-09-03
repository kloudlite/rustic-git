import { CircleCheck, CircleDashed, CircleX, Loader2, Square } from "lucide-react";
import { cn } from "@/lib/utils";
import type { EnvState, WsState } from "@/lib/api";
import { noticesFor } from "@/lib/ws-status";

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
  // The api's enum can grow before this file does; an unknown state is shown by name, not
  // thrown at the error boundary.
  const look = LOOK[state] ?? { Icon: CircleDashed, label: state, cls: "border-border bg-muted text-muted-foreground" };
  return (
    <span className={cn("inline-flex items-center gap-1.5 border px-2 py-0.5 text-caption font-medium", look.cls, className)}>
      <look.Icon className={cn("size-3.5", state === "creating" && "animate-spin")} />
      {look.label}
    </span>
  );
}

/** The waiting-on notices: at most one, rendered where the person is already looking for state.
 *  `text-warning` only for the interrupted case — everything else here is information, and a page
 *  where every line is orange is a page nobody reads. Lives here rather than in either list,
 *  beside the badge it sits under — one look, both kinds. */
export function Notices({ w }: { w: Parameters<typeof noticesFor>[0] }) {
  const notices = noticesFor(w);
  if (notices.length === 0) return null;
  return (
    <>
      {notices.map((n) => (
        <span key={n.text} className={`mt-1 block text-sm2 ${n.tone === "warning" ? "text-warning" : "text-muted-foreground"}`}>
          {n.text}
        </span>
      ))}
    </>
  );
}
