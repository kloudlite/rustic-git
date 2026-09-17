import { For, Show } from "solid-js";
import { Card, Dot } from "./parts";

/** What is running in the workspace, as the tool server lists it. */
export function ProcessRows(props: { data: { id: string; state: string; cmd: string }[] }) {
  return (
    <Card>
      <Show when={props.data.length} fallback={<span class="text-subtle">nothing running</span>}>
        <For each={props.data}>
          {(p) => (
            <div class="flex min-w-0 items-baseline gap-3">
              <Dot state={p.state} />
              <span class="shrink-0 text-subtle">{p.id}</span>
              <span class="shrink-0 text-muted">{p.state}</span>
              <span class="min-w-0 flex-1 truncate text-fg" title={p.cmd}>{p.cmd}</span>
            </div>
          )}
        </For>
      </Show>
    </Card>
  );
}
