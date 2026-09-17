import { For, Show } from "solid-js";
import { Bar, Card } from "./parts";

const DIMENSIONS: [string, string, string][] = [
  ["workspaces", "workspaces", ""],
  ["environments", "environments", ""],
  ["snapshots", "snapshots", ""],
  ["diskGb", "disk", "GB"],
  ["cpu", "cpu", ""],
  ["memoryGb", "memory", "GB"],
];

/** Quota: what is used of what is allowed, per dimension. Computed from the CRDs on every read, so it is never stale. */
export function QuotaCard(props: { data: Record<string, any> }) {
  const limit = () => (props.data.limit ?? {}) as Record<string, number>;
  const used = () => (props.data.used ?? {}) as Record<string, number>;
  const disk = () => props.data.disk as { usedAt?: string } | undefined;
  return (
    <Card>
      <Show when={props.data.owner}><div class="text-subtle">{props.data.owner}</div></Show>
      <div class="grid grid-cols-[repeat(auto-fit,minmax(240px,1fr))] gap-x-8 gap-y-0.5">
        <For each={DIMENSIONS.filter(([k]) => limit()[k] !== undefined || used()[k] !== undefined)}>
          {([k, label, unit]) => (
            <div class="flex items-center gap-3">
              <span class="w-24 shrink-0 truncate text-subtle">{label}</span>
              <Bar used={Number(used()[k] ?? 0)} limit={Number(limit()[k] ?? 0)} />
              <span class="tabular-nums text-fg">{used()[k] ?? 0} / {limit()[k] ?? "—"}{unit}</span>
            </div>
          )}
        </For>
      </div>
      <Show when={disk()?.usedAt}>{(at) => <div class="text-xs text-subtle">disk measured {at()}</div>}</Show>
    </Card>
  );
}
