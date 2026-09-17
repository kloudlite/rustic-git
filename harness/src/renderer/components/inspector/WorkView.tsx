import { Show, createEffect, createResource, createSignal, onCleanup, type JSX } from "solid-js";
import { Segmented } from "../../ui/Segmented";
import * as live from "../../live";
import { Icon } from "../../ui/Icon";
import { Empty } from "../../ui/parts";
import { FileTree } from "./FileTree";
import { FsTree } from "./FsTree";
import { changeLetter, committedPaths } from "../../rows";
import { ChangeList } from "./ChangeList";
import { CommitList } from "./CommitList";
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
  /** Which tree of it: an agent session's own, absent for the workspace's own (spec §4.4). */
  tree?: string;
  packages?: Package[];
  inherited?: string;   // an ephemeral shows its source workspace's packages, read-only
  against: string;
  overview: JSX.Element;
  changeActions?: JSX.Element;
  onOpenFile: (path: string, status?: string) => void;
}) {
  const [tab, setTab] = createSignal<View>(((location.hash.split("/")[1] === "changes" ? "files" : location.hash.split("/")[1]) as View) || "overview");
  /**
   * The workspace's own changes, read when the tab is opened. `changes` from the model is the
   * fallback for a view with no tool server (an ephemeral's source, the fixtures); the TREE is
   * `FsTree`'s own business, one directory at a time.
   */
  const [diff] = createResource(
    () => (tab() === "files" && props.scope ? { scope: props.scope, tree: props.tree, v: live.fsChanged() } : undefined),
    (k) => live.fsChanges(k.scope, k.tree),
  );
  /**
   * The workspace's files follow its own watch while it is on show: the tree, the changes and the
   * file texts are patched as they change instead of being read again on every visit
   * (owner: "can't we use what VSCode is using to sync fs?").
   */
  createEffect(() => {
    const scope = props.scope;
    if (!scope) return;
    live.watchFs(scope);
    onCleanup(() => live.unwatchFs(scope));
  });
  /**
   * Which folders are open, by PATH, held here rather than in the rows: the tree refetches — on the
   * live poll, on a tab switch — and a fold whose state lived in the fetched data snapped shut
   * every time (owner, 2026-09-18). A Set is enough; the rows are redrawn from it.
   */
  /**
   * What this branch has COMMITTED, under what is still uncommitted. A person who had just
   * committed saw an empty CHANGES tab with no sign of where the work went (owner, 2026-09-18).
   */
  const [log] = createResource(
    () => (tab() === "files" && props.scope ? { scope: props.scope, tree: props.tree, v: live.fsChanged() } : undefined),
    (k) => live.fsLog(k.scope, 20, k.tree),
  );
  const commits = () => log()?.commits ?? [];
  const [open, setOpen] = createSignal(new Set<string>());
  const toggle = (path: string) =>
    setOpen((was) => {
      const next = new Set(was);
      if (!next.delete(path)) next.add(path);
      return next;
    });
  /**
   * The tool server's two porcelain columns become the one letter this app draws. It sends no
   * counts, so the +/− beside a row is only what a view with its own data (the fixtures) carries.
   */
  const changes = (): Change[] =>
    diff()?.changes?.length
      ? diff()!.changes.map((c) => ({ path: c.path, status: changeLetter(c) as Change["status"], add: 0, del: 0 }))
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
            {/* A clean tree is not an empty answer: say where it stands, so a person who has just
                committed sees that the commit is why there is nothing here (owner, 2026-09-18). */}
            <Empty>
              {diff() && diff()!.repo === false
                ? "Not a git repository."
                : diff()?.head
                  ? `No uncommitted changes · ${diff()!.branch ?? props.against} at ${diff()!.head!.slice(0, 7)}`
                  : `Nothing differs from ${props.against}.`}
            </Empty>
          </Show>
        </Fold>
        {/* The second section: what is already in, newest first. Folded away by default — a person
            reads what they have not committed first. */}
        <Show when={commits().length}>
          <Fold title="Committed this session" meta={<span class="text-subtle">{commits().length}</span>} closed>
            <CommitList commits={commits()} onOpen={props.onOpenFile} />
          </Fold>
        </Show>
        <Fold title="Files" meta={<span class="text-subtle">{props.against}</span>}>
          <div class="py-0.5">
            {/* A workspace reads its own tree from its tool server; a view with none (the fixtures,
                an ephemeral's source) keeps the static one. */}
            <Show when={props.scope} fallback={<Show when={props.files.length} fallback={<Empty>No files.</Empty>}><FileTree nodes={props.files} onOpen={props.onOpenFile} /></Show>}>
              {(scope) => (
                <FsTree
                  scope={scope()}
                  tree={props.tree}
                  open={open()}
                  changes={diff()?.changes}
                  committed={committedPaths(commits())}
                  onToggle={toggle}
                  onOpen={props.onOpenFile}
                />
              )}
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
function Fold(props: { title: string; meta?: JSX.Element; actions?: JSX.Element; closed?: boolean; children: JSX.Element }) {
  const [open, setOpen] = createSignal(!props.closed);
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
