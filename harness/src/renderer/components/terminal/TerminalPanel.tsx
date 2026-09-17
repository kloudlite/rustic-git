import { For, Show, createSignal } from "solid-js";
import { Icon } from "../../ui/Icon";
import { Button } from "../../ui/Button";
import { TerminalView } from "./TerminalView";
import { type TermTab } from "./tabs";

/**
 * The terminal panel: the active session tab's shells. Tabs stay mounted while
 * another is shown, so scrollback and a running command survive switching. The
 * panel owns no tab state and picks no scope — a terminal belongs to the tab it
 * was opened from, and "+" opens another one there — it only shows and closes them.
 */
export function TerminalPanel(props: {
  tabs: TermTab[];
  active: string;
  maximised: boolean;
  onActivate: (id: string) => void;
  onOpen: () => void;
  onCloseTab: (id: string) => void;
  /** The shell ended by itself: drop the tab, nothing to kill. */
  onEnded: (id: string) => void;
  onToggleMaximise: () => void;
  onClose: () => void;
}) {
  // Which tabs have reported a dead shell. Lifted here so the tab strip can
  // mark one while its view is hidden.
  const [exited, setExited] = createSignal<string[]>([]);
  const markExited = (id: string) => setExited((e) => (e.includes(id) ? e : [...e, id]));
  /** What the shell calls itself (ttyd's `1` frame), per tab: the strip shows it beside the label. */
  const [titles, setTitles] = createSignal<Record<string, string>>({});

  return (
    <section class="grid h-full min-h-0 grid-rows-[30px_minmax(0,1fr)] bg-bg">
      <header class="flex items-stretch border-b border-line-subtle pl-6" role="tablist">
        <For each={props.tabs}>
          {(t) => (
            <button
              role="tab"
              aria-selected={t.id === props.active}
              onClick={() => props.onActivate(t.id)}
              class="group flex max-w-44 items-center gap-1.5 border-r border-line-subtle px-3 text-xs text-muted
                     hover:text-fg aria-selected:bg-raised aria-selected:text-fg"
              title={t.scope === "bench" ? "the bench" : t.label}
            >
              <Icon name="terminal" size={11} class={t.scope === "bench" ? "text-accent" : "text-subtle"} />
              {/* The shell's own title when it says one, the pod's name until then. */}
              <span class="min-w-0 truncate font-mono">{titles()[t.id] || t.label}</span>
              <Show when={exited().includes(t.id)}>
                <span class="text-subtle" title="the shell exited">
                  •
                </span>
              </Show>
              <span
                class="invisible -mr-1 rounded-sm p-0.5 group-hover:visible hover:bg-line"
                title="Close"
                onClick={(e) => {
                  e.stopPropagation();
                  props.onCloseTab(t.id);
                }}
              >
                <Icon name="x" size={10} />
              </span>
            </button>
          )}
        </For>

        <div class="flex items-center">
          <Button variant="icon" size="sm" icon="plus" title="New shell in this tab" onClick={() => props.onOpen()} />
        </div>

        <span class="flex-1" />
        <div class="flex items-center pr-1.5">
          <Button
            variant="icon"
            size="sm"
            icon={props.maximised ? "minimise" : "maximise"}
            title={props.maximised ? "Restore (esc)" : "Full screen"}
            onClick={props.onToggleMaximise}
          />
          <Button variant="icon" size="sm" icon="x" title="Close panel (⌘J)" onClick={props.onClose} />
        </div>
      </header>

      <div class="relative min-h-0 pl-4">
        <For each={props.tabs}>
          {(t) => (
            <TerminalView
              tab={t}
              visible={t.id === props.active}
              onExited={markExited}
              onEnded={() => props.onEnded(t.id)}
              /* ttyd's title is the COMMAND, which reads as `/nix/profile/cur…` — not a name for a
                 tab (owner, 2026-09-18). The tab is named after its scope; a title only replaces
                 that when it is something a person would have written. */
              onTitle={(id, title) => setTitles((m) => (title.startsWith("/") ? m : { ...m, [id]: title }))}
            />
          )}
        </For>
      </div>
    </section>
  );
}
