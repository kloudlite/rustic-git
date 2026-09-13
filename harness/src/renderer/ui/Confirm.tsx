import { Show, type JSX } from "solid-js";
import { Button } from "./Button";

/**
 * A question with two answers, over the window: the workbench's own dialog
 * shape — title, the facts, the destructive answer last and named for what it
 * does. Escape and the backdrop are "no".
 */
export function Confirm(props: { open: boolean; title: string; body: JSX.Element; danger: string; onYes: () => void; onNo: () => void }) {
  return (
    <Show when={props.open}>
      <div class="absolute inset-0 z-50 flex items-start justify-center bg-black/30 pt-[18vh]" onMouseDown={props.onNo} onKeyDown={(e) => e.key === "Escape" && props.onNo()}>
        <div class="w-[440px] max-w-[90vw] rounded-md border border-widget-line bg-overlay p-4 shadow-overlay" onMouseDown={(e) => e.stopPropagation()}>
          <div class="text-base font-semibold text-fg-strong">{props.title}</div>
          <div class="mt-2 text-sm leading-[20px] text-muted">{props.body}</div>
          <div class="mt-4 flex justify-end gap-2">
            <Button onClick={props.onNo}>Cancel</Button>
            <Button variant="primary" onClick={props.onYes}>{props.danger}</Button>
          </div>
        </div>
      </div>
    </Show>
  );
}
