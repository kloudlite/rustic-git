import { For, Show, createSignal } from "solid-js";
import { Icon } from "../ui/Icon";
import { Menu, MenuItem, MenuSep } from "../ui/Menu";
import { Heading, Row, Gutter } from "../ui/parts";
import { SERVICE } from "./status";
import type { Environment, Port } from "../model";

/**
 * The environment this machine is connected to, docked under the workspaces. It
 * belongs to the team, not to the machine, so it is read-only here: a person
 * reads what is up and opens an http port.
 *
 * A port shows what it speaks — `http:3000`, `tcp:6379` — because that is what
 * decides whether it can be opened. An http port is the link itself; there is no
 * separate affordance to hunt for. It opens in a window of this app rather than
 * the system browser, so the environment's pages stay inside the harness.
 */
export function EnvironmentDock(props: {
  env: Environment;
  environments: Environment[];
  onOpen: () => void;
  onConnect: (id: string) => void;
}) {
  const [open, setOpen] = createSignal(false);
  const up = () => props.env.services.filter((x) => x.state === "running").length;

  return (
    <section class="max-h-[46%] shrink-0 border-t border-line">
      <Heading meta="⌘E">Environment</Heading>
      {/* The environment is a picker, and looks like one: a bordered control with
          the name and a chevron, the shape every developer reads as "switch".
          Opening the environment's page is an item in the same menu (and ⌘E). */}
      <div class="relative mx-3 mt-1 mb-2" data-menu-root>
        <button
          class="flex h-6.5 w-full items-center gap-2 rounded-[2px] border border-input-line bg-input px-2 text-left aria-expanded:border-focus"
          aria-expanded={open()}
          title="Switch environment"
          onPointerDown={(e) => {
            // Before any outside-click handler runs, so the menu cannot be
            // opened and closed by the same press.
            e.stopPropagation();
            setOpen((v) => !v);
          }}
        >
          <span class="size-1.5 shrink-0 rounded-full bg-success" title="connected" />
          <span class="min-w-0 flex-1 truncate text-fg">{props.env.name}</span>
          <span class="text-xs text-subtle">{props.env.owner === "team" ? "team" : "yours"}</span>
          <Icon name="chevronDown" size={16} class="shrink-0 text-fg" />
        </button>
        <Menu open={open()} onClose={() => setOpen(false)} align="left" placement="above">
          <For each={props.environments}>
            {(e) => (
              <MenuItem
                mono
                checked={e.id === props.env.id}
                hint={e.owner === "team" ? "team" : e.from ? `from ${e.from.name}` : "yours"}
                onSelect={() => {
                  props.onConnect(e.id);
                  setOpen(false);
                }}
              >
                {e.name}
              </MenuItem>
            )}
          </For>
          <MenuSep />
          <MenuItem icon="arrowUpRight" onSelect={() => { props.onOpen(); setOpen(false); }}>Open {props.env.name}</MenuItem>
          <MenuSep />
          <MenuItem icon="plus" onSelect={() => setOpen(false)}>New environment</MenuItem>
          <MenuItem icon="copy" onSelect={() => setOpen(false)}>Clone {props.env.name}</MenuItem>
          <MenuItem icon="history" onSelect={() => setOpen(false)}>Clone from a snapshot</MenuItem>
        </Menu>
      </div>

      <Heading meta={`${up()}/${props.env.services.length} up`}>Services</Heading>

      <div class="max-h-56 overflow-x-hidden overflow-y-auto">
      <For each={props.env.services}>
        {(svc) => (
          <Row
            class="h-5.5"
            title={`${svc.name} · ${SERVICE[svc.state].label}${svc.note ? ` · ${svc.note}` : ""} · ${svc.image}`}
          >
            <Gutter><span class={`size-1.5 rounded-full ${SERVICE[svc.state].dot}`} /></Gutter>
            <span class={`min-w-0 shrink-0 truncate px-1 ${svc.state === "stopped" ? "text-subtle" : "text-fg"}`}>{svc.name}</span>
            <span class="flex-1" />
            <span class="flex min-w-0 items-center gap-2 overflow-hidden">
              <For each={svc.ports}>
                {(p) => <PortTag port={p} env={props.env.name} service={svc.name} />}
              </For>
            </span>
          </Row>
        )}
      </For>
      </div>
      <div class="h-2" />
    </section>
  );
}

/**
 * A port as VS Code's Ports view prints it: plain text at the row's end, no
 * chip. An http port is a link (underlined on hover) that opens in a window
 * of this app; a tcp port is inert. An intercepted port says where it lands,
 * in the accent, because that is the fact a person debugging needs to see.
 */
function PortTag(props: { port: Port; env: string; service: string }) {
  const title = () =>
    props.port.intercept
      ? `${props.service}:${props.port.port} is delivered to ${props.port.intercept.workspace}:${props.port.intercept.port}`
      : `${props.service}:${props.port.port} (${props.port.protocol})`;
  return (
    <span class="inline-flex shrink-0 items-center gap-1 font-mono text-sm tabular-nums" title={title()}>
      <Show
        when={props.port.url}
        fallback={<span class="text-subtle">{props.port.port}</span>}
      >
        {(url) => (
          <button
            class="text-muted underline decoration-transparent underline-offset-2 hover:text-accent hover:decoration-current"
            title={`open ${url()} in a window`}
            onClick={() => window.harness.openPreview(url(), `${props.env} · ${props.service}:${props.port.port}`)}
          >
            {props.port.port}
          </button>
        )}
      </Show>
      <Show when={props.port.intercept}>
        {(ic) => <span class="text-accent">→ {ic().workspace}:{ic().port}</span>}
      </Show>
    </span>
  );
}
