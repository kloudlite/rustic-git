import { For, Show, createEffect, createMemo, createSignal, onMount } from "solid-js";
import * as live from "../live";
import { noteModelNames, pickerRows, type PickerRow } from "../rows";

/** Rows visible before the list scrolls; the keyboard row is always scrolled back into view. */
const VISIBLE = 12;
/** The gutter every row shares: cursor `❯`, picked `●`, or blank. One column, so nothing shifts. */
const GUTTER = "w-[2ch] shrink-0 select-none";

/**
 * `/model`, opencode's `/models` shape: ONE grouped list, not two columns — the owner was "confused
 * with the way highlights are happening" when a highlight could be in either column. A provider is
 * a dim, unselectable group header; its models are indented beneath it. ↑↓ move across the whole
 * list and skip headers, Enter picks, Esc closes. No ←→.
 *
 * Only providers that are WIRED and actually have models are listed (owner: "why are you showing so
 * many non configured. show only configured"). `allProviders()` still carries the rest for Settings,
 * which is where a key gets added.
 *
 * It takes the COMPOSER'S PLACE (spec §1.3) like the permission prompt, and the FILTER INPUT holds
 * the focus — the card used to take it, so typing narrowed nothing.
 */
export function ModelDialog(props: { session: string; model?: string; effort?: string; onClose: () => void }) {
  const [providers, setProviders] = createSignal<live.ProviderRow[]>(live.providers());
  onMount(() =>
    void live.models().then((r) => {
      setProviders(r);
      noteModelNames(r.flatMap((p) => p.models.map((m) => ({ id: `${p.id}/${m.id}`, name: m.name }))));
    }),
  );
  const [filter, setFilter] = createSignal("");
  /** Once a model that takes an effort is picked, the same list becomes its effort sub-list. */
  const [effortFor, setEffortFor] = createSignal<string | undefined>();
  const [cursor, setCursor] = createSignal(0);

  const usable = createMemo(() => providers().filter((p) => p.wired && p.models.length));
  /** The flat list the keyboard walks; the grouping rules are `pickerRows`, and tested there. */
  const rows = createMemo<PickerRow[]>(() =>
    effortFor() ? live.EFFORT.map((v) => ({ kind: "model" as const, id: v, name: v })) : pickerRows(providers(), filter()),
  );
  const selectable = createMemo(() => rows().flatMap((r, i) => (r.kind === "model" ? [i] : [])));

  /** The cursor only ever rests on a model: a header is a label, not a choice. */
  const step = (by: 1 | -1) => {
    const picks = selectable();
    if (!picks.length) return;
    const at = picks.indexOf(cursor());
    setCursor(picks[(((at < 0 ? 0 : at) + by) + picks.length) % picks.length]);
  };
  const take = () => {
    const r = rows()[cursor()];
    if (r?.kind !== "model") return;
    if (effortFor()) {
      void live.setModel(props.session, { effort: r.id });
      return props.onClose();
    }
    void live.setModel(props.session, { model: r.id });
    // A model that takes an effort asks for it next, in the same list and the same look.
    const takesEffort = usable().some((p) => p.models.some((m) => `${p.id}/${m.id}` === r.id && m.effort));
    if (!takesEffort) return props.onClose();
    setEffortFor(r.id);
    setCursor(0);
  };

  /** Keep the cursor on a model and in view: the filter can shrink the list under it. */
  let listEl: HTMLDivElement | undefined;
  createEffect(() => {
    const picks = selectable();
    if (!picks.length) return;
    if (!picks.includes(cursor())) return void setCursor(picks[0]);
    listEl?.querySelectorAll<HTMLElement>("[data-row]")[cursor()]?.scrollIntoView({ block: "nearest" });
  });

  let input: HTMLInputElement | undefined;
  onMount(() => input?.focus());
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") return (e.preventDefault(), props.onClose());
    if (e.key === "ArrowDown") return (e.preventDefault(), step(1));
    if (e.key === "ArrowUp") return (e.preventDefault(), step(-1));
    if (e.key === "Enter") return (e.preventDefault(), take());
  };

  return (
    <div data-component="model-dialog" class="flex flex-col font-mono">
      <input
        ref={input}
        class="w-full bg-transparent px-[2ch] text-fg outline-none placeholder:text-subtle"
        placeholder={effortFor() ? "effort…" : "filter models…"}
        value={filter()}
        onKeyDown={onKey}
        onInput={(e) => (setFilter(e.currentTarget.value), setCursor(0))}
      />
      <div ref={listEl} class="flex flex-col overflow-y-auto pt-1" style={{ "max-height": `calc(${VISIBLE} * 1.5em)` }}>
        <Show when={rows().length} fallback={<div class="px-[2ch] text-subtle">no provider configured — add a key in Settings</div>}>
          <For each={rows()}>
            {(r, i) => (
              <Show
                when={r.kind === "model"}
                fallback={
                  // A group header: flush with the gutter, dim, and never selectable.
                  <div data-row data-kind="header" class="flex items-baseline pt-1 text-subtle">{(r as { label: string }).label}</div>
                }
              >
                {/* The cursor row is a full-width bar at NORMAL weight — never bold as well. */}
                <button
                  data-row
                  data-kind="model"
                  class="flex w-full items-baseline text-left"
                  classList={{ "bg-hover": cursor() === i() }}
                  onMouseEnter={() => setCursor(i())}
                  onClick={() => (setCursor(i()), take())}
                >
                  <span class={GUTTER} classList={{ "text-accent": cursor() === i(), "text-subtle": cursor() !== i() }}>
                    {cursor() === i() ? "❯" : (r as { id: string }).id === props.model ? "●" : " "}
                  </span>
                  {/* Models sit 2ch in from their header; every row shares this one column. */}
                  <span class="pl-[2ch] text-fg">{(r as { name: string }).name}</span>
                </button>
              </Show>
            )}
          </For>
        </Show>
      </div>
      <div class="px-[2ch] pt-1 text-subtle">↑↓ move · ↩ pick · esc close</div>
    </div>
  );
}
