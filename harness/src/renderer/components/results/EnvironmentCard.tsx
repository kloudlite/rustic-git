import { For, Show } from "solid-js";
import { Dot, Field, Pill, Card } from "./parts";

/** An environment document: its services as a table, and what is intercepted. */
export function EnvironmentCard(props: { data: Record<string, any> }) {
  const d = () => props.data;
  const services = () => (d().services as Record<string, any>[] | undefined) ?? [];
  const status = () => (d().service_status ?? d().services_status ?? []) as { name: string; ready?: boolean; intercepted_by?: string; message?: string }[];
  const of = (name: string) => status().find((s) => s.name === name);
  const intercepts = () => (d().intercepts as { service: string; workspace: string; ports?: { from: number; to: number }[] }[] | undefined) ?? [];
  return (
    <Card>
      <div class="flex items-center gap-2">
        <Dot state={String(d().state ?? "")} />
        <span class="font-bold text-fg-strong">{d().name ?? d().id}</span>
        <span class="text-subtle">{d().id}</span>
        <span class="text-xs text-muted">{d().state}</span>
      </div>
      <div class="grid grid-cols-[repeat(auto-fit,minmax(220px,1fr))] gap-x-8 gap-y-0.5">
        <Show when={d().region}><Field label="region">{d().region}</Field></Show>
        <Show when={d().owner}><Field label="owner">{d().owner}</Field></Show>
        <Show when={d().placement}><Field label="node">{d().placement}</Field></Show>
      </div>
      <Show when={services().length} fallback={<span class="text-subtle">no services</span>}>
        <table class="border-collapse text-xs">
          <thead>
            <tr><For each={["service", "image", "ports", ""]}>{(h) => <th class="border-b border-line px-2 py-1 text-left font-bold text-fg-strong">{h}</th>}</For></tr>
          </thead>
          <tbody>
            <For each={services()}>
              {(s) => {
                const st = of(String(s.name));
                return (
                  <tr>
                    <td class="border-b border-line-subtle px-2 py-1 text-fg">{s.name}</td>
                    <td class="max-w-[280px] truncate border-b border-line-subtle px-2 py-1 text-muted" title={String(s.image ?? "")}>{s.image}</td>
                    <td class="border-b border-line-subtle px-2 py-1 text-muted">{((s.ports as number[] | undefined) ?? []).join(", ") || "—"}</td>
                    <td class="border-b border-line-subtle px-2 py-1">
                      <span class="flex items-center gap-1.5">
                        <Dot state={st?.ready ? "ready" : st ? "creating" : "stopped"} />
                        <span class="text-muted">{st ? (st.ready ? "ready" : (st.message ?? "not ready")) : "—"}</span>
                        <Show when={st?.intercepted_by}>{(w) => <Pill tone="warn">→ {w()}</Pill>}</Show>
                      </span>
                    </td>
                  </tr>
                );
              }}
            </For>
          </tbody>
        </table>
      </Show>
      <Show when={intercepts().length}>
        <div class="flex flex-wrap items-center gap-1 text-xs text-subtle">
          intercepts
          <For each={intercepts()}>
            {(i) => <Pill tone="warn">{i.service} → {i.workspace}{i.ports?.length ? ` (${i.ports.map((p) => `${p.from}:${p.to}`).join(", ")})` : ""}</Pill>}
          </For>
        </div>
      </Show>
    </Card>
  );
}
