import type { JSX } from "solid-js";

/**
 * The pieces every result card is made of. No new colours and no new shapes: a card is the same
 * rail-and-tokens the transcript already uses, so a rendered answer reads as part of the thread
 * rather than as a panel dropped into it.
 */
export function Card(props: { children: JSX.Element }) {
  return <div class="my-1 flex flex-col gap-2">{props.children}</div>;
}

const STATE: Record<string, string> = {
  ready: "bg-success", running: "bg-success", done: "bg-success", true: "bg-success",
  stopped: "bg-subtle", queued: "bg-subtle", "": "bg-subtle",
  creating: "bg-warning", starting: "bg-warning", stopping: "bg-warning", restoring: "bg-warning",
  failed: "bg-danger", error: "bg-danger", deleted: "bg-danger",
};
export function Dot(props: { state: string; title?: string }) {
  return <span class={`size-1.5 shrink-0 rounded-full ${STATE[props.state.toLowerCase()] ?? (props.state.startsWith("exited") ? "bg-subtle" : "bg-subtle")}`} title={props.title ?? props.state} />;
}

export function Field(props: { label: string; children: JSX.Element }) {
  return (
    <div class="flex min-w-0 items-baseline gap-3">
      <span class="w-24 shrink-0 truncate text-subtle">{props.label}</span>
      <span class="min-w-0 truncate text-fg">{props.children}</span>
    </div>
  );
}

export function Pill(props: { children: JSX.Element; tone?: "plain" | "warn" }) {
  return <span class={`rounded-[2px] px-1 ${props.tone === "warn" ? "bg-warning-wash text-fg" : "bg-fg/10 text-muted"}`}>{props.children}</span>;
}

/** One proportion, drawn: the bar is the only chart here and it is two divs. */
export function Bar(props: { used: number; limit: number }) {
  const pct = () => (props.limit > 0 ? Math.min(100, Math.round((props.used / props.limit) * 100)) : 0);
  return (
    <span class="inline-flex h-1 w-20 shrink-0 overflow-hidden rounded-[1px] bg-fg/10" title={`${pct()}%`}>
      <span class={`h-full ${pct() >= 90 ? "bg-danger" : pct() >= 70 ? "bg-warning" : "bg-success"}`} style={{ width: `${pct()}%` }} />
    </span>
  );
}
