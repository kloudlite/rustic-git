import { Show, createMemo } from "solid-js";
import { Heading, Field, Empty } from "../../ui/parts";
import { Badge } from "../../ui/Badge";
import { Button } from "../../ui/Button";
import { AGENT } from "../status";
import { MachineView } from "./MachineView";
import { WorkView, totals } from "./WorkView";
import type { Ephemeral, Machine, Workspace } from "../../model";

/** Whatever the left panel has selected, shown in full. */
export function Inspector(props: {
  machine: Machine;
  selected: string;
  onOpenShell: (scope: string) => void;
  onOpenFile: (path: string, status?: string) => void;
}) {
  const found = createMemo<{ ws?: Workspace; eph?: Ephemeral }>(() => {
    for (const ws of props.machine.workspaces) {
      if (ws.id === props.selected) return { ws };
      const eph = ws.ephemerals.find((e) => e.id === props.selected);
      if (eph) return { ws, eph };
    }
    return {};
  });

  return (
    <aside class="min-h-0 overflow-x-hidden overflow-y-auto border-l border-line-subtle pr-2 pb-4">
      <Show when={!found().ws}>
        <MachineView machine={props.machine} onOpenShell={props.onOpenShell} />
      </Show>
      <Show when={found().ws && !found().eph}>
        <WorkspaceView ws={found().ws!} onOpenShell={props.onOpenShell} onOpenFile={props.onOpenFile} />
      </Show>
      <Show when={found().eph}>
        <EphemeralView eph={found().eph!} ws={found().ws!} onOpenFile={props.onOpenFile} />
      </Show>
    </aside>
  );
}

function WorkspaceView(props: { ws: Workspace; onOpenShell: (scope: string) => void; onOpenFile: (path: string, status?: string) => void }) {
  const t = () => totals(props.ws.changes);
  return (
    <WorkView
      files={props.ws.files}
      changes={props.ws.changes}
      against={props.ws.branch}
      onOpenFile={props.onOpenFile}
      overview={
        <>
          <Heading>Workspace</Heading>
          <Field label="repo" mono>{props.ws.repo}</Field>
          <Field label="branch" mono>{props.ws.branch}</Field>
          <Field label="state">
            <Badge tone={props.ws.state === "running" ? "success" : "neutral"}>{props.ws.state}</Badge>
          </Field>
          <Field label="changes" mono>
            <span class="text-created">+{t().add}</span> <span class="text-deleted">−{t().del}</span>
            <span class="text-muted"> in {props.ws.changes.length}</span>
          </Field>
          <div class="flex flex-wrap gap-1.5 px-3 pt-2 pb-1">
            <Button icon="terminal" onClick={() => props.onOpenShell(props.ws.id)}>Shell</Button>
            <Button variant="ghost">{props.ws.state === "running" ? "Stop" : "Start"}</Button>
          </div>
        </>
      }
      changeActions={
        <div class="flex flex-wrap gap-1.5 px-3 pt-2 pb-1">
          <Button icon="git">Diff all</Button>
          <Button variant="ghost">Discard</Button>
        </div>
      }
    />
  );
}

/**
 * An agent's copy, watched rather than driven: it is given a task, it works, and
 * it folds the result back into the workspace it was cut from itself. Nothing
 * here is a control.
 */
function EphemeralView(props: { eph: Ephemeral; ws: Workspace; onOpenFile: (path: string, status?: string) => void }) {
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

  return (
    <WorkView
      files={props.eph.files ?? props.ws.files}
      changes={props.eph.changes}
      against={`${props.ws.name} @ ${props.ws.branch}`}
      onOpenFile={props.onOpenFile}
      overview={
        <>
          <Heading>Agent</Heading>
          <p class="px-3 pt-1 pb-2 text-base leading-snug wrap-words">{props.eph.task}</p>
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
            <span class="text-muted"> in {props.eph.changes.length}</span>
          </Field>
          <Show when={props.eph.state === "failed"}>
            <Empty tone="danger">typecheck failed: Property 'build' does not exist on type 'Overview'</Empty>
          </Show>
        </>
      }
    />
  );
}
