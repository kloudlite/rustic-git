import { Show, type JSX } from "solid-js";
import { Icon } from "./Icon";
import { cx } from "./cx";

type Variant = "default" | "primary" | "ghost" | "danger" | "icon";

const BASE =
  "inline-flex items-center justify-center gap-1.5 rounded-md whitespace-nowrap " +
  "transition-colors duration-100 ease-out-quick disabled:text-disabled";

const VARIANT: Record<Variant, string> = {
  default: "bg-active text-fg hover:bg-line disabled:bg-hover",
  primary: "bg-accent text-on-accent font-medium hover:brightness-110 disabled:bg-active disabled:brightness-100",
  ghost: "bg-transparent text-muted hover:bg-line hover:text-fg",
  danger: "bg-transparent text-danger border border-line hover:bg-danger-wash",
  icon: "bg-transparent text-muted hover:bg-line hover:text-fg",
};

export function Button(props: {
  variant?: Variant;
  size?: "sm" | "md";
  icon?: string;
  iconRight?: string;
  disabled?: boolean;
  title?: string;
  "aria-pressed"?: boolean;
  onClick?: (e: MouseEvent) => void;
  class?: string;
  children?: JSX.Element;
}) {
  const v = () => props.variant ?? "default";
  const sm = () => props.size === "sm";
  return (
    <button
      class={cx(
        BASE,
        VARIANT[v()],
        v() === "icon"
          ? sm() ? "h-5.5 w-5.5" : "h-6 w-6"
          : sm() ? "h-5.5 px-2 text-xs" : "h-6 px-2.5 text-sm",
        props.class,
      )}
      disabled={props.disabled}
      title={props.title}
      aria-pressed={props["aria-pressed"]}
      onClick={(e) => props.onClick?.(e)}
    >
      <Show when={props.icon}>{(n) => <Icon name={n()} size={sm() ? 12 : 13} />}</Show>
      {props.children}
      <Show when={props.iconRight}>{(n) => <Icon name={n()} size={sm() ? 11 : 12} />}</Show>
    </button>
  );
}
