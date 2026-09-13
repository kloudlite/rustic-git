import { For } from "solid-js";
import { Icon } from "../ui/Icon";

export type Activity = "workspaces" | "repos" | "registries";

/**
 * The strip at the window's edge, the way every editor has one: each icon is
 * a view of the side panel — the bench and its workspaces, the team's code,
 * the team's images. Clicking the current one puts the panel away. Settings
 * and the person sit at the bottom, where they are always found.
 */
export function ActivityBar(props: {
  view: Activity;
  panelOpen: boolean;
  onView: (v: Activity) => void;
  onSettings: () => void;
  owner: string;
}) {
  const ITEMS: { id: Activity; icon: string; label: string }[] = [
    { id: "workspaces", icon: "workspace", label: "Bench and workspaces (⌘B)" },
    { id: "repos", icon: "repo", label: "Code repositories" },
    { id: "registries", icon: "container", label: "Container registries" },
  ];
  const btn = "relative flex h-12 w-full items-center justify-center text-activity-inactive hover:text-activity-fg aria-pressed:text-activity-fg aria-pressed:before:absolute aria-pressed:before:top-2 aria-pressed:before:bottom-2 aria-pressed:before:left-0 aria-pressed:before:w-0.5 aria-pressed:before:bg-accent";
  return (
    <nav class="flex flex-col items-stretch border-r border-line bg-chrome">
      <For each={ITEMS}>
        {(it) => (
          <button class={btn} aria-pressed={props.panelOpen && props.view === it.id} title={it.label} onClick={() => props.onView(it.id)}>
            <Icon name={it.icon} size={24} />
          </button>
        )}
      </For>
      <span class="flex-1" />
      <button class={btn} title={props.owner} onClick={props.onSettings}><Icon name="user" size={24} /></button>
      <button class={btn} title="Settings (⌘,)" onClick={props.onSettings}><Icon name="settings" size={24} /></button>
    </nav>
  );
}
