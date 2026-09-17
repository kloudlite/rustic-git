import { For, Show, createEffect, createMemo, createSignal, onMount } from "solid-js";
import * as live from "../live";
import { noteModelNames } from "../rows";

/** Rows visible before the list scrolls; the keyboard row is always scrolled back into view. */
const VISIBLE = 12;

/**
 * `/model`, opencode's dialog on our shapes: it takes the COMPOSER'S PLACE (spec §1.3) exactly as
 * the permission prompt does — same `❯` marker grammar, same one cell size, no floating modal and
 * no border of its own. Providers on the left, that provider's models beside them, and an effort
 * row only when the model has one (spec §1.1: never a knob the model cannot take).
 *
 * The FILTER INPUT holds the focus, not the card: the card used to take it on mount, so every
 * keystroke went to the card's own handler and typing narrowed nothing (owner, on the fleet). The
 * arrow/enter/escape handler therefore lives on the input, where the keys actually land.
 *
 * An unwired provider is listed and dimmed rather than hidden: the person should see the whole
 * shape of what pi supports and why they cannot pick it yet.
 */
export function ModelDialog(props: { session: string; model?: string; effort?: string; onClose: () => void }) {
  const [rows, setRows] = createSignal<live.ProviderRow[]>(live.providers());
  // The footer renders a pick by NAME, so pi's own catalogue names are registered as they arrive.
  // Registered from HERE, not from live.ts: live.ts is loaded by node:test, where an extensionless
  // `./rows` import does not resolve (Vite resolves it, Node does not).
  onMount(() =>
    void live.models().then((r) => {
      setRows(r);
      noteModelNames(r.flatMap((p) => p.models.map((m) => ({ id: `${p.id}/${m.id}`, name: m.name }))));
    }),
  );
  const [filter, setFilter] = createSignal("");
  const [provider, setProvider] = createSignal<string | undefined>(props.model?.split("/")[0]);
  const [col, setCol] = createSignal<"provider" | "model" | "effort">("provider");
  const [pick, setPick] = createSignal(0);

  const match = (s: string) => s.toLowerCase().includes(filter().toLowerCase());
  /** Typing narrows BOTH columns: a provider stays if it matches, or if any of its models does. */
  const shown = createMemo(() => rows().filter((p) => match(p.label) || p.models.some((m) => match(m.name) || match(m.id))));
  const here = () => shown().find((p) => p.id === provider()) ?? shown()[0];
  const modelsHere = createMemo(() => (here()?.models ?? []).filter((m) => !filter() || match(m.name) || match(m.id)));
  const list = createMemo(() => (col() === "provider" ? shown().length : col() === "model" ? modelsHere().length : live.EFFORT.length));
  /** The longest label decides the column's width, so a provider name never ellipsizes. */
  const nameWidth = createMemo(() => Math.max(12, ...rows().map((p) => p.label.length)) + 1);

  const take = () => {
    if (col() === "provider") {
      const p = shown()[pick()];
      if (!p || !p.wired) return; // a provider with no credentials has nothing to pick
      setProvider(p.id);
      return (setCol("model"), setPick(0));
    }
    if (col() === "model") {
      const m = modelsHere()[pick()];
      if (!m) return;
      void live.setModel(props.session, { model: `${here()!.id}/${m.id}` });
      if (m.effort) return (setCol("effort"), setPick(0));
      return props.onClose();
    }
    void live.setModel(props.session, { effort: live.EFFORT[pick()] });
    props.onClose();
  };

  /**
   * The keyboard row scrolled back into view, so a long list never moves the selection offscreen.
   * It also clamps: narrowing the filter shrinks the list under a selection that was past its end.
   */
  let listEl: HTMLDivElement | undefined;
  createEffect(() => {
    const n = list();
    if (pick() >= n) return void setPick(Math.max(0, n - 1));
    listEl?.querySelectorAll<HTMLElement>("[data-row]")[pick()]?.scrollIntoView({ block: "nearest" });
  });

  /** `❯` on the row the keyboard is on, a blank cell on every other — the permission prompt's grid. */
  const Marker = (p: { on: boolean }) => <span class="w-[2ch] shrink-0 select-none" classList={{ "text-accent": p.on, "text-transparent": !p.on }}>❯</span>;
  const Row = (p: { i: number; label: string; note?: string; dim?: boolean; width?: number }) => (
    <button data-row class="flex w-full items-baseline text-left" onMouseEnter={() => setPick(p.i)} onClick={() => (setPick(p.i), take())}>
      <Marker on={pick() === p.i} />
      {/* No truncation: the column is as wide as the longest label, and the note has its own cell. */}
      <span
        class="shrink-0 whitespace-pre"
        style={p.width ? { "min-width": `${p.width}ch` } : undefined}
        classList={{ "font-bold text-fg": pick() === p.i && !p.dim, "text-muted": pick() !== p.i && !p.dim, "text-subtle": p.dim }}
      >
        {p.label}
      </span>
      <Show when={p.note}>{(n) => <span class="w-[16ch] shrink-0 pl-2 text-right text-subtle">{n()}</span>}</Show>
    </button>
  );

  let input: HTMLInputElement | undefined;
  onMount(() => input?.focus());
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") return (e.preventDefault(), props.onClose());
    if (e.key === "ArrowDown") return (e.preventDefault(), setPick((p) => (p + 1) % Math.max(1, list())));
    if (e.key === "ArrowUp") return (e.preventDefault(), setPick((p) => (p - 1 + Math.max(1, list())) % Math.max(1, list())));
    if (e.key === "ArrowLeft" && col() !== "provider") return (e.preventDefault(), setCol("provider"), setPick(0));
    if (e.key === "ArrowRight" && col() === "provider") return (e.preventDefault(), setCol("model"), setPick(0));
    if (e.key === "Enter") return (e.preventDefault(), take());
  };

  return (
    <div data-component="model-dialog" class="flex flex-col font-mono">
      <input
        ref={input}
        class="w-full bg-transparent px-[2ch] text-fg outline-none placeholder:text-subtle"
        placeholder="filter models…"
        value={filter()}
        onKeyDown={onKey}
        onInput={(e) => (setFilter(e.currentTarget.value), setPick(0))}
      />
      <div ref={listEl} class="flex min-w-0 gap-6 overflow-y-auto pt-1" style={{ "max-height": `calc(${VISIBLE} * 1.5em)` }}>
        <div class="flex shrink-0 flex-col">
          <For each={shown()}>
            {(p, i) => (
              <Show when={col() === "provider" || p.id === here()?.id}>
                <Row i={i()} label={p.label} note={p.wired ? undefined : "not configured"} dim={!p.wired} width={nameWidth()} />
              </Show>
            )}
          </For>
        </div>
        <div class="flex min-w-0 flex-1 flex-col">
          <Show when={col() !== "provider"}>
            <Show when={col() === "model"} fallback={<For each={live.EFFORT}>{(v, i) => <Row i={i()} label={`effort ${v}`} />}</For>}>
              <For each={modelsHere()}>{(m, i) => <Row i={i()} label={m.name} note={m.thinking ? "thinking" : undefined} />}</For>
            </Show>
          </Show>
        </div>
      </div>
      <div class="px-[2ch] pt-1 text-subtle">↑↓ move · ←→ column · ↩ pick · esc close</div>
    </div>
  );
}
