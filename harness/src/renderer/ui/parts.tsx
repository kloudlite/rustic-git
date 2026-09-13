import { Show, type JSX } from "solid-js";
import { cx } from "./cx";

/** A section title in a panel. `meta` sits at the right, quiet; `actions` are the
    section's own controls, so a button never floats alone between sections. */
export function Heading(props: { children: JSX.Element; meta?: JSX.Element; actions?: JSX.Element; class?: string }) {
  return (
    <div class={cx("mt-3 mb-1 flex h-5.5 items-center gap-2 border-t border-line px-3 text-xs font-bold uppercase text-fg first:mt-0 first:border-t-0", props.class)}>
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
    <div class="flex h-5.5 items-center gap-3 px-5">
      <span class="shrink-0 text-muted">{props.label}</span>
      <span class={cx("ml-auto min-w-0 truncate text-right", props.mono && "font-mono text-sm")}>{props.children}</span>
    </div>
  );
}

/** A key in a hint line. A terminal has no key caps: the key is simply the
    brighter half of the pair, and the verb after it is the quieter half. */
/** A key, drawn as a keycap: the one shape a person reads as "press this". */
export function Kbd(props: { children: JSX.Element; class?: string }) {
  return (
    <kbd class={cx("inline-flex h-4.5 min-w-4.5 items-center justify-center rounded-[3px] border border-kbd-line border-b-kbd-bottom bg-kbd px-1 font-mono text-xs leading-none text-fg", props.class)}>
      {props.children}
    </kbd>
  );
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
        "flex min-h-5.5 items-center gap-0.5 px-3 whitespace-nowrap",
        "hover:bg-hover aria-selected:bg-selected aria-selected:text-selected-fg aria-selected:outline aria-selected:outline-1 aria-selected:-outline-offset-1 aria-selected:outline-focus-outline",
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
