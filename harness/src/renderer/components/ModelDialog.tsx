import { For, Show, createMemo, createSignal, onMount } from "solid-js";
import * as live from "../live";

/**
 * `/model`, opencode's dialog on our shapes: it takes the COMPOSER'S PLACE (spec §1.3) exactly as
 * the permission prompt does — same `❯` marker grammar, same one cell size, no floating modal and
 * no border of its own. Providers on the left, that provider's models beside them, and an effort
 * row only when the model has one (spec §1.1: never a knob the model cannot take).
 *
 * An unwired provider is listed and dimmed rather than hidden: the person should see the whole
 * shape of what pi supports and why they cannot pick it yet.
 */
export function ModelDialog(props: { session: string; model?: string; effort?: string; onClose: () => void }) {
  const [rows, setRows] = createSignal<live.ProviderRow[]>(live.providers());
  onMount(() => void live.models().then(setRows));
  const [filter, setFilter] = createSignal("");
  const openProvider = () => props.model?.split("/")[0];
  const [provider, setProvider] = createSignal<string | undefined>(openProvider());
  const [col, setCol] = createSignal<"provider" | "model" | "effort">("provider");
  const [pick, setPick] = createSignal(0);

  const match = (s: string) => s.toLowerCase().includes(filter().toLowerCase());
  const shown = createMemo(() => rows().filter((p) => match(p.label) || p.models.some((m) => match(m.name) || match(m.id))));
  const here = () => shown().find((p) => p.id === provider()) ?? shown()[0];
  const modelsHere = createMemo(() => (here()?.models ?? []).filter((m) => !filter() || match(m.name) || match(m.id)));
  /** The effort row exists only for a model that takes one — the picked one, else the open one. */
  const picked = () => (col() === "effort" ? modelsHere().find((m) => `${here()?.id}/${m.id}` === props.model) ?? modelsHere()[0] : undefined);
  const hasEffort = () => picked()?.effort === true;

  const list = () => (col() === "provider" ? shown().length : col() === "model" ? modelsHere().length : live.EFFORT.length);
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

  /** `❯` on the row the keyboard is on, a blank cell on every other — the permission prompt's grid. */
  const Marker = (p: { on: boolean }) => <span class="w-[2ch] shrink-0 select-none" classList={{ "text-accent": p.on, "text-transparent": !p.on }}>❯</span>;
  const Row = (p: { i: number; label: string; note?: string; dim?: boolean }) => (
    <button class="flex w-full items-baseline text-left" onMouseEnter={() => setPick(p.i)} onClick={() => (setPick(p.i), take())}>
      <Marker on={pick() === p.i} />
      <span class="min-w-0 truncate" classList={{ "font-bold text-fg": pick() === p.i && !p.dim, "text-muted": pick() !== p.i && !p.dim, "text-subtle": p.dim }}>{p.label}</span>
      <Show when={p.note}>{(n) => <span class="shrink-0 pl-2 text-subtle">{n()}</span>}</Show>
    </button>
  );

  let card: HTMLDivElement | undefined;
  onMount(() => card?.focus());
  return (
    <div
      ref={card}
      data-component="model-dialog"
      class="pane flex flex-col px-4 py-1 font-mono outline-none"
      tabindex={0}
      onKeyDown={(e) => {
        if (e.key === "Escape") return (e.preventDefault(), props.onClose());
        if (e.key === "ArrowDown") return (e.preventDefault(), setPick((p) => (p + 1) % Math.max(1, list())));
        if (e.key === "ArrowUp") return (e.preventDefault(), setPick((p) => (p - 1 + Math.max(1, list())) % Math.max(1, list())));
        if (e.key === "ArrowLeft" && col() !== "provider") return (e.preventDefault(), setCol("provider"), setPick(0));
        if (e.key === "ArrowRight" && col() === "provider") return (e.preventDefault(), setCol("model"), setPick(0));
        if (e.key === "Enter") return (e.preventDefault(), take());
      }}
    >
      <input
        class="w-full bg-transparent text-fg outline-none placeholder:text-subtle"
        placeholder="filter models…"
        value={filter()}
        onInput={(e) => (setFilter(e.currentTarget.value), setPick(0))}
      />
      <div class="flex min-w-0 gap-6 pt-1">
        <div class="flex w-[28ch] shrink-0 flex-col">
          <For each={shown()}>{(p, i) => <Show when={col() === "provider" || p.id === here()?.id}><Row i={i()} label={p.label} note={p.wired ? undefined : "not configured"} dim={!p.wired} /></Show>}</For>
        </div>
        <div class="flex min-w-0 flex-1 flex-col">
          <Show when={col() !== "provider"}>
            <Show when={col() === "model"} fallback={<For each={live.EFFORT}>{(v, i) => <Row i={i()} label={`effort ${v}`} />}</For>}>
              <For each={modelsHere()}>{(m, i) => <Row i={i()} label={m.name} note={m.thinking ? "thinking" : undefined} />}</For>
            </Show>
          </Show>
        </div>
      </div>
      <div class="pt-1 text-subtle">
        ↑↓ move · ←→ column · ↩ pick · esc close{hasEffort() ? " · this model takes an effort" : ""}
      </div>
    </div>
  );
}
