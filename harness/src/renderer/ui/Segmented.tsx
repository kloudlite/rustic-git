import { For, Show } from "solid-js";

export type Segment<T extends string> = { value: T; label: string; count?: number };

/**
 * A view switch, drawn as a tab row rather than a pill group: a rule under the
 * row, the active item marked by a rule of its own. It sits inside a panel that
 * already has borders, so another rounded container would be one box too many.
 */
export function Segmented<T extends string>(props: {
  value: T;
  items: Segment<T>[];
  onChange: (v: T) => void;
}) {
  return (
    <div class="flex items-stretch border-b border-line-subtle px-1.5" role="tablist">
      <For each={props.items}>
        {(s) => (
          <button
            role="tab"
            aria-selected={props.value === s.value}
            onClick={() => props.onChange(s.value)}
            class="-mb-px flex h-8 items-center gap-1.5 border-b border-transparent px-2.5 text-sm text-muted
                   transition-colors duration-100 ease-out-quick hover:text-fg
                   aria-selected:border-accent aria-selected:text-fg"
          >
            {s.label}
            <Show when={s.count}>
              {(n) => <span class="font-mono text-xs tabular-nums text-subtle">{n()}</span>}
            </Show>
          </button>
        )}
      </For>
    </div>
  );
}
