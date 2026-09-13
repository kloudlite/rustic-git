import { For, Show } from "solid-js";
import { Icon } from "../../ui/Icon";
import { Gutter } from "../../ui/parts";
import type { Change } from "../../model";

const TONE: Record<string, string> = { M: "text-modified", A: "text-created", D: "text-deleted" };

/**
 * What differs from the branch, one line per file the way source control lists
 * them: the name, its directory dimmed behind it, then the counts and the
 * status letter at the edge. The row has the same gutters as a tree row, so a
 * change and the file it is in the tree below sit on one left edge, and the
 * counts are always there — a column that appears on hover shifts the path.
 */
export function ChangeList(props: { changes: Change[]; onOpen: (path: string, status?: string) => void }) {
  return (
    <div class="py-0.5">
      <For each={props.changes}>
        {(c) => {
          const parts = c.path.split("/");
          const file = parts.pop();
          return (
            <button
              class="group flex h-6 w-full min-w-0 items-center pr-2 pl-3 text-left hover:bg-hover"
              onClick={() => props.onOpen(c.path, c.status)}
              title={`${c.path}${c.by ? ` · ${c.by}` : ""}`}
            >
              <Gutter />
              <Gutter><Icon name="file" size={13} class="text-muted" /></Gutter>
              <span class="flex min-w-0 flex-1 items-baseline gap-1.5 px-1">
                <span class="shrink-0 text-sm">{file}</span>
                <Show when={parts.length}>
                  <span class="truncate-start min-w-0 truncate font-mono text-2xs text-subtle">{parts.join("/")}</span>
                </Show>
              </span>
              <span class="shrink-0 pr-2 font-mono text-2xs tabular-nums">
                <span class="text-created">+{c.add}</span> <span class="text-deleted">−{c.del}</span>
              </span>
              <span class={`shrink-0 pr-1 font-mono text-2xs ${TONE[c.status] ?? ""}`}>{c.status}</span>
            </button>
          );
        }}
      </For>
    </div>
  );
}
