import { For, Show, createSignal } from "solid-js";
import { Icon } from "../ui/Icon";
import { Menu, MenuItem, MenuSep } from "../ui/Menu";
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

  return (
    <section class="max-h-[46%] shrink-0 border-t border-line">
      <div class="px-3 pt-3 pb-1 text-2xs font-semibold tracking-[0.08em] uppercase text-subtle">Environment</div>
      {/* The name selects; the chevron beside it is where the environment is
          switched, cloned or created — the same shape as a workspace row. */}
      {/* The row is the target: the whole width selects, and only the chevron
          inside it opens the menu. */}
      <div class="relative mb-1 flex items-center pr-1.5 hover:bg-hover" data-menu-root>
        <button
          class="flex min-w-0 flex-1 items-center px-3 py-1 text-left"
          onClick={props.onOpen}
          title="Open this environment"
        >
          <span class="min-w-0 flex-1 truncate font-mono text-sm text-fg">{props.env.name}</span>
        </button>
        <button
          class="inline-flex h-5 w-5 shrink-0 items-center justify-center rounded-sm text-subtle hover:bg-active hover:text-fg"
          aria-expanded={open()}
          title="Connect to another environment"
          onPointerDown={(e) => {
            // Before any outside-click handler runs, so the menu cannot be
            // opened and closed by the same press.
            e.stopPropagation();
            setOpen((v) => !v);
          }}
        >
          <Icon name="chevronDown" size={12} />
        </button>
        <Menu open={open()} onClose={() => setOpen(false)} align="right" placement="above">
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
          <MenuItem icon="plus" onSelect={() => setOpen(false)}>New environment</MenuItem>
          <MenuItem icon="copy" onSelect={() => setOpen(false)}>Clone {props.env.name}</MenuItem>
          <MenuItem icon="history" onSelect={() => setOpen(false)}>Clone from a snapshot</MenuItem>
        </Menu>
      </div>

      <div class="px-3 pt-1 pb-1 text-2xs font-semibold tracking-[0.08em] uppercase text-subtle">Services</div>

      <div class="max-h-56 overflow-x-hidden overflow-y-auto">
      <For each={props.env.services}>
        {(svc) => (
          <div
            class="flex h-6.5 items-center gap-2 px-3 whitespace-nowrap hover:bg-hover"
            title={`${svc.name} · ${SERVICE[svc.state].label}${svc.note ? ` · ${svc.note}` : ""} · ${svc.image}`}
          >
            <span class={`size-1.5 shrink-0 rounded-full ${SERVICE[svc.state].dot}`} />
            <span class={`min-w-0 shrink-0 truncate font-mono text-sm ${svc.state === "stopped" ? "text-subtle" : "text-fg"}`}>{svc.name}</span>
            <span class="flex-1" />
            <span class="flex min-w-0 items-center gap-2 overflow-hidden">
              <For each={svc.ports}>
                {(p) => <PortTag port={p} env={props.env.name} service={svc.name} />}
              </For>
            </span>
          </div>
        )}
      </For>
      </div>
      <div class="h-2" />
    </section>
  );
}

/**
 * `http:8080` on its own, or `http:8080 → rustic-git:3000` when a workspace has
 * taken that port over. An http port is itself the link.
 */
function PortTag(props: { port: Port; env: string; service: string }) {
  const label = () => `${props.port.protocol}:${props.port.port}`;
  const title = () =>
    props.port.intercept
      ? `${props.service}:${props.port.port} is delivered to ${props.port.intercept.workspace}:${props.port.intercept.port}`
      : `${props.service}:${props.port.port}`;

  return (
    <span class="flex min-w-0 shrink items-center font-mono text-xs tabular-nums" title={title()}>
      <Show
        when={props.port.url}
        fallback={<span class="shrink-0 text-subtle">{label()}</span>}
      >
        {(url) => (
          <button
            class="shrink-0 text-muted underline decoration-transparent underline-offset-2 hover:text-accent hover:decoration-current"
            title={`open ${url()} in a window`}
            onClick={() => window.harness.openPreview(url(), `${props.env} · ${props.service}:${props.port.port}`)}
          >
            {label()}
          </button>
        )}
      </Show>
      <Show when={props.port.intercept}>
        {(ic) => (
          <span class="min-w-0 truncate text-accent">
            <span class="px-1 text-subtle">→</span>
            {ic().workspace}:{ic().port}
          </span>
        )}
      </Show>
    </span>
  );
}
