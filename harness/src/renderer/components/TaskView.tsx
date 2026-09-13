import { Show, createSignal, onCleanup } from "solid-js";
import { Button } from "../ui/Button";
import { Badge } from "../ui/Badge";
import * as live from "../live";

const TONE: Record<live.Task["state"], "success" | "accent" | "neutral" | "danger" | "warning"> = { running: "success", background: "accent", done: "neutral", failed: "danger", cancelled: "warning", lost: "warning" };

/** One task in full: the command, its state and time, and its output as it grows. */
export function TaskView(props: { task: live.Task; onClose: () => void }) {
  const [tick, setTick] = createSignal(Date.now());
  const timer = setInterval(() => setTick(Date.now()), 1000);
  onCleanup(() => clearInterval(timer));
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
      <pre class="m-0 min-h-0 flex-1 overflow-auto px-6 py-4 font-mono text-sm leading-6 whitespace-pre-wrap text-muted select-text">
        {props.task.output || (active() ? "waiting for output…" : "(no output)")}
      </pre>
    </div>
  );
}
