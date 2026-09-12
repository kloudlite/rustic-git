import { For, Show, createSignal } from "solid-js";
import { Icon } from "../ui/Icon";
import { Heading, Row, Empty } from "../ui/parts";
import { AGENT, ROLE_SHORT } from "./status";
import { EnvironmentDock } from "./EnvironmentDock";
import type { AgentState, Environment, Machine } from "../model";

/**
 * The machine's shape, left to right on screen: this panel is the tree of what
 * exists — the machine, its workspaces, and the ephemeral copy each agent works
 * in. Selecting a node changes the inspector, never the chat.
 */
/** The status column, on the right of every row so the dots read as one line. */
function StateDot(props: { state: AgentState; label: string }) {
  return (
    <span class="ml-2 inline-flex shrink-0 items-center" title={props.label}>
      <span class={`size-1.5 rounded-full ${AGENT[props.state].dot}`} />
    </span>
  );
}

export function MachinePanel(props: {
  machine: Machine;
  environment: Environment;
  environments: Environment[];
  selected: string;
  onSelect: (id: string) => void;
  onOpenEnv: () => void;
  onConnect: (id: string) => void;
}) {
  // Collapsing is per workspace and lives here: it is a view preference, not
  // something the machine or the AI has an opinion about.
  const [collapsed, setCollapsed] = createSignal(new Set<string>());
  const toggle = (id: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev);
      next.has(id) ? next.delete(id) : next.add(id);
      return next;
    });

  return (
    <nav class="flex min-h-0 flex-col border-r border-line bg-panel">
      <div class="flex-1 overflow-x-hidden overflow-y-auto pb-3">
        <Row
          class="h-8.5 border-b border-line-subtle"
          selected={props.selected === props.machine.id}
          onClick={() => props.onSelect(props.machine.id)}
          title={`${props.machine.owner} · one work machine per team`}
        >
          <span class="inline-flex h-4.5 w-4.5 shrink-0 items-center justify-center"><Icon name="sparkle" size={13} class="text-accent" /></span>
          <span class="min-w-0 flex-1 truncate px-1 font-medium">workmachine session</span>
        </Row>

        <Heading>Workspaces</Heading>
        <Show when={props.machine.workspaces.length === 0}>
          <Empty>No workspaces yet. Ask for something and the AI will make what it needs.</Empty>
        </Show>

        <For each={props.machine.workspaces}>
          {(ws) => {
            const open = () => !collapsed().has(ws.id);
            return (
              <div class="mt-2 first:mt-0">
                <Row
                  class="h-7"
                  selected={props.selected === ws.id}
                  state={ws.state}
                  onClick={() => props.onSelect(ws.id)}
                  title={`${ws.repo} @ ${ws.branch}`}
                >
                  <button
                    class="inline-flex h-4.5 w-4.5 shrink-0 items-center justify-center text-subtle hover:text-fg"
                    title={open() ? "Collapse" : "Expand"}
                    onClick={(e) => {
                      e.stopPropagation();
                      toggle(ws.id);
                    }}
                  >
                    <Show when={ws.ephemerals.length}>
                      <Icon name={open() ? "chevronDown" : "chevronRight"} size={12} />
                    </Show>
                  </button>
                  <span class={`min-w-0 flex-1 truncate px-1 font-medium ${ws.state === "stopped" ? "text-muted" : ""}`}>
                    {ws.name}
                  </span>
                  <Show when={!open() && ws.ephemerals.length}>
                    <span class="shrink-0 font-mono text-xs tabular-nums text-subtle">{ws.ephemerals.length}</span>
                  </Show>
                  <StateDot state={ws.state === "running" ? "running" : "idle"} label={ws.state} />
                </Row>

                {/* One unbroken guide down the group, under the chevron's centre,
                    rather than a rail drawn per row that gaps between them. */}
                <Show when={open()}>
                  <div class="relative before:absolute before:top-0 before:bottom-0 before:left-[21px] before:w-px before:bg-line">
                    <For each={ws.ephemerals}>
                      {(e) => (
                        <Row
                          class="pl-[34px]"
                          selected={props.selected === e.id}
                          onClick={() => props.onSelect(e.id)}
                          title={`${e.agent} · ${AGENT[e.state].label} · ${e.task}`}
                        >
                          <span class="min-w-0 flex-1 truncate pr-2 text-sm">{e.task}</span>
                          <span class="shrink-0 text-2xs tracking-[0.05em] uppercase text-subtle">{ROLE_SHORT[e.agent] ?? e.agent}</span>
                          <StateDot state={e.state} label={AGENT[e.state].label} />
                        </Row>
                      )}
                    </For>
                  </div>
                </Show>
              </div>
            );
          }}
        </For>
      </div>

      <EnvironmentDock
        env={props.environment}
        environments={props.environments}
        onOpen={props.onOpenEnv}
        onConnect={props.onConnect}
      />
    </nav>
  );
}
