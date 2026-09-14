import { For, createSignal } from "solid-js";
import { Icon } from "../ui/Icon";
import { Menu, MenuItem, MenuLabel, MenuSep } from "../ui/Menu";

export type Activity = "workspaces" | "repos" | "registries";

/**
 * The strip at the window's edge, the way every editor has one: each icon is
 * a view of the side panel — the bench and its workspaces, the team's code,
 * the team's images. Clicking the current one puts the panel away. Settings
 * and the person sit at the bottom, where they are always found; the person
 * opens their login's menu — profile, team, sign out.
 */
export function ActivityBar(props: {
  view: Activity;
  panelOpen: boolean;
  onView: (v: Activity) => void;
  onSettings: () => void;
  onProfile: () => void;
  owner: string;
}) {
  const ITEMS: { id: Activity; icon: string; label: string }[] = [
    { id: "workspaces", icon: "workspace", label: "Bench and workspaces (⌘B)" },
    { id: "repos", icon: "repo", label: "Code repositories" },
    { id: "registries", icon: "container", label: "Container registries" },
  ];
  const [open, setOpen] = createSignal(false);
  const [who, setWho] = createSignal(props.owner);
  const [team, setTeam] = createSignal("");
  // Read when the menu opens, not once: a team switch happens while the app stays up.
  const toggle = () => {
    if (!open()) void window.harness.auth.status().then((s) => s.phase === "ready" && (setWho(s.username), setTeam(s.team)));
    setOpen(!open());
  };
  const pick = (fn: () => void) => () => (setOpen(false), fn());
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
      <div class="relative" data-menu-root>
        <button class={btn} title={who()} aria-haspopup="menu" aria-expanded={open()} onClick={toggle}><Icon name="user" size={24} /></button>
        <Menu open={open()} onClose={() => setOpen(false)} placement="above" class="!bottom-0 left-[calc(100%+4px)]">
          <MenuLabel>{team() ? `${who()} · ${team()}` : who()}</MenuLabel>
          <MenuItem icon="user" onSelect={pick(props.onProfile)}>Profile</MenuItem>
          <MenuItem icon="settings" hint="⌘," onSelect={pick(props.onSettings)}>Settings</MenuItem>
          <MenuItem icon="users" onSelect={pick(() => void window.harness.auth.switchTeam())}>Switch team</MenuItem>
          <MenuSep />
          <MenuItem icon="lock" onSelect={pick(() => void window.harness.auth.signOut())}>Sign out</MenuItem>
        </Menu>
      </div>
      <button class={btn} title="Settings (⌘,)" onClick={props.onSettings}><Icon name="settings" size={24} /></button>
    </nav>
  );
}
