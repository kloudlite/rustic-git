import { For, Show } from "solid-js";
import { Icon } from "../ui/Icon";
import { Heading, Row, Gutter, Empty } from "../ui/parts";
import type { Image, Repo, Workspace } from "../model";

/** The team's repositories: what a workspace is cut from. */
export function ReposPanel(props: { repos: Repo[]; workspaces: Workspace[] }) {
  return (
    <nav class="flex min-h-0 flex-col border-r border-line bg-panel">
      <div class="flex-1 overflow-x-hidden overflow-y-auto pb-3">
        <Heading meta={String(props.repos.length)}>Repositories</Heading>
        <Show when={!props.repos.length}><Empty>No repositories in this team yet.</Empty></Show>
        <For each={props.repos}>
          {(r) => {
            const copies = () => props.workspaces.filter((w) => w.repo === r.name).length;
            const [owner, name] = r.name.split("/");
            return (
              <Row class="group h-9" title={`${r.name} · ${r.branch} · updated ${r.updated}`}>
                <Gutter><Icon name="repo" size={14} class="text-muted" /></Gutter>
                <div class="flex min-w-0 flex-1 flex-col px-1 leading-tight">
                  <span class="truncate text-fg"><span class="text-subtle">{owner}/</span>{name}</span>
                  <span class="flex items-center gap-1.5 font-mono text-2xs text-subtle">
                    <span>{r.branch}</span><span>·</span><span>{r.updated}</span>
                    <Show when={copies()}><span>·</span><span>{copies()} {copies() === 1 ? "workspace" : "workspaces"}</span></Show>
                  </span>
                </div>
                <Show when={r.private}><Icon name="lock" size={11} class="shrink-0 text-subtle" /></Show>
                <button
                  class="ml-1 hidden size-5 shrink-0 items-center justify-center rounded-sm text-subtle group-hover:inline-flex hover:bg-active hover:text-fg"
                  title={`New workspace on ${r.name}`}
                  onClick={(e) => e.stopPropagation()}
                >
                  <Icon name="plus" size={12} />
                </button>
              </Row>
            );
          }}
        </For>
      </div>
    </nav>
  );
}

/** The team's images, as the registry holds them. */
export function RegistriesPanel(props: { images: Image[] }) {
  return (
    <nav class="flex min-h-0 flex-col border-r border-line bg-panel">
      <div class="flex-1 overflow-x-hidden overflow-y-auto pb-3">
        <Heading meta={String(props.images.length)}>Images</Heading>
        <Show when={!props.images.length}><Empty>Nothing pushed to this team's registry yet.</Empty></Show>
        <For each={props.images}>
          {(im) => {
            const [owner, name] = im.name.split("/");
            return (
              <Row class="h-9" title={`${im.name} · ${im.tags.length} tags · ${im.size} · ${im.pulls} pulls`}>
                <Gutter><Icon name="container" size={14} class="text-muted" /></Gutter>
                <div class="flex min-w-0 flex-1 flex-col px-1 leading-tight">
                  <span class="truncate text-fg"><span class="text-subtle">{owner}/</span>{name}</span>
                  <span class="flex items-center gap-1.5 font-mono text-2xs text-subtle">
                    <span class="truncate">{im.tags.slice(0, 2).join(" ")}</span>
                    <Show when={im.tags.length > 2}><span>+{im.tags.length - 2}</span></Show>
                    <span>·</span><span>{im.pushed}</span>
                  </span>
                </div>
                <span class="shrink-0 font-mono text-2xs text-subtle">{im.size}</span>
              </Row>
            );
          }}
        </For>
      </div>
    </nav>
  );
}
