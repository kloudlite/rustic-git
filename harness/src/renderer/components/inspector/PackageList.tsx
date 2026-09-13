import { For, Show } from "solid-js";
import { Button } from "../../ui/Button";
import type { Package } from "../../model";

/**
 * What the workspace has installed, as the platform pins it: a name and the
 * version that resolved. A pinned entry is the person's own `attr@version`;
 * the rest float with the region's nixpkgs pin and move on Update.
 */
/** `inherited` names the workspace an ephemeral's copy was cut from: the
    list is that workspace's, frozen at the cut, and nobody edits it here. */
export function PackageList(props: { packages: Package[]; inherited?: string }) {
  return (
    <div class="pt-1.5 pb-2">
      <For each={props.packages}>
        {(p) => (
          <div class="flex h-6.5 items-center gap-2 px-3 font-mono text-sm hover:bg-hover" title={p.pinned ? `${p.name}@${p.version} — pinned` : `${p.name} — floats with the region pin`}>
            <span class="min-w-0 flex-1 truncate text-fg">{p.name}</span>
            <span class="text-xs tabular-nums text-muted">{p.version}</span>
            <span class="w-3 text-center text-2xs text-subtle">{p.pinned ? "@" : ""}</span>
          </div>
        )}
      </For>
      <Show
        when={!props.inherited}
        fallback={<div class="px-3 pt-2 pb-1 text-xs text-subtle">as cut from {props.inherited}; the workspace's list is the one to edit</div>}
      >
        <div class="flex flex-wrap gap-1.5 px-3 pt-2 pb-1">
          <Button icon="plus">Add</Button>
          <Button variant="ghost">Update</Button>
        </div>
      </Show>
    </div>
  );
}
