import { Show, createSignal, type JSX } from "solid-js";
import { Segmented } from "../../ui/Segmented";
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
  packages?: Package[];
  inherited?: string;   // an ephemeral shows its source workspace's packages, read-only
  against: string;
  overview: JSX.Element;
  changeActions?: JSX.Element;
  onOpenFile: (path: string, status?: string) => void;
}) {
  const [tab, setTab] = createSignal<View>(((location.hash.split("/")[1] === "changes" ? "files" : location.hash.split("/")[1]) as View) || "overview");
  const sum = () => totals(props.changes);

  return (
    <>
      <Segmented
        value={tab()}
        onChange={setTab}
        items={[
          { value: "overview", label: "Overview" },
          { value: "files", label: "Files", count: props.changes.length || undefined },
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
          meta={<Show when={props.changes.length}><span class="text-created">+{sum().add}</span> <span class="text-deleted">−{sum().del}</span></Show>}
          actions={<Show when={props.changes.length}>{props.changeActions}</Show>}
        >
          <Show when={props.changes.length === 0} fallback={<ChangeList changes={props.changes} onOpen={props.onOpenFile} />}>
            <Empty>Nothing differs from {props.against}.</Empty>
          </Show>
        </Fold>
        <Fold title="Files" meta={<span class="text-subtle">{props.against}</span>}>
          <div class="py-0.5"><FileTree nodes={props.files} onOpen={props.onOpenFile} /></div>
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
