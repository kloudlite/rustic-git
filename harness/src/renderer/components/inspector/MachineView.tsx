import { Show, createSignal } from "solid-js";
import { Segmented } from "../../ui/Segmented";
import { Heading, Empty } from "../../ui/parts";
import { Button } from "../../ui/Button";
import * as live from "../../live";
import { PlanTree, leaves } from "./PlanTree";
import { Queue } from "./Inspector";
import type { Machine } from "../../model";

/**
 * What the machine is working on, how far along it is, and the plan it is
 * through. Workspaces and agents are not repeated here: the left panel is the
 * one place they are listed.
 */
export function MachineView(props: { machine: Machine; session: string; onOpenShell: (scope: string) => void }) {
  // This session's exchanges with the workspaces, as ONE queue in the order
  // they happened — each row names its workspace; grouping by workspace would
  // hide the order, which is what a queue is.
  const queue = () =>
    props.machine.workspaces
      .flatMap((w) => w.queue.filter((x) => x.session === props.session).map((x) => ({ ...x, workspace: w.name })))
      .sort((a, b) => a.at.localeCompare(b.at));
  const openCount = () => queue().filter((x) => x.state !== "done").length;
  const [tab, setTab] = createSignal<"overview" | "queue">("overview");
  const all = () => leaves(props.machine.todos);
  const done = () => all().filter((t) => t.state === "done").length;
  const total = () => all().length;
  const pct = () => (total() ? Math.round((done() / total()) * 100) : 0);
  const active = () => all().find((t) => t.state === "active");

  return (
    <>
      <Segmented
        value={tab()}
        onChange={setTab}
        items={[
          { value: "overview", label: "Overview" },
          { value: "queue", label: "Queue", count: openCount() || undefined },
        ]}
      />
      <Show when={tab() === "queue"}>
        <Show when={queue().length} fallback={<Empty>This session has not sent anything to a workspace yet.</Empty>}>
          <Queue queue={queue()} bare />
        </Show>
      </Show>
      <Show when={tab() === "overview"}>
      <Heading class="mt-1 border-t-0" actions={<Button variant="ghost" size="sm" icon="terminal" title="Open a shell on this machine (⌘J)" onClick={() => props.onOpenShell("machine")}>Shell</Button>}>
        Goal
      </Heading>
      <p class="m-0 px-5 pt-1 pb-3 leading-[20px] wrap-words">
        <Show when={props.machine.goal} fallback={<span class="text-subtle">No goal yet. The first message sets it.</span>}>
          {props.machine.goal}
        </Show>
      </p>

      {/* What this session has spent: opencode's Context block, from pi's own usage. */}
      <Show when={live.thread(props.session).spend().tokens}>
        <Heading>Context</Heading>
        <div class="flex flex-col gap-0.5 px-5 pt-1 pb-3 font-mono text-xs text-muted">
          <div class="tabular-nums">{live.thread(props.session).spend().tokens.toLocaleString()} tokens</div>
          <Show when={live.thread(props.session).spend().context}>
            {(w) => <div class="tabular-nums">{Math.min(100, Math.round((live.thread(props.session).spend().tokens / w()) * 100))}% used</div>}
          </Show>
          <Show when={live.thread(props.session).spend().cost}>{(c) => <div class="tabular-nums">${c().toFixed(2)} spent</div>}</Show>
        </div>
      </Show>

      {/* Progress is the plan's own headline, not a section of its own. */}
      <Heading meta={`${done()}/${total()} done`}>Plan</Heading>
      {/* progressBar.background: a 2px rule, the way the workbench shows progress. */}
      <div class="mx-5 mt-1.5 mb-2.5 h-0.5 bg-active">
        <div class="h-full bg-focus" style={{ width: `${pct()}%` }} />
      </div>
      <Show when={active()}>
        {(t) => (
          <div class="flex items-start gap-2 px-5 pb-3 leading-[20px]">
            <span class="shrink-0 pt-px text-xs text-subtle">now</span>
            <span class="min-w-0 wrap-words">{t().text}</span>
          </div>
        )}
      </Show>
      <Show when={total() === 0}><Empty>Nothing planned yet.</Empty></Show>
      <div class="pb-4">
        <PlanTree todos={props.machine.todos} />
      </div>
      </Show>
    </>
  );
}
