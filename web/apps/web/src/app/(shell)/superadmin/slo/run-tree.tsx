"use client";

import { useEffect, useState } from "react";
import type { SloRun, SloRunDetail, SloStatus, SloStep } from "@/lib/api";
import { msLabel, progressOf, runTone, targetMs, treeOf, type StageNode, type StageState } from "@/lib/slo";
import { cn } from "@/lib/utils";
import { Pill } from "../ui/pill";

/** The run tree: the whole journey as a list, one row per stage and its steps indented beneath,
 *  with what has not happened yet drawn muted so the reader sees the shape of the run rather than
 *  a list that grows a row at a time.
 *
 *  A client component for two reasons and no others: a stage row is a `<button>` that collapses,
 *  and a running run's clocks tick. Everything else is derived in `lib/slo.ts`, on the server's
 *  data, so this file holds layout and nothing that could disagree with the api. */

/** One glyph vocabulary for stages and steps — inline SVG, because this is five circles and a
 *  dash, not a reason to carry an icon library. The running glyph pulses `motion-safe:` only; the
 *  filled dot carries the same meaning to a viewer who asked for no motion. */
export function Glyph({ state, className }: { state: StageState; className?: string }) {
  const tone =
    state === "failed"
      ? "text-destructive"
      : state === "passed"
        ? "text-primary"
        : state === "running"
          ? "text-primary motion-safe:animate-pulse"
          : "text-muted-foreground/60";
  return (
    <svg viewBox="0 0 12 12" className={cn("size-3 shrink-0", tone, className)} aria-hidden>
      <circle
        cx="6"
        cy="6"
        r={state === "running" ? 3.5 : 4}
        fill={state === "passed" || state === "failed" ? "currentColor" : "none"}
        stroke="currentColor"
        strokeWidth={state === "running" ? 2.5 : 1.25}
      />
      {state === "skipped" && <path d="M3.5 6h5" stroke="currentColor" strokeWidth="1.25" />}
    </svg>
  );
}

/** Every row of the tree shares this grid, which is what puts the stage numbers, the counts and
 *  the milliseconds each in one column down the whole panel. */
const ROW = "grid grid-cols-[2.25rem_minmax(0,1fr)_5rem_7rem] items-baseline gap-3 px-4";

/** Elapsed since an instant, ticking every second. The first value is computed during render, on
 *  the server too, so the HTML carries a real clock rather than a dash somebody screenshots —
 *  `suppressHydrationWarning` because the browser is by definition a second or two later, which is
 *  the one hydration difference that is correct rather than a bug. */
function Elapsed({ since, className }: { since: string; className?: string }) {
  const [ms, setMs] = useState(() => Date.now() - new Date(since).getTime());
  useEffect(() => {
    const from = new Date(since).getTime();
    const t = setInterval(() => setMs(Date.now() - from), 1_000);
    return () => clearInterval(t);
  }, [since]);
  return (
    <span suppressHydrationWarning className={cn("tabular-nums", className)}>
      {msLabel(ms)}
    </span>
  );
}

function StepRow({ node, slo }: { node: { id: string; step: SloStep | null }; slo: SloStatus | undefined }) {
  const s = node.step;
  const state: StageState = !s ? "pending" : s.skipped ? "skipped" : s.ok ? "passed" : "failed";
  const ceiling = targetMs(slo?.target);
  const over = s != null && ceiling != null && s.ms > ceiling;
  return (
    <li className={cn(ROW, "border-b border-border py-1.5 last:border-b-0 hover:bg-muted/60", !s && "text-muted-foreground")}>
      <span />
      <span className="flex min-w-0 flex-col pl-8">
        <span className="flex min-w-0 items-baseline gap-2">
          <Glyph state={state} className="translate-y-0.5" />
          <span className="shrink-0 font-mono text-caption">{node.id}</span>
          <span className={cn("truncate text-sm2", s ? "text-muted-foreground" : "text-muted-foreground/70")}>
            {slo?.sli ?? ""}
          </span>
        </span>
        {/* Why a step failed or was skipped is the sentence an operator is here for; two lines of
            it, and the whole of it in the title for the one that is longer. */}
        {s?.detail && (state === "failed" || state === "skipped") && (
          <span
            title={s.detail}
            className={cn("line-clamp-2 pl-5 text-caption", state === "failed" ? "text-destructive" : "text-muted-foreground")}
          >
            {s.detail}
          </span>
        )}
      </span>
      <span />
      <span className="text-right text-sm2 tabular-nums whitespace-nowrap">
        <span className={cn(over && "text-warning")}>{s && !s.skipped ? msLabel(s.ms) : "—"}</span>
        {ceiling != null && <span className="ml-1 text-caption text-muted-foreground">/ {msLabel(ceiling)}</span>}
      </span>
    </li>
  );
}

function StageRow({ stage, slos, openAll }: { stage: StageNode; slos: Map<string, SloStatus>; openAll: boolean }) {
  // Passed stages collapse: a green stage is answered, and eleven of them expanded is a page the
  // running one is lost in. The running stage and every failed one start open.
  const [open, setOpen] = useState(openAll || stage.state === "running" || stage.state === "failed");
  const [num, ...rest] = stage.name.split(" · ");
  const shown = openAll || open;
  return (
    <li className="border-b border-border last:border-b-0">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={shown}
        className={cn(ROW, "w-full py-2 text-left hover:bg-muted focus-visible:bg-muted focus-visible:outline-none")}
      >
        <span className="text-right text-caption tabular-nums text-muted-foreground">{num}</span>
        <span className="flex min-w-0 items-baseline gap-2">
          <Glyph state={stage.state} className="translate-y-0.5" />
          <span className={cn("truncate text-sm2 font-medium", stage.state === "pending" && "text-muted-foreground")}>
            {rest.join(" · ") || stage.name}
          </span>
        </span>
        <span className="text-right text-caption tabular-nums text-muted-foreground">
          {stage.total === 0 ? "—" : `${stage.ok}/${stage.total} ok`}
        </span>
        <span className="text-right text-sm2 tabular-nums text-muted-foreground">
          {stage.state === "running" && stage.startedTs ? <Elapsed since={stage.startedTs} /> : stage.ms > 0 ? msLabel(stage.ms) : "—"}
        </span>
      </button>
      {shown && stage.steps.length > 0 && <ul className="border-t border-border bg-muted/20">{stage.steps.map((n) => <StepRow key={n.id} node={n} slo={slos.get(n.id)} />)}</ul>}
    </li>
  );
}

export function RunTree({
  run,
  steps,
  journey,
  slos,
  expanded = false,
}: {
  run: SloRun | SloRunDetail;
  steps: SloStep[];
  journey: { name: string; ids: string[] }[];
  slos: SloStatus[];
  /** The run page shows a finished run whole; the overview panel collapses what has passed. */
  expanded?: boolean;
}) {
  const tree = treeOf(journey, steps);
  const { done, total } = progressOf(tree);
  const stageNo = tree.findIndex((s) => s.name === run.stage);
  const bySlo = new Map(slos.map((s) => [s.id, s]));
  return (
    <div className="flex flex-col">
      {/* The bar is the run's own progress, not a percentage anyone quotes — no label on it, the
          header's "31 of 61 steps" is the number. */}
      <div className="h-0.5 w-full bg-muted">
        <div
          className={cn("h-full transition-[width] duration-500 motion-reduce:transition-none", run.state === "failed" ? "bg-destructive" : "bg-primary")}
          style={{ width: `${total === 0 ? 0 : Math.round((done / total) * 100)}%` }}
        />
      </div>
      <div className="flex flex-wrap items-baseline gap-x-4 gap-y-1 border-b border-border px-4 py-2.5">
        <Pill tone="info">{run.suite}</Pill>
        <span className="font-mono text-caption text-muted-foreground">{run.run_id}</span>
        <span className="text-caption text-muted-foreground">
          stage {stageNo < 0 ? tree.length : stageNo + 1} of {tree.length}
        </span>
        <span className="text-caption text-muted-foreground">
          {done} of {total} steps
        </span>
        <div className="flex-1" />
        <span className="text-sm2 font-medium tabular-nums">
          {run.state === "running" ? <Elapsed since={run.started} /> : msLabel(run.duration_ms)}
        </span>
        <Pill tone={runTone(run.state)}>{run.state}</Pill>
      </div>
      <ul>
        {tree.map((s) => (
          <StageRow key={s.name} stage={s} slos={bySlo} openAll={expanded} />
        ))}
      </ul>
    </div>
  );
}
