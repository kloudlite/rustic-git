import { For, createSignal } from "solid-js";
import { Icon } from "../ui/Icon";
import { Button } from "../ui/Button";
import { Menu, MenuItem } from "../ui/Menu";
import { cycleTheme, mode, THEME_ICON } from "../theme";
import type { Machine } from "../model";
import type { Team } from "../../connect/bench";

/**
 * The TEAM, which is what the window is scoped by: a developer has exactly one
 * bench per team, so picking a team is what picks the bench, and there
 * is nothing to choose within one. The list is the person's real teams; a team
 * with no region has no bench to open and is shown, but not choosable.
 */
export function TitleBar(props: {
  machine: Machine;
  teams: Team[];
  team: string;
  onSwitchTeam: (slug: string) => void;
  onSearch: () => void;
}) {
  const [teamOpen, setTeamOpen] = createSignal(false);
  const current = () => props.teams.find((t) => t.slug === props.team);

  return (
    <header class="relative z-40 flex items-center gap-1 border-b border-line bg-chrome pr-2.5 pl-[78px] [-webkit-app-region:drag] [&>*]:[-webkit-app-region:no-drag]">
      {/* Centred on the window, not after the traffic lights: the team is what
          the whole window is scoped by. */}
      <div class="absolute left-1/2 -translate-x-1/2" data-menu-root>
        <button
          class="flex h-5.5 max-w-56 items-center gap-1.5 rounded-[2px] px-2 hover:bg-toolbar-hover data-[open=true]:bg-toolbar-hover"
          data-open={teamOpen()}
          aria-expanded={teamOpen()}
          onPointerDown={(e) => {
            e.stopPropagation();
            setTeamOpen((v) => !v);
          }}
          title={`${props.machine.owner} · one bench per team`}
        >
          <Icon name="users" class="text-accent" />
          <span class="truncate font-medium">{current()?.name || props.team}</span>
          <Icon name="chevronDown" size={11} class="text-subtle" />
        </button>
        <Menu open={teamOpen()} onClose={() => setTeamOpen(false)}>
          <For each={props.teams}>
            {(t) => (
              <MenuItem
                checked={t.slug === props.team}
                disabled={!t.region}
                hint={t.region ? undefined : "no region"}
                onSelect={() => {
                  setTeamOpen(false);
                  if (t.region && t.slug !== props.team) props.onSwitchTeam(t.slug);
                }}
              >
                {t.name || t.slug}
              </MenuItem>
            )}
          </For>
        </Menu>
      </div>

      <span class="flex-1" />

      <Button variant="icon" icon="search" title="Go to… (⌘P)" onClick={props.onSearch} />
      <Button variant="icon" icon={THEME_ICON[mode()]} title={`Theme: ${mode()}`} onClick={cycleTheme} />
      <Button variant="icon" icon="bell" title="Notifications" />
    </header>
  );
}
