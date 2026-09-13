import { For, Show, createEffect, createMemo, createSignal } from "solid-js";
import { Icon } from "../ui/Icon";
import { Kbd } from "../ui/parts";

/**
 * The palette every editor has: ⌘P goes somewhere, ⌘⇧P runs something. One
 * box, two lists — a leading `>` switches to commands, as it does in VS Code,
 * so the key a person's hands already know does what they expect here.
 *
 * A command carries its shortcut on the right, which makes the command list
 * the keymap reference too; there is no separate screen to learn.
 */
export type PaletteItem = {
  id: string;
  label: string;
  detail?: string;   // where it is, or what it does
  kind?: string;     // shown as a faint tag: workspace, thread, file, service
  icon?: string;
  keys?: string;     // a command's shortcut
  run: () => void;
};

export function Palette(props: {
  open: boolean;
  mode: "go" | "commands";
  items: PaletteItem[];
  commands: PaletteItem[];
  onClose: () => void;
}) {
  const [query, setQuery] = createSignal("");
  const [cursor, setCursor] = createSignal(0);
  let input: HTMLInputElement | undefined;

  createEffect(() => {
    if (!props.open) return;
    setQuery(props.mode === "commands" ? ">" : "");
    setCursor(0);
    queueMicrotask(() => input?.focus());
  });

  const commands = () => query().startsWith(">");
  const needle = () => (commands() ? query().slice(1) : query()).trim().toLowerCase();
  const list = createMemo(() => {
    const src = commands() ? props.commands : props.items;
    const n = needle();
    if (!n) return src;
    // Subsequence match, ranked by how early and how tightly it matches — the
    // fuzzy rule people expect from an editor's quick open.
    return src
      .map((it) => ({ it, s: score(`${it.label} ${it.detail ?? ""}`.toLowerCase(), n) }))
      .filter((x) => x.s > 0)
      .sort((a, b) => b.s - a.s)
      .map((x) => x.it);
  });

  const pick = (it: PaletteItem) => {
    props.onClose();
    it.run();
  };

  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") return props.onClose();
    if (e.key === "ArrowDown") return (e.preventDefault(), setCursor((c) => Math.min(c + 1, list().length - 1)));
    if (e.key === "ArrowUp") return (e.preventDefault(), setCursor((c) => Math.max(c - 1, 0)));
    if (e.key === "Enter") {
      const it = list()[cursor()];
      if (it) pick(it);
    }
  };

  return (
    <Show when={props.open}>
      <div class="absolute inset-0 z-40" onMouseDown={props.onClose}>
        <div
          class="mx-auto mt-[6vh] flex w-[600px] max-w-[90vw] flex-col overflow-hidden rounded-md border border-widget-line bg-overlay shadow-overlay"
          onMouseDown={(e) => e.stopPropagation()}
        >
          <div class="flex items-center gap-2 border-b border-widget-line px-3">
            <Icon name={commands() ? "terminal" : "search"} size={13} class="text-subtle" />
            <input
              ref={input}
              class="h-8 flex-1 bg-transparent font-mono text-sm outline-none placeholder:text-subtle"
              placeholder="go to a workspace, thread, file or service  ·  > for commands"
              value={query()}
              onInput={(e) => (setQuery(e.currentTarget.value), setCursor(0))}
              onKeyDown={onKey}
            />
          </div>
          <div class="max-h-[50vh] overflow-y-auto py-1">
            <For each={list()} fallback={<div class="px-3 py-2 text-sm text-subtle">nothing matches</div>}>
              {(it, i) => (
                <button
                  class="flex h-5.5 w-full items-center gap-2 px-3 text-left text-base"
                  classList={{ "bg-selected text-selected-fg": i() === cursor(), "text-fg": i() !== cursor() }}
                  onMouseMove={() => setCursor(i())}
                  onClick={() => pick(it)}
                >
                  <Show when={it.icon}>{(n) => <Icon name={n()} size={13} class="shrink-0 text-subtle" />}</Show>
                  <span class="truncate text-fg">{it.label}</span>
                  <Show when={it.detail}>{(d) => <span class="min-w-0 truncate text-xs text-subtle">{d()}</span>}</Show>
                  <span class="flex-1" />
                  <Show when={it.kind}>{(k) => <span class="text-2xs uppercase text-subtle">{k()}</span>}</Show>
                  <Show when={it.keys}>{(k) => <Kbd>{k()}</Kbd>}</Show>
                </button>
              )}
            </For>
          </div>
        </div>
      </div>
    </Show>
  );
}

/** 0 when `needle` is not a subsequence of `hay`; higher for earlier, tighter matches. */
function score(hay: string, needle: string): number {
  let i = 0;
  let last = -1;
  let s = 0;
  for (const ch of needle) {
    const at = hay.indexOf(ch, last + 1);
    if (at < 0) return 0;
    s += at === last + 1 ? 3 : 1;
    if (i === 0) s += Math.max(0, 20 - at);
    last = at;
    i++;
  }
  return s + Math.max(0, 40 - hay.length) / 10;
}
