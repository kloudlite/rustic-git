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
              class="grid w-full min-w-0 grid-cols-[20px_minmax(0,1fr)_auto] items-baseline gap-x-2 px-3 py-1 text-left hover:bg-hover"
              onClick={() => props.onOpen(c.path, c.status)}
            >
              <span class={`text-center font-mono text-2xs leading-[18px] ${TONE[c.status] ?? ""}`}>{c.status}</span>
              <span class="min-w-0 truncate text-sm leading-[18px]">{file}</span>
              <span class="font-mono text-xs leading-[18px] tabular-nums">
                <span class="text-created">+{c.add}</span> <span class="text-deleted">−{c.del}</span>
              </span>
              <span class="col-start-2 -col-end-1 flex min-w-0 items-baseline gap-1.5 font-mono text-2xs leading-[14px]">
                <Show when={parts.length}>
                  <span class="truncate-start min-w-0 truncate text-muted">{parts.join("/")}</span>
                </Show>
                <Show when={c.by}>
                  {(by) => (
                    <>
                      <span class="text-line">·</span>
                      <span class="shrink-0 text-subtle">{by()}</span>
                    </>
                  )}
                </Show>
              </span>
            </button>
          );
        }}
      </For>
    </div>
  );
}
