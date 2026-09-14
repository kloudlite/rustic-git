import { For, Show, createSignal } from "solid-js";
import { Icon } from "../ui/Icon";
import { Button } from "../ui/Button";
import { Kbd } from "../ui/parts";
import { KEYS } from "../keys";
import { PROVIDERS, type Machine, type Plugin, type Provider } from "../model";
import { TOOLS } from "../../../pi/catalog";

/**
 * The bench's settings, as a page of its own — a section list on the
 * left, the sections on the right, a filter box over both — the shape every
 * editor's settings page has, so nobody has to learn it.
 *
 * Everything here is the MACHINE's: the model it thinks with, the skills it
 * can be told to follow, the servers that lend it tools, the hooks that run in
 * its loop. A workspace has no settings of its own; an agent inherits these.
 */
export function SettingsPage(props: { machine: Machine }) {
  const [q, setQ] = createSignal("");
  const hit = (...s: (string | undefined)[]) => !q() || s.some((x) => x?.toLowerCase().includes(q().toLowerCase()));
  const of = (k: Plugin["kind"]) => props.machine.plugins.filter((p) => p.kind === k && hit(p.name, p.from));
  const keys = () => Object.values(KEYS).filter((b) => hit(b.label, b.keys));

  type Page = "model" | "providers" | "tools" | "skills" | "mcp" | "hooks" | "keys" | "account" | "discover";
  const PAGES: { id: Page; label: string }[] = [
    { id: "model", label: "Model" },
    { id: "providers", label: "Providers" },
    { id: "tools", label: "Tools" },
    { id: "skills", label: "Skills" },
    { id: "mcp", label: "MCP servers" },
    { id: "hooks", label: "Hooks" },
    { id: "keys", label: "Keyboard" },
    { id: "account", label: "Account" },
  ];
  const [who, setWho] = createSignal("");
  const [api, setApi] = createSignal("");
  const [team, setTeam] = createSignal("");
  void window.harness.auth.status().then((s) => s.phase === "ready" && (setWho(s.username), setTeam(s.team)));
  void window.harness.auth.api().then(setApi);
  const [page, setPage] = createSignal<Page>(location.hash.endsWith("/discover") ? "discover" : "model");
  const [filter, setFilter] = createSignal("all");
  const installed = (name: string) => props.machine.plugins.some((p) => p.name === name);

  return (
    <div class="grid min-h-0 grid-cols-[200px_minmax(0,1fr)] select-text">
      <nav class="flex flex-col gap-0.5 border-r border-line-subtle px-2 py-4">
        <For each={PAGES}>
          {(s) => (
            <button class={NAV} aria-current={page() === s.id} onClick={() => setPage(s.id)}>
              {s.label}
            </button>
          )}
        </For>
        <span class="my-2 h-px bg-line-subtle" />
        <button class={NAV} aria-current={page() === "discover"} onClick={() => setPage("discover")}>
          <Icon name="sparkle" size={13} class="text-accent" /> Discover
        </button>
      </nav>
      <div class="flex min-h-0 flex-col overflow-y-auto">
        <div class="sticky top-0 z-10 flex items-center gap-2 border-b border-line-subtle bg-bg px-6 py-3">
          <Icon name="search" size={13} class="text-subtle" />
          <input
            class="h-6 flex-1 bg-transparent text-sm outline-none placeholder:text-subtle"
            placeholder={page() === "discover" ? "search the marketplace" : "search settings"}
            value={q()}
            onInput={(e) => setQ(e.currentTarget.value)}
          />
        </div>
        <div class="flex w-full max-w-[760px] flex-col gap-10 px-6 py-6">
          <Show when={page() === "account"}>
            <Section id="account" title="Account" hint="this app's Kloudlite login">
              <Row name="signed in as" detail="a CLI login labelled with this computer's name and (desktop)">
                <span class="font-mono text-sm text-fg">{who()}</span>
              </Row>
              <Row name="team" detail="the team whose bench this app is connected to">
                <span class="font-mono text-sm text-fg">{team()}</span>
                <Button onClick={() => void window.harness.auth.switchTeam()}>Switch team</Button>
              </Row>
              <Row name="address" detail="change it from the login screen, after signing out">
                <span class="font-mono text-sm text-fg">{api()}</span>
              </Row>
              <Row name="sign out" detail="revokes this login, forgets it here, and disconnects the bench">
                <Button variant="danger" onClick={() => void window.harness.auth.signOut()}>Sign out</Button>
              </Row>
            </Section>
          </Show>
          <Show when={page() === "model"}>
          <Section id="model" title="Model" hint="what the machine thinks with">
            <Show when={hit("model", props.machine.model)}>
              <Row name="model" detail="every thread and every agent cut from this machine">
                <select class="h-6.5 rounded-[2px] border border-input-line bg-input px-1.5 font-mono text-sm text-fg outline-none focus:border-focus" value={props.machine.model}>
                  <For each={PROVIDERS.filter((p) => p.state === "connected")}>
                    {(p) => <optgroup label={p.name}><For each={p.models}>{(m) => <option value={m}>{m}</option>}</For></optgroup>}
                  </For>
                </select>
              </Row>
            </Show>
            <Show when={hit("owner", props.machine.owner)}>
              <Row name="owner" detail="whose keys and quota the machine runs under">
                <span class="font-mono text-sm text-fg">{props.machine.owner}</span>
              </Row>
            </Show>
          </Section>
          </Show>

          <Show when={page() === "providers"}>
          <Section id="providers" title="Providers" hint="a key connects one; its models become choices above">
            <For each={PROVIDERS.filter((p) => hit(p.name, ...p.models))}>
              {(p) => (
                <Row name={p.name} detail={p.note ?? PROVIDER_NOTE[p.state](p)} off={p.state !== "connected"} bad={p.state === "unreachable"}>
                  <span class={`size-1.5 rounded-full ${PROVIDER_DOT[p.state]}`} />
                  <Show when={p.state === "no-key"} fallback={<Button variant="ghost" size="sm">{p.state === "connected" ? "Replace key" : "Retry"}</Button>}>
                    <Button size="sm">Add key</Button>
                  </Show>
                </Row>
              )}
            </For>
          </Section>
          </Show>

          <Show when={page() === "tools"}>
            <Section id="tools" title="Tools" hint="what the bench can do; every write and delete is marked">
              <For each={["shell", "workspace", "environment", "platform"] as const}>
                {(g) => (
                  <>
                    <div class="mt-4 mb-1 text-2xs font-semibold uppercase text-subtle first:mt-0">{g}</div>
                    <For each={TOOLS.filter((t) => t.group === g && hit(t.name, t.summary))}>
                      {(t) => (
                        <Row name={t.name} detail={t.summary} mono>
                          <span class={`rounded-sm px-1.5 font-mono text-2xs ${EFFECT[t.effect]}`}>{t.effect}</span>
                          <Show when={t.builtin}><span class="text-2xs text-subtle">pi</span></Show>
                        </Row>
                      )}
                    </For>
                  </>
                )}
              </For>
            </Section>
          </Show>
          <Show when={page() === "skills"}>
          <Section id="skills" title="Skills" hint="a prompt the machine follows when told /name" action={<Button icon="plus" size="sm">Add</Button>}>
            <For each={of("skill")}>
              {(p) => (
                <Row name={`/${p.name}`} from={p.from} detail={(p as { summary: string }).summary} mono off={!p.enabled}>
                  <Switch on={p.enabled} />
                </Row>
              )}
            </For>
          </Section>
          </Show>

          <Show when={page() === "mcp"}>
          <Section id="mcp" title="MCP servers" hint="processes that lend the machine tools" action={<Button icon="plus" size="sm">Add</Button>}>
            <For each={of("mcp")}>
              {(p) => {
                const m = p as Extract<Plugin, { kind: "mcp" }>;
                return (
                  <Row name={m.name} from={m.from} detail={m.note ?? (m.state === "connected" ? `${m.tools} tools` : m.state)} mono off={!m.enabled} bad={m.enabled && m.state === "failed"}>
                    <span class={`size-1.5 rounded-full ${m.enabled ? MCP_DOT[m.state] : "bg-subtle"}`} />
                    <Switch on={m.enabled} />
                  </Row>
                );
              }}
            </For>
          </Section>
          </Show>

          <Show when={page() === "hooks"}>
          <Section id="hooks" title="Hooks" hint="run at a moment of the machine's own loop" action={<Button icon="plus" size="sm">Add</Button>}>
            <For each={of("hook")}>
              {(p) => (
                <Row name={p.name} from={p.from} detail={(p as { on: string }).on} mono off={!p.enabled}>
                  <Switch on={p.enabled} />
                </Row>
              )}
            </For>
          </Section>
          </Show>

          <Show when={page() === "discover"}>
            <Section id="discover" title="Discover" hint="skills and servers other people published; one click installs onto this machine">
              <div class="mb-3 flex gap-1.5 pt-2">
                <For each={["all", "skills", "mcp servers"]}>
                  {(f) => <button class="h-5.5 rounded-[2px] border border-btn2-line bg-btn2 px-2.5 text-xs text-fg hover:bg-btn2-hover aria-pressed:border-focus aria-pressed:bg-selected" aria-pressed={filter() === f} onClick={() => setFilter(f)}>{f}</button>}
                </For>
              </div>
              <div class="grid grid-cols-2 gap-3">
                <For each={MARKET.filter((m) => (filter() === "all" || (filter() === "skills") === (m.kind === "skill")) && hit(m.name, m.from, m.summary))}>
                  {(m) => (
                    <div class="flex flex-col gap-2 rounded-md bg-tile p-3">
                      <div class="flex items-center gap-2">
                        <Icon name={m.kind === "skill" ? "sparkle" : "server"} size={13} class="text-subtle" />
                        <span class="font-mono text-sm text-fg">{m.kind === "skill" ? `/${m.name}` : m.name}</span>
                        <span class="text-2xs text-subtle">{m.from}</span>
                        <span class="flex-1" />
                        <Show when={installed(m.name)} fallback={<Button size="sm">Install</Button>}>
                          <span class="text-2xs uppercase text-success">installed</span>
                        </Show>
                      </div>
                      <p class="text-xs leading-relaxed text-muted">{m.summary}</p>
                      <div class="flex items-center gap-3 text-2xs text-subtle">
                        <span>{m.installs} installs</span>
                        <Show when={m.kind === "mcp"}><span>{m.tools} tools</span></Show>
                        <span>{m.updated}</span>
                      </div>
                    </div>
                  )}
                </For>
              </div>
            </Section>
          </Show>
          <Show when={page() === "keys"}>
          <Section id="keys" title="Keyboard" hint="what the keys do; ⌘⇧P lists them too">
            <For each={keys()}>
              {(b) => (
                <Row name={b.label}>
                  <Kbd>{b.keys}</Kbd>
                </Row>
              )}
            </For>
            <Row name="thread by position"><Kbd>⌘1…9</Kbd></Row>
          </Section>
          </Show>
        </div>
      </div>
    </div>
  );
}

const NAV = "flex h-5.5 items-center gap-2 px-2.5 text-left text-muted hover:text-fg aria-[current=true]:font-semibold aria-[current=true]:text-fg";

/** What the marketplace lists. Fixture until there is a registry to ask. */
type Listing = { kind: "skill" | "mcp"; name: string; from: string; summary: string; installs: string; updated: string; tools?: number };
const MARKET: Listing[] = [
  { kind: "skill", name: "ship", from: "kloudlite/deploy", summary: "Pin, roll and verify a build on the fleet; refuses a red commit.", installs: "1.2k", updated: "2d ago" },
  { kind: "skill", name: "brainstorm", from: "superpowers", summary: "Turn an idea into a design and a spec before any code is written.", installs: "18k", updated: "1w ago" },
  { kind: "skill", name: "systematic-debugging", from: "superpowers", summary: "Reproduce, isolate, and root-cause before touching a fix.", installs: "14k", updated: "1w ago" },
  { kind: "skill", name: "ponytail", from: "ponytail", summary: "The lazy senior developer: stdlib first, shortest working diff.", installs: "6.1k", updated: "3d ago" },
  { kind: "mcp", name: "graft", from: "graft", summary: "A prebuilt call graph of the repo: find, callers, file API, map.", installs: "9.4k", updated: "5d ago", tools: 5 },
  { kind: "mcp", name: "clickstack", from: "clickstack", summary: "Logs, metrics, traces and dashboards from ClickHouse.", installs: "3.3k", updated: "2w ago", tools: 29 },
  { kind: "mcp", name: "github", from: "github", summary: "Issues, pull requests, checks and reviews on GitHub.", installs: "41k", updated: "1d ago", tools: 38 },
  { kind: "mcp", name: "postgres", from: "modelcontextprotocol", summary: "Read-only SQL against a Postgres database, schema included.", installs: "22k", updated: "3w ago", tools: 4 },
  { kind: "mcp", name: "playwright", from: "microsoft", summary: "Drive a browser: navigate, click, read, screenshot.", installs: "27k", updated: "4d ago", tools: 21 },
  { kind: "mcp", name: "figma", from: "figma", summary: "Read frames, tokens and components from a Figma file.", installs: "8.8k", updated: "1w ago", tools: 9 },
];

const EFFECT: Record<string, string> = { read: "bg-active text-muted", write: "bg-warning-wash text-warning", destroy: "bg-danger-wash text-danger" };

const PROVIDER_DOT: Record<Provider["state"], string> = { connected: "bg-success", "no-key": "bg-subtle", unreachable: "bg-danger" };
const PROVIDER_NOTE: Record<Provider["state"], (p: Provider) => string> = {
  connected: (p) => `${p.models.length} models`,
  "no-key": () => "no key on this machine",
  unreachable: () => "unreachable",
};

const MCP_DOT: Record<string, string> = { connected: "bg-success", starting: "bg-warning", failed: "bg-danger", off: "bg-subtle" };

function Section(props: { id: string; title: string; hint?: string; action?: any; children: any }) {
  return (
    <section id={`s-${props.id}`} class="scroll-mt-16">
      <div class="mb-2 flex items-baseline gap-3 border-b border-line-subtle pb-2">
        <h2 class="text-md font-medium">{props.title}</h2>
        <span class="text-xs text-subtle">{props.hint}</span>
        <span class="flex-1" />
        {props.action}
      </div>
      <div class="flex flex-col">{props.children}</div>
    </section>
  );
}

function Row(props: { name: string; from?: string; detail?: string; mono?: boolean; off?: boolean; bad?: boolean; children: any }) {
  return (
    <div class="flex min-h-11 items-center gap-4 border-b border-line-subtle py-2 last:border-b-0" classList={{ "text-subtle": props.off }}>
      <div class="flex min-w-0 flex-1 flex-col gap-0.5">
        <span class="flex items-baseline gap-2">
          <span class={props.mono ? "font-mono text-sm" : "text-sm"} classList={{ "text-fg": !props.off }}>{props.name}</span>
          <Show when={props.from}>{(f) => <span class="text-2xs text-subtle">{f()}</span>}</Show>
        </span>
        <Show when={props.detail}>{(d) => <span class="text-xs" classList={{ "text-danger": props.bad, "text-subtle": !props.bad }}>{d()}</span>}</Show>
      </div>
      <div class="flex shrink-0 items-center gap-3">{props.children}</div>
    </div>
  );
}

function Switch(props: { on: boolean }) {
  return (
    <button role="switch" aria-checked={props.on} class="relative h-3.5 w-6 shrink-0 rounded-full bg-active transition-colors aria-checked:bg-accent" title={props.on ? "Disable" : "Enable"}>
      <span class="absolute top-0.5 left-0.5 size-2.5 rounded-full bg-fg transition-transform" classList={{ "translate-x-2.5": props.on }} />
    </button>
  );
}
