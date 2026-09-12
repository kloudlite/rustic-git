import { type JSX } from "solid-js";
import { cx } from "./cx";

type Tone = "neutral" | "accent" | "success" | "warning" | "danger" | "outline";

const TONE: Record<Tone, string> = {
  neutral: "bg-active text-muted",
  accent: "bg-accent text-on-accent",
  success: "bg-success-wash text-success",
  warning: "bg-warning-wash text-warning",
  danger: "bg-danger-wash text-danger",
  outline: "text-subtle ring-1 ring-inset ring-line-subtle",
};

/** A count or a state. Never interactive: if it can be clicked it is a Button. */
export function Badge(props: { tone?: Tone; class?: string; children?: JSX.Element }) {
  return (
    <span
      class={cx(
        "inline-flex h-4 items-center gap-1 rounded-full px-1.5 text-2xs font-medium whitespace-nowrap",
        TONE[props.tone ?? "neutral"],
        props.class,
      )}
    >
      {props.children}
    </span>
  );
}
