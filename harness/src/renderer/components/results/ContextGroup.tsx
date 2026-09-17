import { For, Show, createSignal } from "solid-js";
import { Icon } from "../../ui/Icon";
import { contextCounts, contextSummary, trigger } from "./opencode-map";
import { Spinner, Ticker } from "../Motion";
import type { Message } from "../../model";

type Action = Extract<Message, { role: "action" }>;

/**
 * Consecutive read / glob / grep / list calls, folded into one row — opencode's context group
 * (`message-part.tsx:1043`). The trigger says "Exploring" while any of them runs and "Explored"
 * once they are done, with the counts beside it; the body lists each call using the bare row
 * grammar, with no arrow and no body of its own.
 */
export function ContextGroup(props: { rows: Action[] }) {
  const [open, setOpen] = createSignal(false);
  const pending = () => props.rows.some((r) => r.pending);
  const counts = () => contextSummary(contextCounts(props.rows));
  return (
    <div data-component="context-group" class="flex flex-col">
      <button class="group flex w-full items-baseline gap-2 py-px text-left" onClick={() => setOpen((v) => !v)}>
        <span class={`w-4 shrink-0 ${pending() ? "text-accent" : "text-success"}`}>
          <Show when={!pending()} fallback={<Spinner />}>⏺</Show>
        </span>
        <span class="shrink-0 text-fg" classList={{ shimmer: pending() }}>{pending() ? "Exploring" : "Explored"}</span>
        {/* The counts, in the contract's own order: reads, searches, lists. */}
        {/* The count ticks over as each call lands, the way `Exploring → Explored` settles. */}
        <Ticker class="min-w-0 flex-1 truncate text-muted" value={counts().join(", ")} />
        <Icon name={open() ? "chevronDown" : "chevronRight"} size={14} class="shrink-0 text-subtle opacity-40 group-hover:opacity-100" />
      </button>
      <Show when={open()}>
        <div class="flex flex-col pl-4">
          <For each={props.rows}>
            {(r) => {
              const t = trigger(r.tool, r.args ?? {});
              return (
                <div class="flex min-w-0 items-baseline gap-2">
                  <span class="shrink-0 text-fg">{t.title}</span>
                  <Show when={t.subtitle}>{(s) => <span class="min-w-0 truncate text-muted">{s()}</span>}</Show>
                  <For each={t.args}>{(a) => <span class="shrink-0 text-subtle">{a}</span>}</For>
                </div>
              );
            }}
          </For>
        </div>
      </Show>
    </div>
  );
}
