import { For, Show } from "solid-js";
import type { Change } from "../../model";

const TONE: Record<string, string> = { M: "text-modified", A: "text-created", D: "text-deleted" };

/**
 * What differs from the branch. Two lines on a grid — status, name, counts and
 * the agent that made it, then the directory underneath, clipped from the left
 * so the tail of the path always reads. Nothing needs a tooltip.
 */
export function ChangeList(props: { changes: Change[]; onOpen: (path: string, status?: string) => void }) {
  return (
    <div class="pt-1.5 pb-0.5">
      <For each={props.changes}>
        {(c) => {
          const parts = c.path.split("/");
          const file = parts.pop();
          return (
            <button
              class="grid w-full grid-cols-[20px_minmax(0,1fr)_auto_auto] items-baseline gap-x-2 px-3 py-1 text-left hover:bg-hover"
              onClick={() => props.onOpen(c.path, c.status)}
            >
              <span class={`text-center font-mono text-2xs leading-[18px] ${TONE[c.status] ?? ""}`}>{c.status}</span>
              <span class="min-w-0 truncate text-sm leading-[18px]">{file}</span>
              <span class="font-mono text-xs leading-[18px] tabular-nums">
                <span class="text-created">+{c.add}</span> <span class="text-deleted">−{c.del}</span>
              </span>
              <Show when={c.by}>{(by) => <span class="font-mono text-2xs leading-[18px] text-subtle">{by()}</span>}</Show>
              <Show when={parts.length}>
                <span class="truncate-start col-start-2 -col-end-1 truncate font-mono text-2xs leading-[14px] text-muted">
                  {parts.join("/")}
                </span>
              </Show>
            </button>
          );
        }}
      </For>
    </div>
  );
}
