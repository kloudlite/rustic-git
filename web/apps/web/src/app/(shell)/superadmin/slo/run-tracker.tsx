import type { SloRun, SloRunState, SloStep } from "@/lib/api";
import type { Tone } from "@/lib/console";
import { cn } from "@/lib/utils";
import { stagesOf } from "@/lib/slo";

export function runTone(state: SloRunState): Tone {
  return state === "failed" ? "critical" : state === "passed" ? "ok" : "info";
}

/** Durations here are seconds, not `when()`'s relative words: a run is tens of seconds long and
 *  "less than a minute ago" says nothing about whether it got slower. */
export function seconds(ms: number): string {
  return ms >= 60_000 ? `${Math.round(ms / 6_000) / 10} min` : `${Math.round(ms / 100) / 10} s`;
}

const CHIP: Record<"ok" | "failed" | "skipped", string> = {
  ok: "border-primary/40 bg-primary/10 text-primary",
  failed: "border-destructive/40 bg-destructive/10 text-destructive",
  skipped: "border-border bg-muted text-muted-foreground",
};

function outcome(s: SloStep) {
  return s.skipped ? "skipped" : s.ok ? "ok" : "failed";
}

/** The journey a run walks, one row per stage and one chip per step. The stage the run is
 *  currently in pulses — `motion-safe:` so a viewer who asked for no motion gets the ring alone,
 *  which carries the same information. A finished run has no current stage, so nothing pulses. */
export function RunTracker({ run, steps }: { run: SloRun; steps: SloStep[] }) {
  const stages = stagesOf(steps);
  return (
    <ol className="flex flex-col gap-3">
      {stages.map((st, i) => {
        const current = run.state === "running" && st.stage === run.stage;
        return (
          <li key={`${st.stage}-${i}`} className="flex flex-col gap-1.5 sm:flex-row sm:items-baseline sm:gap-4">
            <p
              className={cn(
                "shrink-0 text-sm2 font-medium sm:w-44",
                current ? "text-primary motion-safe:animate-pulse" : "text-muted-foreground",
              )}
            >
              {st.stage}
            </p>
            <ul className={cn("flex flex-wrap gap-1.5", current && "ring-1 ring-primary/40 p-1")}>
              {st.steps.map((s) => (
                <li
                  key={`${s.slo_id}-${s.ts}`}
                  title={`${s.slo_id} · ${s.ms} ms${s.detail ? ` · ${s.detail}` : ""}`}
                  className={cn("inline-flex h-5 items-center border px-1.5 text-micro whitespace-nowrap", CHIP[outcome(s)])}
                >
                  {s.slo_id}
                </li>
              ))}
            </ul>
          </li>
        );
      })}
    </ol>
  );
}
