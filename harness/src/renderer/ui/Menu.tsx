import { Show, onCleanup, type JSX } from "solid-js";
import { Icon } from "./Icon";
import { cx } from "./cx";

/**
 * A menu that hangs off the control that opened it.
 *
 * The menu itself is the grid — a mark column, the label, then the hint — and
 * every row is a subgrid spanning it. That is what keeps the columns lined up
 * down the list AND every row the same width: a row cannot size itself, and the
 * menu sizes to its widest row. (Rows that carried their own grid plus `w-full`
 * made the width circular, and the menu came out narrower than its items.)
 *
 * Closing is the caller's business, but escape and a click outside are handled
 * here because every menu wants both.
 */
export function Menu(props: {
  open: boolean;
  onClose: () => void;
  align?: "left" | "right";
  placement?: "below" | "above";
  class?: string;
  children: JSX.Element;
}) {
  const outside = (e: MouseEvent) => {
    if (!(e.target as HTMLElement).closest("[data-menu-root]")) props.onClose();
  };
  const esc = (e: KeyboardEvent) => e.key === "Escape" && props.onClose();
  document.addEventListener("mousedown", outside);
  document.addEventListener("keydown", esc);
  onCleanup(() => {
    document.removeEventListener("mousedown", outside);
    document.removeEventListener("keydown", esc);
  });

  return (
    <Show when={props.open}>
      <div
        role="menu"
        class={cx(
          "absolute z-30 grid w-max min-w-44 grid-cols-[16px_auto_1fr] gap-x-2 px-2 py-1.5",
          "rounded-md border border-line bg-overlay shadow-overlay",
          props.placement === "above" ? "bottom-[calc(100%+4px)]" : "top-[calc(100%+4px)]",
          props.align === "right" ? "right-0" : "left-0",
          props.class,
        )}
      >
        {props.children}
      </div>
    </Show>
  );
}

/** Every row spans the menu's own columns, so none of them can be a different size. */
// Gaps come from the parent grid: a subgrid inherits them, so the row must not
// set its own or the columns drift apart from the label row.
const ROW = "col-span-full grid grid-cols-subgrid items-center px-2";

export function MenuLabel(props: { children: JSX.Element }) {
  return (
    <div class={cx(ROW, "h-5 text-2xs font-semibold tracking-[0.08em] uppercase text-subtle")}>
      <span />
      <span>{props.children}</span>
    </div>
  );
}

export function MenuItem(props: {
  checked?: boolean;
  icon?: string;
  hint?: string;
  mono?: boolean;
  onSelect?: () => void;
  children: JSX.Element;
}) {
  return (
    <button role="menuitem" onClick={() => props.onSelect?.()} class={cx(ROW, "h-6 rounded-sm text-left text-sm hover:bg-hover")}>
      <span class="justify-self-center text-accent">
        <Show when={props.checked}>
          <Icon name="check" size={12} />
        </Show>
        <Show when={!props.checked && props.icon}>
          <Icon name={props.icon!} size={12} class="text-subtle" />
        </Show>
      </span>
      <span class={cx("max-w-72 truncate", props.mono && "font-mono text-xs")}>{props.children}</span>
      <span class="justify-self-end pl-8 text-xs whitespace-nowrap text-subtle">{props.hint}</span>
    </button>
  );
}

export function MenuSep() {
  return <div class="col-span-full -mx-2 my-1.5 h-px bg-line" />;
}
