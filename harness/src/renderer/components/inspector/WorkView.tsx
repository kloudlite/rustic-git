import { Show, createSignal, type JSX } from "solid-js";
import { Segmented } from "../../ui/Segmented";
import { Empty } from "../../ui/parts";
import { FileTree } from "./FileTree";
import { ChangeList } from "./ChangeList";
import type { Change, FileNode } from "../../model";

export type View = "overview" | "files" | "changes";

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
  against: string;
  overview: JSX.Element;
  changeActions?: JSX.Element;
  onOpenFile: (path: string, status?: string) => void;
}) {
  const [tab, setTab] = createSignal<View>((location.hash.split("/")[1] as View) || "overview");

  return (
    <>
      <Segmented
        value={tab()}
        onChange={setTab}
        items={[
          { value: "overview", label: "Overview" },
          { value: "files", label: "Files" },
          { value: "changes", label: "Changes", count: props.changes.length || undefined },
        ]}
      />
      <Show when={tab() === "overview"}>{props.overview}</Show>
      <Show when={tab() === "files"}>
        <div class="pt-1.5 pb-2"><FileTree nodes={props.files} onOpen={props.onOpenFile} /></div>
      </Show>
      <Show when={tab() === "changes"}>
        <Show when={props.changes.length === 0} fallback={<ChangeList changes={props.changes} onOpen={props.onOpenFile} />}>
          <Empty>Nothing differs from {props.against}.</Empty>
        </Show>
        <Show when={props.changes.length}>{props.changeActions}</Show>
      </Show>
    </>
  );
}
