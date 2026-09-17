import { For, Show, createResource } from "solid-js";
import { Icon } from "../../ui/Icon";
import { Row, Gutter } from "../../ui/parts";
import * as live from "../../live";
import { deletedIn, dimmed, isDir, rowTone, statusBadge, type FsChange } from "../../rows";

/**
 * A workspace's own tree, read from its tool server one directory at a time.
 *
 * Three rules, each one a thing the first version got wrong (owner, 2026-09-18):
 *
 * 1. A DIRECTORY is `kind: "dir"` in the tool server's own words, not a boolean we invented — every
 *    row drew with the file glyph and no chevron.
 * 2. WHICH FOLDERS ARE OPEN is kept outside the fetched rows, keyed by path, so a refetch (the live
 *    poll, a tab switch) cannot shut what a person opened.
 * 3. EVERY ENTRY STAYS IN ITS PLACE — directories first, then files, each alphabetical, as an
 *    editor lists them. An ignored entry is DIMMED, never grouped away ("why showing ignored
 *    separately").
 *
 * A deleted file is not on disk, so the tree cannot list it; the changes say where it was, and the
 * row is drawn there, struck through.
 */
export function FsTree(props: {
  scope: string;
  /** Which tree of that workspace: absent is its own working directory (spec §4.4). */
  tree?: string;
  /** Open paths, keyed by path and owned by the panel — never by a row object. */
  open: Set<string>;
  /** What differs from the branch, for the rows a tree cannot show. */
  changes?: readonly FsChange[];
  /** Paths this session's commits touched: tinted softer, since they are history, not work. */
  committed?: ReadonlySet<string>;
  onToggle: (path: string) => void;
  onOpen: (path: string, status?: string) => void;
  path?: string;
  depth?: number;
}) {
  const depth = () => props.depth ?? 0;
  const at = (name: string) => (props.path ? `${props.path}/${name}` : name);
  const [rows] = createResource(
    // `fsChanged` is in the key so a directory the watch patched is redrawn from the cache; the
    // fetch itself is a cache hit, not a second read.
    () => ({ scope: props.scope, tree: props.tree, path: props.path, v: live.fsChanged() }),
    async (k) => (await live.fsTree(k.scope, k.path, k.tree))?.entries ?? [],
  );
  /** Directories first, then files, each alphabetical — the order an editor's explorer uses. */
  const listed = () =>
    [...(rows() ?? [])].sort((a, b) => (isDir(a) === isDir(b) ? a.name.localeCompare(b.name) : isDir(a) ? -1 : 1));
  /** Files the tree cannot show because they are gone, put back where they were. */
  const deleted = () => deletedIn(props.changes ?? [], props.path);

  const Line = (p: { name: string; dir?: boolean; letter?: string; ignored?: boolean }) => {
    const here = () => at(p.name);
    const open = () => props.open.has(here());
    return (
      <>
        <Row
          class="h-5.5"
          style={{ "padding-left": `${12 + depth() * 14}px` }}
          onClick={() => (p.dir ? props.onToggle(here()) : props.onOpen(here(), p.letter))}
          title={here()}
        >
          <Gutter>
            <Show when={p.dir}>
              <Icon name={open() ? "chevronDown" : "chevronRight"} size={16} class="text-muted" />
            </Show>
          </Gutter>
          <Gutter>
            <Icon name={p.dir ? "folder" : "file"} size={16} class={p.ignored ? "text-subtle" : "text-muted"} />
          </Gutter>
          {/* Tinted by its git state, dimmed when ignored — the CHANGES list's own tokens. */}
          <span class={`min-w-0 flex-1 truncate px-1 ${rowTone(p.letter, p.ignored, props.committed?.has(here()))}`}>{p.name}</span>
          <Show when={statusBadge(p.letter)}>
            {(b) => <span class={`shrink-0 pr-1 font-mono text-2xs ${rowTone(p.letter, p.ignored)}`}>{b()}</span>}
          </Show>
        </Row>
        {/* Opened lazily: a directory's listing is fetched when it is first opened, keyed by path,
            so reopening one that was read before is instant. */}
        <Show when={p.dir && open()}>
          <FsTree
            scope={props.scope}
            tree={props.tree}
            open={props.open}
            changes={props.changes}
            committed={props.committed}
            onToggle={props.onToggle}
            onOpen={props.onOpen}
            path={here()}
            depth={depth() + 1}
          />
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
      <For each={listed()}>{(e) => <Line name={e.name} dir={isDir(e)} letter={e.git || undefined} ignored={dimmed(e)} />}</For>
      <For each={deleted()}>{(d) => <Line name={d.name} letter={d.letter} />}</For>
    </Show>
  );
}
