import { For, createSignal } from "solid-js";
import { Icon } from "../ui/Icon";
import { Button } from "../ui/Button";
import { Menu, MenuItem } from "../ui/Menu";
import { cycleTheme, mode, THEME_ICON } from "../theme";
import type { Machine, Team } from "../model";

/**
 * The TEAM, which is what the window is scoped by: a developer has exactly one
 * work machine per team, so picking a team is what picks the machine, and there
 * is nothing to choose within one. The environment the machine is connected to
 * is picked where it is shown, in the dock.
 */
export function TitleBar(props: {
  machine: Machine;
  teams: Team[];
  teamId: string;
  onSwitchTeam: (id: string) => void;
}) {
  const [teamOpen, setTeamOpen] = createSignal(false);
  const team = () => props.teams.find((t) => t.id === props.teamId);

  return (
    <header class="relative flex items-center gap-1 border-b border-line bg-chrome pr-2.5 pl-[78px] [-webkit-app-region:drag] [&>*]:[-webkit-app-region:no-drag]">
      {/* Centred on the window, not after the traffic lights: the team is what
          the whole window is scoped by. */}
      <div class="absolute left-1/2 -translate-x-1/2" data-menu-root>
        <button
          class="flex h-5.5 max-w-56 items-center gap-1.5 rounded-md px-2 hover:bg-hover data-[open=true]:bg-hover"
          data-open={teamOpen()}
          aria-expanded={teamOpen()}
          onPointerDown={(e) => {
            e.stopPropagation();
            setTeamOpen((v) => !v);
          }}
          title={`${props.machine.owner} · one machine per team`}
        >
          <Icon name="users" class="text-accent" />
          <span class="truncate font-medium">{team()?.name}</span>
          <Icon name="chevronDown" size={11} class="text-subtle" />
        </button>
        <Menu open={teamOpen()} onClose={() => setTeamOpen(false)}>
          <For each={props.teams}>
            {(t) => (
              <MenuItem
                checked={t.id === props.teamId}
                onSelect={() => {
                  props.onSwitchTeam(t.id);
                  setTeamOpen(false);
                }}
              >
                {t.name}
              </MenuItem>
            )}
          </For>
        </Menu>
      </div>

      <span class="flex-1" />

      <Button variant="icon" icon="search" title="Search (⌘P)" />
      <Button variant="icon" icon={THEME_ICON[mode()]} title={`Theme: ${mode()}`} onClick={cycleTheme} />
      <Button variant="icon" icon="bell" title="Notifications" />
    </header>
  );
}
