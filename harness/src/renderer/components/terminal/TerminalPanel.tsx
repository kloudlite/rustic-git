import { For, Show, createSignal } from "solid-js";
import { Icon } from "../../ui/Icon";
import { Button } from "../../ui/Button";
import { Menu, MenuItem } from "../../ui/Menu";
import { TerminalView } from "./TerminalView";
import { scopesOf, type TermTab } from "./tabs";
import type { Machine } from "../../model";

/**
 * The terminal panel: several shells at once, each against a scope. Tabs stay
 * mounted while another is shown, so scrollback and a running command survive
 * switching. The panel owns no tab state — a shell is opened from the place it
 * belongs to, in the inspector — it only shows and closes them.
 */
export function TerminalPanel(props: {
  machine: Machine;
  tabs: TermTab[];
  active: string;
  maximised: boolean;
  onActivate: (id: string) => void;
  onOpen: (scopeId: string) => void;
  onCloseTab: (id: string) => void;
  onToggleMaximise: () => void;
  onClose: () => void;
}) {
  const [picking, setPicking] = createSignal(false);
  // Which tabs have reported a dead shell. Lifted here so the tab strip can
  // mark one while its view is hidden.
  const [exited, setExited] = createSignal<string[]>([]);
  const markExited = (id: string) => setExited((e) => (e.includes(id) ? e : [...e, id]));

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
              <span class="min-w-0 truncate font-mono">{t.label}</span>
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

        <div class="relative flex items-center" data-menu-root>
          <Button variant="icon" size="sm" icon="plus" title="New shell" onClick={() => setPicking((v) => !v)} />
          <Menu open={picking()} onClose={() => setPicking(false)} class="max-h-72 overflow-y-auto">
            <For each={scopesOf(props.machine)}>
              {(s) => (
                <MenuItem
                  mono
                  icon={s.kind === "bench" ? "sparkle" : "terminal"}
                  hint={s.sub}
                  disabled={"disabled" in s && s.disabled}
                  onSelect={() => {
                    props.onOpen(s.id);
                    setPicking(false);
                  }}
                >
                  {s.label}
                </MenuItem>
              )}
            </For>
          </Menu>
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
        <For each={props.tabs}>{(t) => <TerminalView tab={t} visible={t.id === props.active} onExited={markExited} onClose={() => props.onCloseTab(t.id)} />}</For>
      </div>
    </section>
  );
}
