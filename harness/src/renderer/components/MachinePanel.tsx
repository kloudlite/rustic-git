import { For, Show, createSignal } from "solid-js";
import { Icon } from "../ui/Icon";
import { Menu, MenuItem, MenuSep } from "../ui/Menu";
import { Button } from "../ui/Button";
import { Heading, Row, Gutter, Empty } from "../ui/parts";
import { AGENT, ROLE_SHORT } from "./status";
import { EnvironmentDock } from "./EnvironmentDock";
import type { AgentState, Environment, Machine, Thread } from "../model";

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
  team: string;
  sessions: { id: string; name: string }[];   // the bench's sessions; the first is "bench"
  busy: (id: string) => boolean;
  onNewSession: () => void;
  onArchiveSession: (id: string) => void;
  onRestoreSession: (id: string) => void;
  onDeleteSession: (id: string) => void;
  onArchiveIdle: () => void;
  idle: number;                                   // sessions untouched past the archive window
  archived: { id: string; name: string }[];       // asleep: no pi, one click from coming back
  sides: Thread[];                 // read-only answers forked from a session
  onCloseSide: (id: string) => void;
  selected: string;
  onSelect: (id: string) => void;
  onOpenEnv: () => void;
  onConnect: (id: string) => void;
}) {
  // Collapsing is per workspace and lives here: it is a view preference, not
  // something the machine or the AI has an opinion about.
  const [collapsed, setCollapsed] = createSignal(new Set<string>());
  const [archivedOpen, setArchivedOpen] = createSignal(false);
  const [menu, setMenu] = createSignal(false);
  const toggle = (id: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev);
      next.has(id) ? next.delete(id) : next.add(id);
      return next;
    });

  return (
    <nav class="flex min-h-0 flex-col border-r border-line bg-panel">
      {/* Sessions are the first pane — one per thing being worked on — with a
          plus on the header to start another; the team is in the title bar. */}
      <div class="flex-1 overflow-x-hidden overflow-y-auto pb-3">
      <Heading
        class="border-t-0 pt-1"
        actions={
          <>
            <Button variant="icon" size="sm" icon="plus" title="New session (/new)" onClick={props.onNewSession} />
            <div class="relative" data-menu-root>
              <Button variant="icon" size="sm" icon="more" title="Session options" onPointerDown={(e) => (e.stopPropagation(), setMenu((v) => !v))} />
              <Menu open={menu()} onClose={() => setMenu(false)} align="right">
                <MenuItem icon="archive" hint={props.idle ? `${props.idle} idle` : "none idle"} onSelect={() => (props.onArchiveIdle(), setMenu(false))}>Archive idle sessions</MenuItem>
                <MenuItem icon="archive" onSelect={() => (props.onArchiveSession(props.selected), setMenu(false))}>Archive this session</MenuItem>
                <MenuSep />
                <MenuItem icon="x" onSelect={() => (props.onDeleteSession(props.selected), setMenu(false))}>Delete this session</MenuItem>
              </Menu>
            </div>
          </>
        }
      >
        Sessions
      </Heading>
      {/* The sessions, one per thing being worked on, under the bench they
          belong to; a session's btw answers hang under it in turn (a lock for
          what they are). A row's dot says its pi is busy; the cross on hover
          ends one — never the last. */}
      <For each={props.sessions}>
        {(x, i) => {
          const sid = () => (i() === 0 ? props.machine.id : x.id);
          return (
            <>
              <Row
                class="group h-5.5 pl-3"
                selected={props.selected === sid()}
                onClick={() => props.onSelect(sid())}
                title={x.id}
              >
                <Icon name="thread" size={12} class="mr-1.5 shrink-0 text-muted" />
                <span class="min-w-0 flex-1 truncate pr-2">{x.name}</span>
                <Show when={props.busy(x.id)}><span class="mr-1 size-1.5 shrink-0 rounded-full bg-success" title="working" /></Show>
                {/* Every session can be put away or deleted — the last one is
                    replaced by a fresh one rather than leaving nothing. */}
                <button
                  class="hidden size-4.5 shrink-0 items-center justify-center rounded-[2px] text-subtle group-hover:inline-flex hover:bg-toolbar-hover hover:text-fg"
                  title="Archive (its pi stops; it comes back on click)"
                  onClick={(e) => (e.stopPropagation(), props.onArchiveSession(x.id))}
                >
                  <Icon name="archive" size={12} />
                </button>
                <button
                  class="hidden size-4.5 shrink-0 items-center justify-center rounded-[2px] text-subtle group-hover:inline-flex hover:bg-toolbar-hover hover:text-danger"
                  title="Delete (stops its work; its workspace messages are discarded)"
                  onClick={(e) => (e.stopPropagation(), props.onDeleteSession(x.id))}
                >
                  <Icon name="x" size={12} />
                </button>
              </Row>
              <For each={props.sides.filter((t) => (t.session ?? "bench") === x.id)}>
                {(t) => (
                  <Row
                    class="group relative h-5.5 pl-[34px] before:absolute before:top-0 before:bottom-0 before:left-[21px] before:w-px before:bg-guide"
                    selected={props.selected === t.id}
                    onClick={() => props.onSelect(t.id)}
                    title="Read-only answer forked from this session"
                  >
                    <Icon name="lock" size={12} class="mr-1.5 shrink-0 text-subtle" />
                    <span class="min-w-0 flex-1 truncate pr-2 text-muted">{t.name}</span>
                    <button
                      class="hidden size-4.5 shrink-0 items-center justify-center rounded-[2px] text-subtle group-hover:inline-flex hover:bg-toolbar-hover hover:text-fg"
                      title="Remove"
                      onClick={(e) => (e.stopPropagation(), props.onCloseSide(t.id))}
                    >
                      <Icon name="x" size={12} />
                    </button>
                  </Row>
                )}
              </For>
            </>
          );
        }}
      </For>
        {/* Archived sessions fold away under the live ones: a click wakes one
            (its pi resumes its own file), the cross deletes it for good. */}
        <Show when={props.archived.length}>
          <button class="flex h-5.5 w-full items-center gap-1 px-3 text-left text-xs text-subtle hover:text-fg" onClick={() => setArchivedOpen((v) => !v)}>
            <Icon name={archivedOpen() ? "chevronDown" : "chevronRight"} size={16} />
            <span>Archived</span>
            <span class="ml-auto font-mono tabular-nums">{props.archived.length}</span>
          </button>
          <Show when={archivedOpen()}>
            <For each={props.archived}>
              {(x) => (
                <Row class="group h-5.5 pl-[34px]" onClick={() => props.onRestoreSession(x.id)} title="Restore this session">
                  <Icon name="thread" size={12} class="mr-1.5 shrink-0 text-subtle" />
                  <span class="min-w-0 flex-1 truncate pr-2 text-muted">{x.name}</span>
                  <button
                    class="hidden size-4.5 shrink-0 items-center justify-center rounded-[2px] text-subtle group-hover:inline-flex hover:bg-toolbar-hover hover:text-danger"
                    title="Delete this session"
                    onClick={(e) => (e.stopPropagation(), props.onDeleteSession(x.id))}
                  >
                    <Icon name="x" size={12} />
                  </button>
                </Row>
              )}
            </For>
          </Show>
        </Show>

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
                  class="h-5.5"
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
                      <Icon name={open() ? "chevronDown" : "chevronRight"} size={16} />
                    </Show>
                  </button>
                  <Icon name="workspace" size={16} class={`shrink-0 ${ws.state === "stopped" ? "text-subtle" : "text-accent"}`} />
                  <span class={`min-w-0 flex-1 truncate px-1 ${ws.state === "stopped" ? "text-muted" : "text-fg"}`}>
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
                  {/* The guide is drawn by each row, not once behind them: a row
                      that is selected then paints its own guide over its own
                      fill, the way the editor's tree does. */}
                  <div>
                    <For each={ws.ephemerals}>
                      {(e) => (
                        <Row
                          class="relative pl-[34px] before:absolute before:top-0 before:bottom-0 before:left-[21px] before:w-px before:bg-guide"
                          selected={props.selected === e.id}
                          onClick={() => props.onSelect(e.id)}
                          title={`${e.agent} · ${AGENT[e.state].label} · ${e.task}`}
                        >
                          <Icon name="ephemeral" size={16} class="mr-1.5 shrink-0 text-subtle" />
                          <span class="min-w-0 flex-1 truncate pr-2 text-muted">{e.task}</span>
                          <span class="shrink-0 text-xs text-subtle">{ROLE_SHORT[e.agent] ?? e.agent}</span>
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

        {/* The team's repositories, under the workspaces cut from them. A row
            says how many working copies it has here; the plus on hover opens
            a fresh one — the way a workspace begins. */}
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
