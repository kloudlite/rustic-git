import { Show, type JSX } from "solid-js";
import { cx } from "./cx";

/** A section title in a panel. `meta` sits at the right, quiet; `actions` are the
    section's own controls, so a button never floats alone between sections. */
export function Heading(props: { children: JSX.Element; meta?: JSX.Element; actions?: JSX.Element; class?: string }) {
  return (
    <div class={cx("flex h-7 items-center gap-2 px-3 pt-2 text-xs font-semibold tracking-[0.06em] uppercase text-muted", props.class)}>
      {props.children}
      <span class="flex-1" />
      <Show when={props.meta}>
        <span class="text-xs font-normal tracking-normal normal-case text-subtle">{props.meta}</span>
      </Show>
      <Show when={props.actions}>
        <span class="-mr-1.5 -my-1 flex items-center gap-0.5 normal-case tracking-normal">{props.actions}</span>
      </Show>
    </div>
  );
}

/** A label and its value on one line. The value truncates; the label never does. */
export function Field(props: { label: string; mono?: boolean; children: JSX.Element }) {
  return (
    <div class="flex items-baseline gap-3 px-3 py-[3px] text-sm">
      <span class="shrink-0 text-muted">{props.label}</span>
      <span class={cx("ml-auto min-w-0 truncate text-right", props.mono && "font-mono text-xs")}>{props.children}</span>
    </div>
  );
}

/** A key in a hint line. A terminal has no key caps: the key is simply the
    brighter half of the pair, and the verb after it is the quieter half. */
export function Kbd(props: { children: JSX.Element }) {
  return <kbd class="font-mono text-xs font-medium text-fg">{props.children}</kbd>;
}

/** Nothing to show, said in a sentence rather than a blank. */
export function Empty(props: { children: JSX.Element; tone?: "subtle" | "danger" }) {
  return (
    <p class={cx("m-0 px-3 py-2 text-sm leading-relaxed wrap-words", props.tone === "danger" ? "text-danger font-mono text-xs" : "text-subtle")}>
      {props.children}
    </p>
  );
}

/** One line in any tree or list: a glyph gutter, a label that truncates, meta at the end. */
export function Row(props: {
  selected?: boolean;
  onClick?: () => void;
  title?: string;
  class?: string;
  state?: string;
  status?: string;
  style?: JSX.CSSProperties;
  children: JSX.Element;
}) {
  return (
    <div
      role="treeitem"
      aria-selected={props.selected ?? false}
      data-state={props.state}
      data-status={props.status}
      onClick={() => props.onClick?.()}
      title={props.title}
      style={props.style}
      class={cx(
        "flex h-6.5 items-center gap-0.5 px-3 whitespace-nowrap transition-colors duration-100 ease-out-quick",
        "hover:bg-hover aria-selected:bg-selected",
        props.class,
      )}
    >
      {props.children}
    </div>
  );
}

/** The fixed 18px column every glyph in a row sits in, so glyphs stack in a line. */
export function Gutter(props: { children?: JSX.Element; class?: string }) {
  return <span class={cx("inline-flex h-4.5 w-4.5 shrink-0 items-center justify-center", props.class)}>{props.children}</span>;
}
