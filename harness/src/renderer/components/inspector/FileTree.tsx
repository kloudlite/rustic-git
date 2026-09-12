import { For, Show, createSignal } from "solid-js";
import { Icon } from "../../ui/Icon";
import { Row, Gutter } from "../../ui/parts";
import type { FileNode } from "../../model";

const STATUS_TONE: Record<string, string> = { M: "text-modified", A: "text-created", D: "text-deleted line-through" };

/** The working copy's tree, with git status on the files that differ. */
export function FileTree(props: { nodes: FileNode[]; depth?: number; path?: string; onOpen: (path: string, status?: string) => void }) {
  const depth = () => props.depth ?? 0;
  const at = (name: string) => (props.path ? `${props.path}/${name}` : name);
  return (
    <For each={props.nodes}>
      {(n) => {
        const [open, setOpen] = createSignal(!!n.open);
        return (
          <>
            <Row
              class="h-6"
              style={{ "padding-left": `${12 + depth() * 14}px` }}
              onClick={() => (n.dir ? setOpen(!open()) : props.onOpen(at(n.name), n.status))}
              title={n.name}
            >
              <Gutter>
                <Show when={n.dir}><Icon name={open() ? "chevronDown" : "chevronRight"} size={11} class="text-subtle" /></Show>
              </Gutter>
              <Gutter><Icon name={n.dir ? "folder" : "file"} size={13} class="text-muted" /></Gutter>
              <span class={`min-w-0 flex-1 truncate px-1 text-sm ${n.status ? STATUS_TONE[n.status] : ""}`}>{n.name}</span>
              <Show when={n.status}>{(s) => <span class="shrink-0 pr-1 font-mono text-2xs text-subtle">{s()}</span>}</Show>
            </Row>
            <Show when={n.dir && open() && n.children}>
              <FileTree nodes={n.children!} depth={depth() + 1} path={at(n.name)} onOpen={props.onOpen} />
            </Show>
          </>
        );
      }}
    </For>
  );
}
