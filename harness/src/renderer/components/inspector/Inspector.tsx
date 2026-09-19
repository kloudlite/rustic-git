import { For, Show, createMemo, createSignal } from "solid-js";
import { Icon } from "../../ui/Icon";
import * as live from "../../live";
import { Heading, Field, Empty } from "../../ui/parts";
import { sessionOf } from "../../rows";
import { Badge } from "../../ui/Badge";
import { Button } from "../../ui/Button";
import { AGENT } from "../status";
import { FileDiff } from "../results/FileDiff";
import { patchFiles } from "../results/diff";
import { MachineView } from "./MachineView";
import { Tasks } from "./Tasks";
import { operationStore, type OperationProjection } from "../../operations/index.ts";
import { Processes } from "./Processes";
import { WorkView, totals } from "./WorkView";
import type { Ephemeral, Exchange, Machine, Workspace } from "../../model";

/** Whatever the left panel has selected, shown in full. */
export function Inspector(props: {
  machine: Machine;
  selected: string;
  onOpenShell: (scope: string) => void;
  onOpenTask: (id: string) => void;
  onOpenOperation?: (projection: OperationProjection) => void;
  onOpenFile: (path: string, status?: string) => void;
  /** Which tree an agent session works in, by session id — the bench's answer, not a guess. */
  treeOf?: (session: string) => string | undefined;
}) {
  const found = createMemo<{ ws?: Workspace; eph?: Ephemeral }>(() => {
    for (const ws of props.machine.workspaces) {
      if (ws.id === props.selected) return { ws };
      const eph = ws.ephemerals.find((e) => e.id === props.selected);
      if (eph) return { ws, eph };
    }
    return {};
  });

  // The bench row is session 1 ("bench"); a session tab is its own id; a
  // fork shows the bench's view.
  const session = () => (/^s-\d+$/.test(props.selected) ? props.selected : "bench");
  /** Whose processes these are: a workspace tab has its own session, and its own dev server. */
  const procSession = () =>
    found().eph ? sessionOf({ kind: "ephemeral", id: found().eph!.id }) : found().ws ? sessionOf({ kind: "workspace", id: found().ws!.id }) : sessionOf({ kind: /^s-\d+$/.test(props.selected) ? "session" : "bench", id: props.selected });

  /** The workspace whose processes and tasks these are: a clone is its own, the bench is one. */
  const procWorkspace = () => found().eph?.id ?? found().ws?.id ?? "bench";

  return (
    <aside class="min-h-0 overflow-x-hidden overflow-y-auto border-l border-line bg-panel pb-4">
      {/* The tasks of THIS thread, as the processes below are: the bench session's rows appeared
          under a workspace tab (owner, 2026-09-17). Same session, same rule. */}
      {/* Both belong to the WORKSPACE this thread's work runs in: every bench session shares the
          bench's machine, a workspace's sessions share its own (owner, 2026-09-17). */}
      <Tasks onOpen={props.onOpenTask} onOpenOperation={props.onOpenOperation} session={procSession()} workspace={procWorkspace()} operations={() => {
        operationStore()?.entries();
        return operationStore()?.taskRows(procSession(), procWorkspace()) ?? [];
      }} />
      <Processes onOpen={props.onOpenTask} session={procSession()} workspace={procWorkspace()} />
      <Show when={!found().ws}>
        <MachineView machine={props.machine} session={session()} onOpenShell={props.onOpenShell} />
      </Show>
      <Show when={found().ws && !found().eph}>
        <WorkspaceView ws={found().ws!} onOpenShell={props.onOpenShell} onOpenFile={props.onOpenFile} />
      </Show>
      <Show when={found().eph}>
        <EphemeralView eph={found().eph!} ws={found().ws!} tree={props.treeOf?.(found().eph!.id)} onOpenFile={props.onOpenFile} />
      </Show>
    </aside>
  );
}

/**
 * What passed between the machine and this workspace, newest last: an arrow
 * says which way, the tone says whether it is done, being worked, or waiting.
 */
export function Queue(props: { queue: (Exchange & { workspace?: string })[]; bare?: boolean }) {
  const open = () => props.queue.filter((x) => x.state !== "done").length;
  return (
    <>
      <Show when={!props.bare}><Heading meta={open() ? `${open()} open` : undefined}>Queue</Heading></Show>
      <Show when={props.queue.length} fallback={<Empty>Nothing asked of this workspace yet.</Empty>}>
        <div class="flex flex-col px-5 pt-1 pb-4">
          <For each={props.queue}>
            {(x) => (
              <div class="flex gap-2 py-1.5 leading-[18px]" classList={{ "text-subtle": x.state === "done" }}>
                <span class="mt-0.5 shrink-0" classList={{ "text-accent": x.dir === "in" && x.state !== "done", "text-success": x.dir === "out" && x.state !== "done", "text-subtle": x.state === "done" }}>
                  <Icon name={x.dir === "in" ? "arrowDownLeft" : "arrowUpRight"} size={12} />
                </span>
                <span class="min-w-0 flex-1 wrap-words">
                  <Show when={x.workspace}>{(w) => <span class="mr-1.5 font-mono text-xs text-muted">{w()}</span>}</Show>
                  {x.text}
                  <span class="ml-1.5 font-mono text-xs text-subtle">{x.at}</span>
                  {/* Which session asked, once there is more than one to tell apart. */}
                  <Show when={live.sessionCount() > 1}><span class="ml-1.5 font-mono text-xs text-subtle">· {x.session}</span></Show>
                </span>
                <Show when={x.state === "working"}><span class="mt-1.5 size-1.5 shrink-0 rounded-full bg-success" /></Show>
                <Show when={x.state === "pending"}><span class="mt-1.5 size-1.5 shrink-0 rounded-full bg-warning" /></Show>
              </div>
            )}
          </For>
        </div>
      </Show>
    </>
  );
}

function WorkspaceView(props: { ws: Workspace; onOpenShell: (scope: string) => void; onOpenFile: (path: string, status?: string) => void }) {
  const t = () => totals(props.ws.changes);
  return (
    <WorkView
      scope={props.ws.id}
      files={props.ws.files}
      changes={props.ws.changes}
      packages={props.ws.packages}
      against={props.ws.branch}
      onOpenFile={props.onOpenFile}
      overview={
        <>
          <Heading
            class="mt-1 border-t-0"
            actions={
              <>
                <Button variant="ghost" size="sm" icon="terminal" title="Open a shell in this workspace" onClick={() => props.onOpenShell(props.ws.id)}>Shell</Button>
                <Button variant="ghost" size="sm" title={props.ws.state === "running" ? "Stop this workspace" : "Start this workspace"}>{props.ws.state === "running" ? "Stop" : "Start"}</Button>
              </>
            }
          >
            Workspace
          </Heading>
          <Field label="repo" mono>{props.ws.repo}</Field>
          <Field label="branch" mono>{props.ws.branch}</Field>
          <Field label="state">
            <Badge tone={props.ws.state === "running" ? "success" : "neutral"}>{props.ws.state}</Badge>
          </Field>
          <Field label="changes" mono>
            <span class="text-created">+{t().add}</span> <span class="text-deleted">−{t().del}</span>
            <span class="text-muted"> · {props.ws.changes.length} {props.ws.changes.length === 1 ? "file" : "files"}</span>
          </Field>
          <Queue queue={props.ws.queue.filter((x) => !live.discarded().has(x.session))} />
        </>
      }
      changeActions={
        <>
          <Button variant="icon" icon="diff" title="Diff all" />
          <Button variant="icon" icon="x" title="Discard every change" />
        </>
      }
    />
  );
}

/**
 * An agent's copy, watched rather than driven: it is given a task, it works, and
 * it folds the result back into the workspace it was cut from itself. Nothing
 * here is a control.
 */
function EphemeralView(props: { eph: Ephemeral; ws: Workspace; tree?: string; onOpenFile: (path: string, status?: string) => void }) {
  const t = () => totals(props.eph.changes);
  const st = () => AGENT[props.eph.state];
  const tone = () =>
    props.eph.state === "running"
      ? "success"
      : props.eph.state === "failed"
        ? "danger"
        : props.eph.state === "waiting"
          ? "warning"
          : "neutral";
  /**
   * What this agent has that main does not, asked for by a click (spec §4.5). A read-only
   * `git diff main...HEAD` inside its tree: the one thing a person needs before deciding whether
   * the work is mergeable, and the reason a tree outlives its report.
   */
  const [against, setAgainst] = createSignal<{ diff?: string; error?: string } | "asking" | undefined>();
  const showDiff = async () => {
    if (!props.tree) return;
    setAgainst("asking");
    setAgainst((await live.fsAgainstMain(props.ws.id, props.tree)) ?? { error: "the workspace did not answer" });
  };

  return (
    <WorkView
      // Its files are the WORKSPACE's tool server, confined to its own tree: the same pod, one
      // working directory further in. Before trees there was no scope here at all and the tab was
      // empty.
      scope={props.ws.id}
      tree={props.tree}
      files={props.eph.files ?? props.ws.files}
      changes={props.eph.changes}
      packages={props.ws.packages}
      inherited={props.ws.name}
      against={`${props.ws.name} @ ${props.ws.branch}`}
      changeActions={
        <Show when={props.tree}>
          <Button variant="ghost" size="sm" title="What this agent has that main does not" onClick={showDiff}>Diff against main</Button>
        </Show>
      }
      onOpenFile={props.onOpenFile}
      overview={
        <>
          <Heading class="mt-1 border-t-0">Agent</Heading>
          <p class="m-0 px-5 pt-2 pb-4 leading-[20px] wrap-words">{props.eph.task}</p>
          <Field label="state"><Badge tone={tone()}>{st().label}</Badge></Field>
          <Show when={props.eph.step}>{(step) => <Field label="now">{step()}</Field>}</Show>
          <Show when={props.eph.outcome}>
            {(o) => (
              <>
                <Field label="outcome" mono>{o().summary}</Field>
                <Field label="finished">{o().at}</Field>
              </>
            )}
          </Show>
          <Field label="role">{props.eph.agent}</Field>
          <Field label="copy" mono>{props.eph.id}</Field>
          <Field label="cut from" mono>{props.ws.name} @ {props.ws.branch}</Field>
          <Field label="started">{props.eph.started}</Field>
          <Field label="changes" mono>
            <span class="text-created">+{t().add}</span> <span class="text-deleted">−{t().del}</span>
            <span class="text-muted"> · {props.eph.changes.length} {props.eph.changes.length === 1 ? "file" : "files"}</span>
          </Field>
          <Show when={against()}>
            {(got) => (
              <>
                <Heading>Against main</Heading>
                <Show when={got() !== "asking"} fallback={<Empty>reading…</Empty>}>
                  <Show
                    when={(got() as { diff?: string }).diff?.trim()}
                    fallback={<Empty tone={(got() as { error?: string }).error ? "danger" : undefined}>{(got() as { error?: string }).error ?? "Nothing this branch has that main does not."}</Empty>}
                  >
                    {(text) => (
                      <div class="px-5 pt-1 pb-4">
                        <For each={patchFiles(text())}>{(f) => <FileDiff file={f} />}</For>
                      </div>
                    )}
                  </Show>
                </Show>
              </>
            )}
          </Show>
        </>
      }
    />
  );
}
