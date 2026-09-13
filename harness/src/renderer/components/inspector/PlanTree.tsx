import { For, Show, createSignal, type JSX } from "solid-js";
import { Row, Gutter } from "../../ui/parts";
import { Icon } from "../../ui/Icon";
import { TODO } from "../status";
import type { Todo, TodoState } from "../../model";

/** Leaves are the unit of progress: a group is done when its children are. */
export function leaves(ts: Todo[]): Todo[] {
  return ts.flatMap((t) => (t.children?.length ? leaves(t.children) : [t]));
}

export function stateOf(t: Todo): TodoState {
  if (!t.children?.length) return t.state ?? "pending";
  const ss = t.children.map(stateOf);
  if (ss.every((s) => s === "done")) return "done";
  if (ss.some((s) => s === "active")) return "active";
  if (ss.some((s) => s === "blocked")) return "blocked";
  return "pending";
}

/**
 * The plan as the workbench draws a tree: 22px rows, a chevron in the first
 * gutter for a group, a state mark for a step, indent guides down each level,
 * the count at the row's end. Nothing here is a card; it is the same tree the
 * side bar uses, so the eye reads it without learning a second shape.
 */
export function PlanTree(props: { todos: Todo[]; depth?: number }) {
  const depth = () => props.depth ?? 0;
  const indent = () => 12 + depth() * 16;
  const guide = (): JSX.CSSProperties => ({ "padding-left": `${indent()}px` });

  return (
    <For each={props.todos}>
      {(t) => {
        const st = () => stateOf(t);
        const branch = () => !!t.children?.length;
        const [open, setOpen] = createSignal(st() !== "done");
        const done = () => leaves(t.children ?? []).filter((l) => l.state === "done").length;
        const total = () => leaves(t.children ?? []).length;
        const dim = () => st() === "done" || st() === "pending";

        return (
          <Show when={branch()} fallback={<Step todo={t} state={st()} depth={depth()} />}>
            <Row class="min-h-5.5 py-0.5 pr-3" style={guide()} onClick={() => setOpen(!open())}>
              <Gutter><Icon name={open() ? "chevronDown" : "chevronRight"} size={16} class="text-muted" /></Gutter>
              <span class="min-w-0 flex-1 px-1 leading-[18px] whitespace-normal wrap-words" classList={{ "text-muted": dim() }}>{t.text}</span>
              <span class="shrink-0 font-mono text-xs tabular-nums text-subtle">{done()}/{total()}</span>
            </Row>
            <Show when={open()}>
              <Guides depth={depth() + 1}>
                <PlanTree todos={t.children!} depth={depth() + 1} />
              </Guides>
            </Show>
          </Show>
        );
      }}
    </For>
  );
}

/** One guide per level, at the chevron's centre, drawn behind the rows. */
function Guides(props: { depth: number; children: JSX.Element }) {
  return (
    <div class="relative">
      <For each={Array.from({ length: props.depth })}>
        {(_, i) => <span class="pointer-events-none absolute top-0 bottom-0 w-px bg-guide" style={{ left: `${12 + i() * 16 + 8}px` }} />}
      </For>
      {props.children}
    </div>
  );
}

/** One step: a state mark in the gutter, the text, the agent and note after it. */
function Step(props: { todo: Todo; state: TodoState; depth: number }) {
  const t = () => props.todo;
  const dim = () => props.state === "done" || props.state === "pending";
  return (
    <Row class="min-h-5.5 items-start py-0.5 pr-3" style={{ "padding-left": `${12 + props.depth * 16}px` }} title={`${TODO[props.state].label}${t().note ? ` · ${t().note}` : ""}`}>
      <Gutter>
        <Show when={props.state === "done"} fallback={<span class={`size-1.5 rounded-full ${TODO[props.state].dot}`} />}>
          <Icon name="check" size={16} class="text-subtle" />
        </Show>
      </Gutter>
      <span class="min-w-0 flex-1 px-1 leading-[18px] whitespace-normal wrap-words" classList={{ "text-muted": dim() }}>{t().text}</span>
      <Show when={t().eph}>{(id) => <span class="shrink-0 pt-px font-mono text-xs leading-[18px] text-accent">{id()}</span>}</Show>
    </Row>
  );
}
