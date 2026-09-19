import { Show, type JSX } from "solid-js";
import { Icon } from "./Icon";
import { cx } from "./cx";

type Variant = "default" | "primary" | "ghost" | "danger" | "icon";

const BASE =
  "inline-flex items-center justify-center gap-1.5 rounded-[2px] whitespace-nowrap " +
  "transition-[color,background-color,border-color] duration-[var(--motion)] ease-out-quick disabled:text-disabled";

const VARIANT: Record<Variant, string> = {
  default: "border border-btn2-line bg-btn2 text-fg hover:bg-btn2-hover disabled:opacity-50",
  primary: "border border-btn2-line bg-focus text-on-accent hover:bg-btn-hover disabled:opacity-50",
  ghost: "bg-transparent text-fg hover:bg-toolbar-hover",
  danger: "bg-transparent text-danger border border-line hover:bg-danger-wash",
  icon: "bg-transparent text-fg hover:bg-toolbar-hover",
};

export function Button(props: {
  variant?: Variant;
  size?: "sm" | "md" | "lg";
  icon?: string;
  iconRight?: string;
  disabled?: boolean;
  title?: string;
  "aria-pressed"?: boolean;
  "aria-label"?: string;
  type?: "button" | "submit" | "reset";
  onClick?: (e: MouseEvent) => void;
  onPointerDown?: (e: PointerEvent) => void;
  class?: string;
  children?: JSX.Element;
}) {
  const v = () => props.variant ?? "default";
  const sm = () => props.size === "sm";
  const lg = () => props.size === "lg";
  return (
    <button
      type={props.type ?? "button"}
      class={cx(
        BASE,
        VARIANT[v()],
        v() === "icon"
          ? sm() ? "h-5.5 w-5.5" : "h-6 w-6"
          : lg() ? "h-9 px-4 text-sm" : sm() ? "h-5.5 px-2 text-xs" : "h-6 px-2.5 text-sm",
        props.class,
      )}
      disabled={props.disabled}
      title={props.title}
      aria-pressed={props["aria-pressed"]}
      aria-label={props["aria-label"]}
      onClick={(e) => props.onClick?.(e)}
      onPointerDown={(e) => props.onPointerDown?.(e)}
    >
      <Show when={props.icon}>{(n) => <Icon name={n()} size={16} />}</Show>
      {props.children}
      <Show when={props.iconRight}>{(n) => <Icon name={n()} size={16} />}</Show>
    </button>
  );
}
