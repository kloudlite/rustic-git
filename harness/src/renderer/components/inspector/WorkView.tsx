import { Show, createResource, createSignal, type JSX } from "solid-js";
import { Segmented } from "../../ui/Segmented";
import * as live from "../../live";
import { Icon } from "../../ui/Icon";
import { Empty } from "../../ui/parts";
import { FileTree } from "./FileTree";
import { ChangeList } from "./ChangeList";
import { PackageList } from "./PackageList";
import type { Package, Change, FileNode } from "../../model";

export type View = "overview" | "files" | "packages";

export function totals(cs: Change[]) {
  return cs.reduce((a, c) => ({ add: a.add + c.add, del: a.del + c.del }), { add: 0, del: 0 });
}

/**
 * A workspace and an ephemeral are both working copies, so both get the same
 * three views. Only the overview differs, which is why it is passed in.
 */
export function WorkView(props: {
  files: FileNode[];
  changes: Change[];
  /** The workspace whose tool server holds the files; absent for a view with none. */
  scope?: string;
  packages?: Package[];
  inherited?: string;   // an ephemeral shows its source workspace's packages, read-only
  against: string;
  overview: JSX.Element;
  changeActions?: JSX.Element;
  onOpenFile: (path: string, status?: string) => void;
}) {
  const [tab, setTab] = createSignal<View>(((location.hash.split("/")[1] === "changes" ? "files" : location.hash.split("/")[1]) as View) || "overview");
  /**
   * The workspace's own files, read when the tab is opened. `files`/`changes` from the model are
   * the fallback for a view that has no tool server (an ephemeral's source, the fixtures).
   */
  const [tree] = createResource(() => (tab() === "files" && props.scope ? props.scope : undefined), async (scope) => (await live.fsTree(scope))?.entries ?? []);
  const [diff] = createResource(() => (tab() === "files" && props.scope ? props.scope : undefined), (scope) => live.fsChanges(scope));
  const nodes = (): FileNode[] =>
    tree()?.length ? tree()!.map((e) => ({ name: e.name, dir: e.dir, children: e.dir ? [] : undefined })) : props.files;
  const changes = (): Change[] =>
    diff()?.changes?.length
      ? diff()!.changes.map((c) => ({ path: c.path, status: (c.status ?? "M") as Change["status"], add: c.add ?? 0, del: c.del ?? 0 }))
      : props.changes;
  const sum = () => totals(changes());

  return (
    <>
      <Segmented
        value={tab()}
        onChange={setTab}
        items={[
          { value: "overview", label: "Overview" },
          { value: "files", label: "Files", count: changes().length || undefined },
          ...(props.packages ? [{ value: "packages" as const, label: "Packages", count: props.packages.length || undefined }] : []),
        ]}
      />
      <Show when={tab() === "overview"}>{props.overview}</Show>
      {/* One tab, two sections, the way an editor's explorer shows source
          control above the tree: what changed is the first thing read, the
          tree is where everything else is found. Each section folds. */}
      <Show when={tab() === "files"}>
        <Fold
          title="Changes"
          meta={<Show when={changes().length}><span class="text-created">+{sum().add}</span> <span class="text-deleted">−{sum().del}</span></Show>}
          actions={<Show when={changes().length}>{props.changeActions}</Show>}
        >
          <Show when={changes().length === 0} fallback={<ChangeList changes={changes()} onOpen={props.onOpenFile} />}>
            {/* A workspace that is not a git repository has nothing to differ FROM: saying
                "nothing differs from ." was the tool server's `repo: false` read as a branch. */}
            <Empty>{diff() && diff()!.repo === false ? "Not a git repository." : `Nothing differs from ${props.against}.`}</Empty>
          </Show>
        </Fold>
        <Fold title="Files" meta={<span class="text-subtle">{props.against}</span>}>
          <div class="py-0.5">
            <Show when={nodes().length} fallback={<Empty>{tree.loading ? "reading…" : "No files."}</Empty>}>
              <FileTree nodes={nodes()} onOpen={props.onOpenFile} />
            </Show>
          </div>
        </Fold>
      </Show>
      <Show when={tab() === "packages" && props.packages}>
        {(pkgs) => <PackageList packages={pkgs()} inherited={props.inherited} />}
      </Show>
    </>
  );
}

/** A section of the explorer: a header that folds it, its meta on the right. */
function Fold(props: { title: string; meta?: JSX.Element; actions?: JSX.Element; children: JSX.Element }) {
  const [open, setOpen] = createSignal(true);
  return (
    <section class="group/fold border-b border-line-subtle last:border-b-0">
      <div class="flex h-6 items-center gap-1 pr-2 pl-2">
        <button
          class="flex h-full min-w-0 flex-1 items-center gap-1 text-left text-2xs font-semibold uppercase text-muted hover:text-fg"
          onClick={() => setOpen((v) => !v)}
          aria-expanded={open()}
        >
          <Icon name={open() ? "chevronDown" : "chevronRight"} size={11} class="text-subtle" />
          <span>{props.title}</span>
          <span class="ml-1 font-mono text-2xs font-normal normal-case tracking-normal tabular-nums">{props.meta}</span>
        </button>
        <span class="flex items-center gap-0.5 opacity-0 group-hover/fold:opacity-100 focus-within:opacity-100">{props.actions}</span>
      </div>
      <Show when={open()}>{props.children}</Show>
    </section>
  );
}
