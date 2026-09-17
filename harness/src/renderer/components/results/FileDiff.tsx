import { For, Show, createSignal } from "solid-js";
import { Icon } from "../../ui/Icon";
import { highlight, languageOf } from "../../syntax";
import { badge, split, type DiffFile } from "./diff";

/**
 * One file's diff under a sticky header, as opencode's `ToolFileAccordion` draws it
 * (`message-part.tsx:1498`): the directory dimmed, the filename plain, what changed on the right,
 * and a grabber. The header STICKS because a long diff scrolls past the only thing that says which
 * file you are reading.
 *
 * A pure deletion opens closed (`part-default-open.ts:19`) — there is nothing in it to read.
 */
export function FileDiff(props: { file: DiffFile; open?: boolean }) {
  const [open, setOpen] = createSignal(props.open ?? props.file.type !== "delete");
  const parts = () => split(props.file.path);
  const tag = () => badge(props.file);
  const lang = () => languageOf(props.file.path);
  const width = () => Math.max(2, String(Math.max(...props.file.lines.map((l) => l.new ?? l.old ?? 0), 0)).length);
  return (
    <div data-component="tool-file-accordion" class="my-1 min-w-0">
      <button
        data-slot="tool-file-accordion-header"
        class="sticky top-0 z-1 flex w-full items-baseline gap-1 bg-bg py-0.5 text-left"
        onClick={() => setOpen((v) => !v)}
      >
        <Show when={parts().dir}>{(d) => <span class="min-w-0 truncate text-subtle">{d()}</span>}</Show>
        <span class="shrink-0 text-fg">{parts().name}</span>
        <Show when={props.file.from}>{(f) => <span class="shrink-0 text-subtle">← {f()}</span>}</Show>
        <span class="flex-1" />
        <Show
          when={tag()}
          fallback={
            <span data-slot="diff-changes" class="shrink-0">
              <span class="text-created">+{props.file.additions}</span> <span class="text-deleted">−{props.file.deletions}</span>
            </span>
          }
        >
          {(t) => <span data-type={t().type} class="shrink-0 rounded-[2px] bg-fg/10 px-1 text-muted">{t().text}</span>}
        </Show>
        <Icon name={open() ? "chevronDown" : "chevronRight"} size={14} class="shrink-0 text-subtle opacity-40" />
      </button>
      <Show when={open()}>
        <div class="overflow-x-auto rounded-[2px] bg-codeblock py-1 [tab-size:4]">
          <For each={props.file.lines}>
            {(l) => (
              <Show
                when={l.kind !== "sep"}
                /* Between hunks the line info IS the separator (`hunkSeparators: "line-info-basic"`). */
                fallback={<div class="my-0.5 border-t border-line px-2 text-subtle">{l.text}</div>}
              >
                <div class="flex" classList={{ "bg-success-wash": l.kind === "add", "bg-danger-wash": l.kind === "del" }}>
                  <span class="shrink-0 pr-2 pl-2 text-right text-line-number tabular-nums select-none" style={{ width: `${width() + 1}ch` }}>
                    {l.new ?? l.old ?? ""}
                  </span>
                  <span class="w-4 shrink-0 text-center select-none" classList={{ "text-created": l.kind === "add", "text-deleted": l.kind === "del", "text-subtle": l.kind === "context" }}>
                    {l.kind === "add" ? "+" : l.kind === "del" ? "−" : " "}
                  </span>
                  <span class="whitespace-pre" innerHTML={highlight(l.text, lang())} />
                </div>
              </Show>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}
