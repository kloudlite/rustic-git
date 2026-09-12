import { Show } from "solid-js";
import { Heading, Empty } from "../../ui/parts";
import { Button } from "../../ui/Button";
import { PlanTree, leaves } from "./PlanTree";
import type { Machine } from "../../model";

/**
 * What the machine is working on, how far along it is, and the plan it is
 * through. Workspaces and agents are not repeated here: the left panel is the
 * one place they are listed.
 */
export function MachineView(props: { machine: Machine; onOpenShell: (scope: string) => void }) {
  const all = () => leaves(props.machine.todos);
  const done = () => all().filter((t) => t.state === "done").length;
  const total = () => all().length;
  const pct = () => (total() ? Math.round((done() / total()) * 100) : 0);
  const active = () => all().find((t) => t.state === "active");

  return (
    <>
      <Heading actions={<Button variant="ghost" size="sm" icon="terminal" title="Open a shell on this machine (⌘J)" onClick={() => props.onOpenShell("machine")}>Shell</Button>}>
        Goal
      </Heading>
      <p class="mx-3 mt-1 mb-3 rounded-r-md border-l-2 border-accent bg-bg px-2.5 py-2 text-sm leading-relaxed wrap-words">
        <Show when={props.machine.goal} fallback={<span class="text-subtle">No goal yet. The first message sets it.</span>}>
          {props.machine.goal}
        </Show>
      </p>

      {/* Progress is the plan's own headline, not a section of its own. */}
      <Heading meta={`${done()}/${total()} done`}>Plan</Heading>
      <div class="flex items-center gap-2.5 px-3 pt-0.5 pb-1.5">
        <div class="h-1 flex-1 overflow-hidden rounded-full bg-active">
          <div class="h-full rounded-full bg-success transition-[width] duration-200 ease-out-quick" style={{ width: `${pct()}%` }} />
        </div>
      </div>
      <Show when={active()}>
        {(t) => (
          <div class="flex items-baseline gap-2 px-3 pb-2 text-sm">
            <span class="shrink-0 text-2xs tracking-[0.05em] uppercase text-subtle">now</span>
            <span class="min-w-0 truncate">{t().text}</span>
          </div>
        )}
      </Show>
      <Show when={total() === 0}><Empty>Nothing planned yet.</Empty></Show>
      <div class="px-3 pt-1 pb-2">
        <PlanTree todos={props.machine.todos} />
      </div>
    </>
  );
}
