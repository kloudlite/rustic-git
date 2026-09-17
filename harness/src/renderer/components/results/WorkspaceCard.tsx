import { For, Show } from "solid-js";
import { Dot, Field, Pill, Card } from "./parts";

/** A workspace document: what it is, where it runs, what it has. */
export function WorkspaceCard(props: { data: Record<string, any> }) {
  const d = () => props.data;
  const pkgs = () => (d().packages as string[] | undefined) ?? [];
  const st = () => d().packages_status as { ready?: boolean; reason?: string; message?: string } | undefined;
  return (
    <Card>
      <div class="flex items-center gap-2">
        <Dot state={String(d().state ?? "")} />
        <span class="font-bold text-fg-strong">{d().name ?? d().id}</span>
        <span class="text-subtle">{d().id}</span>
        <span class="text-muted">{d().state}</span>
      </div>
      <div class="grid grid-cols-[repeat(auto-fit,minmax(220px,1fr))] gap-x-8 gap-y-0.5">
        <Show when={d().node || d().placement}><Field label="node">{d().node ?? d().placement}</Field></Show>
        <Show when={d().region}><Field label="region">{d().region}</Field></Show>
        <Show when={d().owner}><Field label="owner">{d().owner}</Field></Show>
        <Show when={d().environment}><Field label="environment">{d().environment}</Field></Show>
        <Show when={d().repo}><Field label="repo">{`${d().repo}${d().branch ? `#${d().branch}` : ""}`}</Field></Show>
      </div>
      <Show when={pkgs().length || st()}>
        <div class="flex flex-col gap-1">
          <div class="flex items-center gap-1.5 text-subtle">
            packages
            <Show when={st()}>{(s) => <><Dot state={s().ready ? "ready" : "creating"} />{s().reason ?? (s().ready ? "ready" : "not ready")}</>}</Show>
          </div>
          <Show when={pkgs().length} fallback={<span class="text-subtle">none</span>}>
            <div class="flex flex-wrap gap-1"><For each={pkgs()}>{(p) => <Pill>{p}</Pill>}</For></div>
          </Show>
          <Show when={st()?.message}><span class="text-muted">{st()!.message}</span></Show>
        </div>
      </Show>
    </Card>
  );
}
