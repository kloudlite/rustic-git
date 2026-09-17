import { For, Show, createResource } from "solid-js";
import { Icon } from "../../ui/Icon";
import { Row, Gutter } from "../../ui/parts";
import * as live from "../../live";
import { hidden, ignoredKey, isDir } from "../../rows";

const STATUS_TONE: Record<string, string> = { M: "text-modified", A: "text-created", D: "text-deleted line-through" };

/**
 * A workspace's own tree, read from its tool server one directory at a time. Two rules the first
 * version got wrong (owner, 2026-09-18):
 *
 * 1. A DIRECTORY is `kind: "dir"` in the tool server's own words, not a boolean we invented — every
 *    row drew with the file glyph and no chevron.
 * 2. WHICH FOLDERS ARE OPEN is kept outside the fetched rows, keyed by path, so a refetch (the live
 *    poll, a tab switch) cannot shut what a person opened. The rows are data; the open set is the
 *    panel's own, and it outlives them.
 */
export function FsTree(props: {
  scope: string;
  /** Open paths, keyed by path and owned by the panel — never by a row object. */
  open: Set<string>;
  onToggle: (path: string) => void;
  onOpen: (path: string, status?: string) => void;
  path?: string;
  depth?: number;
}) {
  const depth = () => props.depth ?? 0;
  const at = (name: string) => (props.path ? `${props.path}/${name}` : name);
  const [rows] = createResource(
    () => ({ scope: props.scope, path: props.path }),
    async (k) => (await live.fsTree(k.scope, k.path))?.entries ?? [],
  );
  const shown = () => (rows() ?? []).filter((e) => !hidden(e));
  const tucked = () => (rows() ?? []).filter((e) => hidden(e));
  const tuckedOpen = () => props.open.has(ignoredKey(props.path));

  const Entry = (p: { e: live.FsEntry; dim?: boolean }) => {
    const dir = () => isDir(p.e);
    const here = () => at(p.e.name);
    const open = () => props.open.has(here());
    return (
      <>
        <Row
          class={`h-5.5${p.dim ? " opacity-60" : ""}`}
          style={{ "padding-left": `${12 + depth() * 14}px` }}
          onClick={() => (dir() ? props.onToggle(here()) : props.onOpen(here(), p.e.git || undefined))}
          title={here()}
        >
          <Gutter>
            <Show when={dir()}>
              <Icon name={open() ? "chevronDown" : "chevronRight"} size={16} class="text-muted" />
            </Show>
          </Gutter>
          <Gutter>
            <Icon name={dir() ? "folder" : "file"} size={16} class="text-muted" />
          </Gutter>
          <span class={`min-w-0 flex-1 truncate px-1 ${p.e.git ? (STATUS_TONE[p.e.git] ?? "") : ""}`}>{p.e.name}</span>
          <Show when={p.e.git}>{(g) => <span class="shrink-0 pr-1 font-mono text-2xs text-subtle">{g()}</span>}</Show>
        </Row>
        {/* Opened lazily: a directory's own listing is fetched when it is first opened, and the
            fetch is keyed by path, so reopening one that was read before is instant. */}
        <Show when={dir() && open()}>
          <FsTree scope={props.scope} open={props.open} onToggle={props.onToggle} onOpen={props.onOpen} path={here()} depth={depth() + 1} />
        </Show>
      </>
    );
  };

  return (
    <Show
      when={!rows.loading || rows()}
      fallback={
        <Row class="h-5.5" style={{ "padding-left": `${12 + depth() * 14}px` }}>
          <span class="px-1 text-subtle">reading…</span>
        </Row>
      }
    >
      <For each={shown()}>{(e) => <Entry e={e} />}</For>
      <Show when={tucked().length}>
        {/* What the person did not ask to see — `.git`, `node_modules`, build output — under one
            dim line rather than at the top of every listing. */}
        <Row class="h-5.5" style={{ "padding-left": `${12 + depth() * 14}px` }} onClick={() => props.onToggle(ignoredKey(props.path))}>
          <Gutter>
            <Icon name={tuckedOpen() ? "chevronDown" : "chevronRight"} size={16} class="text-subtle" />
          </Gutter>
          <Gutter />
          <span class="min-w-0 flex-1 truncate px-1 text-subtle">{tucked().length} ignored</span>
        </Row>
        <Show when={tuckedOpen()}>
          <For each={tucked()}>{(e) => <Entry e={e} dim />}</For>
        </Show>
      </Show>
    </Show>
  );
}
