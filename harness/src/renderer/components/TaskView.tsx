import { Show, createEffect, createSignal, onCleanup } from "solid-js";
import { Button } from "../ui/Button";
import { Badge } from "../ui/Badge";
import * as live from "../live";
import { plainBlock } from "./results/code";
import { OperationPanel, operationTaskRow, useOperationClock, type OperationProjection } from "../operations/index.ts";

const TONE: Record<live.Task["state"], "success" | "accent" | "neutral" | "danger" | "warning"> = { running: "success", background: "accent", done: "neutral", failed: "danger", cancelled: "warning", lost: "warning" };

/** One task in full: the command, its state and time, and its output as it grows. */
export function TaskView(props: { task: live.Task; onClose: () => void }) {
  const [tick, setTick] = createSignal(Date.now());
  const timer = setInterval(() => setTick(Date.now()), 1000);
  onCleanup(() => clearInterval(timer));
  /**
   * A PROCESS keeps its output on the tool server that runs it, not in the tool result the bench
   * saw — which is why this said "(no output)" for a dev server that had printed plenty. Followed
   * from a byte offset while it runs, so the view grows rather than re-reading.
   */
  const [log, setLog] = createSignal("");
  // One offset per STREAM: stderr has its own (`next_err`, 81621d02). Following with stdout's alone
  // re-read every stderr line on every poll, so a build's log grew by its whole history each second.
  let since = 0;
  let sinceErr = 0;
  let following = false;
  createEffect(() => {
    if (props.task.tool !== "Process") return;
    void tick();
    if (following) return;
    following = true;
    void window.harness
      .bench<{ stdout: string; stderr: string; next: number; next_err?: number }>("GET", `/procs/${encodeURIComponent(props.task.id)}/output?since=${since}&sinceErr=${sinceErr}`)
      .then((r) => {
        since = r.next ?? since;
        sinceErr = r.next_err ?? sinceErr;
        const add = [r.stdout, r.stderr].filter(Boolean).join("");
        if (add) setLog((t) => (t + add).slice(-200_000));
      })
      .catch(() => undefined)
      .finally(() => (following = false));
  });
  const secs = () => Math.max(0, Math.round(((props.task.ended ?? tick()) - props.task.started) / 1000));
  const active = () => props.task.state === "running" || props.task.state === "background";
  return (
    <div class="flex min-h-0 min-w-0 flex-col overflow-hidden">
      <header class="flex items-center gap-3 border-b border-line-subtle px-4 py-2">
        <Button variant="ghost" size="sm" icon="chevronLeft" onClick={props.onClose}>Back</Button>
        <span class="min-w-0 flex-1 truncate font-mono text-sm">
          <span class="text-subtle">{props.task.tool} </span>
          <span class="text-fg">{props.task.arg}</span>
        </span>
        <Show when={props.task.n}>{(n) => <span class="font-mono text-xs text-subtle">#{n()}</span>}</Show>
        <Badge tone={TONE[props.task.state]}>{props.task.state}</Badge>
        <span class="font-mono text-xs tabular-nums text-subtle">{secs()}s</span>
        <Show when={active()}>
          <Button variant="ghost" size="sm" icon="x" onClick={() => (props.task.tool === "Process" ? live.stopProc({ id: props.task.id } as live.Proc) : live.cancel(props.task))}>{props.task.tool === "Process" ? "Stop" : "Cancel"}</Button>
        </Show>
      </header>
      {/* A process writes colour; the escapes render as mojibake in a <pre>, so they are stripped. */}
      <pre class="m-0 min-h-0 flex-1 overflow-auto px-6 py-4 font-mono text-sm leading-6 whitespace-pre-wrap text-muted select-text [tab-size:4]">
        {plainBlock(props.task.tool === "Process" ? log() : props.task.output).lines.map((l) => l.text).join("\n") || (active() ? "waiting for output…" : "(no output)")}
      </pre>
    </div>
  );
}

export function OperationTaskView(props: { operation: OperationProjection; onClose: () => void }) {
  const row = () => operationTaskRow(props.operation.view(), props.operation);
  const operationNow = useOperationClock();
  return (
    <div class="flex min-h-0 min-w-0 flex-col overflow-hidden">
      <header class="flex items-center gap-3 border-b border-line-subtle px-4 py-2">
        <Button variant="ghost" size="sm" icon="chevronLeft" onClick={props.onClose}>Back</Button>
        <span class="min-w-0 flex-1 truncate font-mono text-sm">{row().title}</span>
        <Badge tone={row().state === "failed" ? "danger" : row().ended ? "neutral" : "accent"}>{row().state}</Badge>
      </header>
      <div class="min-h-0 flex-1 overflow-auto"><OperationPanel view={props.operation.view()} now={operationNow()} onResync={props.operation.resync} onDecision={props.operation.controls.decide} onAdditionalInput={props.operation.controls.answer} onCancel={props.operation.controls.cancel} loadError={props.operation.error()} onRetry={props.operation.reload} expanded /></div>
    </div>
  );
}
