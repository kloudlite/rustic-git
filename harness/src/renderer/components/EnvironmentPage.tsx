import { For, Show, type JSX } from "solid-js";
import { Button } from "../ui/Button";
import { Badge } from "../ui/Badge";
import { SERVICE } from "./status";
import type { Environment, Port, Snapshot } from "../model";

/**
 * An environment in full, as its own tab. It belongs to the team, so what a
 * developer can do here is take a snapshot or a clone, never edit it in place.
 *
 * Layout is a header with the facts as stats, then two tables with the same
 * shape — a titled section, a column header row, hairline rows — so the page
 * reads as one document rather than a sentence, a table and a card.
 */
export function EnvironmentPage(props: { env: Environment; snapshots: Snapshot[] }) {
  const up = () => props.env.services.filter((s) => s.state === "running").length;
  const mine = () => props.env.owner === "you";

  return (
    <div class="overflow-y-auto px-6 py-6 select-text">
      <div class="mx-auto flex w-full max-w-[1100px] flex-col gap-8">
        <header class="flex flex-col gap-4">
          <div class="flex items-center gap-3">
            <h1 class="truncate font-mono text-md font-medium">{props.env.name}</h1>
            <Badge tone={mine() ? "accent" : "neutral"}>{mine() ? "yours" : "team"}</Badge>
            <span class="flex-1" />
            <Button icon="camera" size="sm" title="Freeze this environment so you can come back to it">Snapshot</Button>
            <Button icon="copy" size="sm" variant="ghost" title="Make a copy of your own">Clone</Button>
          </div>
          <dl class="grid grid-cols-2 gap-px overflow-hidden rounded-md border border-line-subtle bg-line-subtle sm:grid-cols-4">
            <Stat label="Services">
              <span class="text-fg">{up()}</span>
              <span class="text-subtle"> / {props.env.services.length} up</span>
            </Stat>
            <Stat label="Source">
              <Show when={props.env.from} fallback={<span class="text-subtle">created empty</span>}>
                {(f) => (
                  <>
                    <span class="text-fg">{f().name}</span>
                    <span class="text-subtle"> · {f().kind}</span>
                  </>
                )}
              </Show>
            </Stat>
            <Stat label="Region"><span class="text-fg">{props.env.region}</span></Stat>
            <Stat label="Snapshots"><span class="text-fg">{props.snapshots.length}</span></Stat>
          </dl>
        </header>

        <Section title="Services" count={props.env.services.length}>
          <Table cols="grid-cols-[180px_minmax(0,1fr)_150px_minmax(0,260px)]" head={["Service", "Image", "State", "Ports"]}>
            <For each={props.env.services}>
              {(svc) => (
                <Line cols="grid-cols-[180px_minmax(0,1fr)_150px_minmax(0,260px)]">
                  <span class="truncate font-mono text-sm">{svc.name}</span>
                  <span class="truncate font-mono text-xs text-subtle">{svc.image}</span>
                  <span class="flex items-center gap-2 text-xs">
                    <span class={`size-1.5 shrink-0 rounded-full ${SERVICE[svc.state].dot}`} />
                    <span class={svc.state === "running" ? "text-muted" : "text-fg"}>{svc.note ?? SERVICE[svc.state].label}</span>
                  </span>
                  <span class="flex flex-wrap justify-end gap-x-3 gap-y-1 font-mono text-xs">
                    <Show when={svc.ports.length === 0}><span class="text-subtle">—</span></Show>
                    <For each={svc.ports}>{(p) => <PortTag port={p} env={props.env.name} service={svc.name} />}</For>
                  </span>
                </Line>
              )}
            </For>
          </Table>
        </Section>

        <Section title="Snapshots" count={props.snapshots.length}>
          <Show
            when={props.snapshots.length}
            fallback={
              <p class="rounded-md border border-dashed border-line px-4 py-6 text-center text-sm text-subtle">
                No snapshots yet. A snapshot freezes what every service runs, so you can clone it back later.
              </p>
            }
          >
            <Table cols="grid-cols-[minmax(0,1fr)_140px_120px_90px_84px]" head={["Name", "Taken", "By", "Services", ""]}>
              <For each={props.snapshots}>
                {(s) => (
                  <Line cols="grid-cols-[minmax(0,1fr)_140px_120px_90px_84px]">
                    <span class="min-w-0">
                      <span class="block truncate text-sm">{s.name}</span>
                      <Show when={s.note}>{(n) => <span class="block truncate text-xs text-subtle">{n()}</span>}</Show>
                    </span>
                    <span class="text-xs text-muted">{s.at}</span>
                    <span class="truncate text-xs text-muted">{s.by}</span>
                    <span class="font-mono text-xs tabular-nums text-muted">{s.services}</span>
                    <span class="flex justify-end">
                      <Button size="sm" variant="ghost" icon="copy" title={`clone a new environment from ${s.name}`}>Clone</Button>
                    </span>
                  </Line>
                )}
              </For>
            </Table>
          </Show>
        </Section>
      </div>
    </div>
  );
}

function Stat(props: { label: string; children: JSX.Element }) {
  return (
    <div class="bg-bg px-4 py-3">
      <dt class="text-2xs font-semibold tracking-[0.08em] uppercase text-subtle">{props.label}</dt>
      <dd class="mt-1 truncate font-mono text-sm">{props.children}</dd>
    </div>
  );
}

function Section(props: { title: string; count: number; children: JSX.Element }) {
  return (
    <section class="flex flex-col gap-2.5">
      <h2 class="flex items-center gap-2 text-xs font-semibold tracking-[0.06em] uppercase text-muted">
        {props.title}
        <span class="rounded-full bg-active px-1.5 font-mono text-2xs font-normal tracking-normal text-subtle">{props.count}</span>
      </h2>
      {props.children}
    </section>
  );
}

function Table(props: { cols: string; head: string[]; children: JSX.Element }) {
  return (
    <div class="overflow-hidden rounded-md border border-line-subtle">
      <div class={`grid ${props.cols} gap-4 border-b border-line-subtle bg-panel px-4 py-2 text-2xs font-semibold tracking-[0.06em] uppercase text-subtle`}>
        <For each={props.head}>{(h, i) => <span classList={{ "text-right": i() === props.head.length - 1 }}>{h}</span>}</For>
      </div>
      <div class="divide-y divide-line-subtle">{props.children}</div>
    </div>
  );
}

function Line(props: { cols: string; children: JSX.Element }) {
  return <div class={`grid ${props.cols} items-center gap-4 px-4 py-2.5 hover:bg-hover`}>{props.children}</div>;
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
            onClick={() => window.harness.openPreview(url(), `${props.env} · ${props.service}:${props.port.port}`)}
          >
            {props.port.protocol}:{props.port.port}
          </button>
        )}
      </Show>
      <Show when={props.port.intercept}>
        {(ic) => (
          <span class="text-accent" title={`the environment still dials ${props.service}:${props.port.port}; traffic is delivered to ${ic().workspace}:${ic().port}`}>
            <span class="px-1 text-subtle">→</span>
            {ic().workspace}:{ic().port}
          </span>
        )}
      </Show>
    </span>
  );
}
