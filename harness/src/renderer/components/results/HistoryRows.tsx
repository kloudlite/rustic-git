import { For, Show } from "solid-js";
import { Card, Dot, Pill } from "./parts";

/** A volume's snapshots, newest first: the id, when it was cut, and what it was called. */
export function HistoryRows(props: { data: Record<string, any>[] }) {
  return (
    <Card>
      <Show when={props.data.length} fallback={<span class="text-subtle">no snapshots</span>}>
        <For each={props.data}>
          {(r) => (
            <div class="flex min-w-0 items-baseline gap-3">
              <Dot state={String(r.state ?? (r.ready === false ? "creating" : "ready"))} />
              <span class="shrink-0 text-fg">{r.id ?? r.snapshot ?? r.name}</span>
              <span class="shrink-0 text-subtle">{r.at ?? r.created ?? r.timestamp ?? ""}</span>
              <span class="min-w-0 flex-1 truncate text-muted" title={String(r.message ?? "")}>{r.message ?? ""}</span>
              <Show when={r.transient}><Pill>sync point</Pill></Show>
            </div>
          )}
        </For>
      </Show>
    </Card>
  );
}
