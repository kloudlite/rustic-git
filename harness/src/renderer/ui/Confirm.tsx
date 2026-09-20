import { Show, createEffect, createUniqueId, onCleanup, type JSX } from "solid-js";
import { Portal } from "solid-js/web";
import { Button } from "./Button";

/**
 * A question with two answers, over the window: the workbench's own dialog
 * shape — title, the facts, the destructive answer last and named for what it
 * does. Escape and the backdrop are "no".
 */
export function Confirm(props: { open: boolean; title: string; body: JSX.Element; danger: string; disabled?: boolean; onYes: () => void; onNo: () => void }) {
  const titleId = `confirm-${createUniqueId()}`;
  let dialog: HTMLDivElement | undefined;
  let restore: HTMLElement | null = null;
  let wasOpen = false;
  const focusable = () => dialog?.querySelector<HTMLElement>("button:not([disabled]), [href], input:not([disabled]), [tabindex]:not([tabindex='-1'])");
  const restoreFocus = () => {
    const active = document.activeElement;
    if (active === document.body || active === dialog || dialog?.contains(active)) restore?.focus();
  };
  createEffect(() => {
    if (props.open && !wasOpen) {
      restore = document.activeElement as HTMLElement | null;
      wasOpen = true;
      queueMicrotask(() => focusable()?.focus());
    } else if (!props.open && wasOpen) {
      wasOpen = false;
      queueMicrotask(restoreFocus);
    }
  });
  createEffect(() => {
    if (!props.open) return;
    const containFocus = (event: FocusEvent) => {
      if (!dialog?.contains(event.target as Node)) focusable()?.focus();
    };
    document.addEventListener("focusin", containFocus);
    onCleanup(() => document.removeEventListener("focusin", containFocus));
  });
  const keyDown = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      event.preventDefault();
      props.onNo();
      return;
    }
    if (event.key !== "Tab" || !dialog) return;
    const controls = Array.from(dialog.querySelectorAll<HTMLElement>("button:not([disabled]), [href], input:not([disabled]), [tabindex]:not([tabindex='-1'])"));
    if (!controls.length) return;
    const first = controls[0];
    const last = controls[controls.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  };
  return (
    <Show when={props.open}>
      {/* Chat.tsx wraps action rows in `[contain:layout_style]`, and `contain: layout` makes
          that row the containing block for `fixed` AND `absolute` descendants, so without a
          portal the backdrop covers one card instead of the window. `fixed` alone is NOT
          enough — that containment is exactly what `fixed` is supposed to escape, and CSS
          containment overrides it; both the portal and `fixed` are required. */}
      <Portal>
        <div class="fixed inset-0 z-50 flex items-start justify-center bg-black/30 pt-[18vh]" onMouseDown={props.onNo}>
          <div ref={dialog} role="dialog" aria-modal="true" aria-labelledby={titleId} class="w-[440px] max-w-[90vw] rounded-md border border-widget-line bg-overlay p-4 shadow-overlay" onMouseDown={(e) => e.stopPropagation()} onKeyDown={keyDown}>
            <div id={titleId} class="text-base font-semibold text-fg-strong">{props.title}</div>
            <div class="mt-2 text-sm leading-[20px] text-muted">{props.body}</div>
            <div class="mt-4 flex justify-end gap-2">
              <Button onClick={props.onNo}>Cancel</Button>
              <Button variant="primary" disabled={props.disabled} onClick={props.onYes}>{props.danger}</Button>
            </div>
          </div>
        </div>
      </Portal>
    </Show>
  );
}
