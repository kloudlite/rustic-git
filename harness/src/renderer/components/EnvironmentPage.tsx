import { For, Show, type JSX } from "solid-js";
import { Button } from "../ui/Button";
import { Badge } from "../ui/Badge";
import { SERVICE } from "./status";
import * as live from "../live";
import type { Environment, Port, Service, Snapshot } from "../model";

/**
 * An environment in full, as its own tab: what runs, what is intercepted, and what has been kept.
 *
 * This is CHROME, not the terminal pane — it is set in the UI face, on an 8px rhythm, and it reads
 * top to bottom as one document: what this environment is, its services, anything intercepted, its
 * snapshots, its disk. What a person does here goes through the same proposal path a model's call
 * does, so a change from the page and a change from the bench are the same change.
 */
export function EnvironmentPage(props: { env: Environment; snapshots: Snapshot[]; followed?: boolean }) {
  const up = () => props.env.services.filter((s) => s.state === "running").length;
  const mine = () => props.env.owner === "you";
  const intercepted = () =>
    props.env.services.flatMap((s) => s.ports.filter((p) => p.intercept).map((p) => ({ service: s.name, port: p.port, to: p.intercept! })));

  return (
    <div class="overflow-y-auto px-6 py-6 font-ui select-text">
      <div class="flex w-full flex-col gap-6">
        <header class="flex flex-col gap-2">
          <div class="flex items-center gap-3">
            <h1 class="truncate text-md font-medium">{props.env.name}</h1>
            <Badge tone={up() === props.env.services.length && props.env.services.length ? "success" : up() ? "warning" : "neutral"}>
              {up()}/{props.env.services.length} up
            </Badge>
            <Badge tone={mine() ? "accent" : "neutral"}>{mine() ? "yours" : "team"}</Badge>
            <span class="flex-1" />
            <Button icon="plus" size="sm" title="Add a service to this environment">Add service</Button>
            <Button icon="camera" size="sm" variant="ghost" title="Keep this environment as it is, to come back to">Snapshot</Button>
          </div>
          <div class="flex flex-wrap items-baseline gap-x-4 gap-y-1 text-sm text-subtle">
            <span>{props.env.region}</span>
            <Show when={props.env.from}>{(f) => <span>from {f().kind} {f().name}</span>}</Show>
            <Show when={props.followed}><span class="text-accent">followed by this space</span></Show>
            {/* The id is the platform's name for it, not the person's: quiet, and at the end. */}
            <span class="font-mono text-xs text-subtle">{props.env.id}</span>
          </div>
        </header>

        <Section title="Services" count={props.env.services.length}>
          <Show when={props.env.services.length} fallback={<Empty>Nothing runs here yet. Add a service and it becomes reachable by its own name inside this environment.</Empty>}>
            <Table cols={COLS} head={["Service", "Image", "Ports", "Ready", "Intercept", ""]}>
              <For each={props.env.services}>{(svc) => <Row env={props.env} svc={svc} />}</For>
            </Table>
          </Show>
        </Section>

        <Show when={intercepted().length}>
          <Section title="Intercepts" count={intercepted().length}>
            <div class="flex flex-col gap-1.5">
              <For each={intercepted()}>
                {(i) => (
                  <div class="flex items-baseline gap-2 text-sm">
                    <span class="font-mono text-fg">{i.service}:{i.port}</span>
                    <span class="text-subtle">→</span>
                    <span class="min-w-0 truncate font-mono text-accent" title={i.to.workspace}>{live.wsName(i.to.workspace)}</span>
                    <span class="shrink-0 font-mono text-accent">:{i.to.port}</span>
                    <span class="flex-1" />
                    <Button size="sm" variant="ghost" title={`stop sending ${i.service} to ${live.wsName(i.to.workspace)}`}>Clear</Button>
                  </div>
                )}
              </For>
              <p class="text-xs text-subtle">Callers still dial the service by name; the traffic is delivered to the workspace.</p>
            </div>
          </Section>
        </Show>

        <Section title="Snapshots" count={props.snapshots.length}>
          <Show when={props.snapshots.length} fallback={<Empty>No snapshots yet. One keeps this environment as it is now, so you can put it back or start a new one from it.</Empty>}>
            <div class="flex flex-col gap-1.5">
              <For each={props.snapshots}>
                {(s) => (
                  <div class="flex items-baseline gap-3 text-sm">
                    <span class="min-w-0 flex-1 truncate">{s.note || s.name}</span>
                    <span class="shrink-0 text-subtle">{s.at}</span>
                    <span class="shrink-0 truncate text-subtle">{s.by}</span>
                    <Button size="sm" variant="ghost" title={`put this environment back to ${s.note || s.name}`}>Restore</Button>
                  </div>
                )}
              </For>
            </div>
          </Show>
        </Section>

        <Show when={props.env.volume}>
          {(v) => (
            <Section title="Disk" count={0} bare>
              <p class="text-sm text-muted">
                One volume for the whole environment; every mount is a folder inside it.
                <span class="pl-2 font-mono text-xs text-subtle">{v()}</span>
              </p>
            </Section>
          )}
        </Show>
      </div>
    </div>
  );
}

const COLS = "grid-cols-[minmax(140px,1fr)_minmax(0,1.4fr)_auto_minmax(120px,auto)_minmax(0,1fr)_auto]";

/** One service: what it is, what it answers on, whether it is up, and where its traffic goes. */
function Row(props: { env: Environment; svc: Service }) {
  const ic = () => props.svc.ports.find((p) => p.intercept)?.intercept;
  return (
    <Line cols={COLS}>
      <span class="truncate font-mono">{props.svc.name}</span>
      <span class="truncate font-mono text-xs text-subtle" title={props.svc.image}>{props.svc.image}</span>
      {/* Ports never elide: the port is the thing a person came to read. */}
      <span class="flex shrink-0 flex-wrap gap-x-3 gap-y-1 font-mono text-xs">
        <Show when={props.svc.ports.length === 0}><span class="text-subtle">—</span></Show>
        <For each={props.svc.ports}>{(p) => <PortTag port={p} env={props.env.name} service={props.svc.name} />}</For>
      </span>
      <span class="flex items-center gap-2 text-xs">
        <span class={`size-1.5 shrink-0 rounded-full ${SERVICE[props.svc.state].dot}`} />
        <span class={props.svc.state === "running" ? "text-muted" : "text-fg"}>{props.svc.note ?? SERVICE[props.svc.state].label}</span>
      </span>
      <span class="flex min-w-0 items-baseline font-mono text-xs">
        <Show when={ic()} fallback={<span class="text-subtle">—</span>}>
          {(i) => (
            <>
              <span class="shrink-0 text-subtle">→&nbsp;</span>
              <span class="min-w-0 truncate text-accent" title={i().workspace}>{live.wsName(i().workspace)}</span>
              <span class="shrink-0 text-accent">:{i().port}</span>
            </>
          )}
        </Show>
      </span>
      <span class="flex shrink-0 justify-end gap-1">
        <Button size="sm" variant="ghost" title={`deliver ${props.svc.name}'s traffic to a workspace`}>Intercept…</Button>
        <Button size="sm" variant="ghost" icon="x" title={`remove ${props.svc.name}; its files stay on the volume`} />
      </span>
    </Line>
  );
}

function Empty(props: { children: JSX.Element }) {
  return <p class="border border-dashed border-line px-4 py-6 text-center text-sm text-subtle">{props.children}</p>;
}

function Section(props: { title: string; count: number; bare?: boolean; children: JSX.Element }) {
  return (
    <section class="flex flex-col gap-2">
      <h2 class="flex items-center gap-2 text-xs font-semibold uppercase text-muted">
        {props.title}
        <Show when={!props.bare}>
          <span class="rounded-full bg-active px-1.5 font-mono text-2xs font-normal tracking-normal text-subtle">{props.count}</span>
        </Show>
      </h2>
      {props.children}
    </section>
  );
}

function Table(props: { cols: string; head: string[]; children: JSX.Element }) {
  return (
    <div class="overflow-hidden border border-line">
      <div class={`grid ${props.cols} gap-4 border-b border-line bg-codeblock px-4 py-2 text-xs font-semibold uppercase text-subtle`}>
        <For each={props.head}>{(h, i) => <span classList={{ "text-right": i() === props.head.length - 1 }}>{h}</span>}</For>
      </div>
      <div class="divide-y divide-line-subtle">{props.children}</div>
    </div>
  );
}

function Line(props: { cols: string; children: JSX.Element }) {
  return <div class={`grid ${props.cols} items-center gap-4 px-4 py-2 hover:bg-hover`}>{props.children}</div>;
}

/** An http port is the link; an intercepted port names where its traffic goes. */
function PortTag(props: { port: Port; env: string; service: string }) {
  return (
    <span class="whitespace-nowrap">
      <Show when={props.port.url} fallback={<span class="text-subtle">{props.port.protocol}:{props.port.port}</span>}>
        {(url) => (
          <button
            class="text-muted underline decoration-transparent underline-offset-2 hover:text-accent hover:decoration-current"
            title={`open ${url()} in a window`}
            onClick={() => window.harness.openPreview(url(), `${props.env} \u00b7 ${props.service}:${props.port.port}`)}
          >
            {props.port.protocol}:{props.port.port}
          </button>
        )}
      </Show>
    </span>
  );
}
