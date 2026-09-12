import { For, Show, createSignal } from "solid-js";
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
 * The plan, as a list of groups rather than a drawn graph. A group is a heading
 * with its own progress; its steps hang off one guide line, the same shape the
 * workspace tree uses on the left. An earlier version drew commit-graph rails,
 * which cost a lot of CSS to say what an indent already says.
 *
 * Nesting stops mattering below the second level, so deeper groups indent but
 * keep the same shape.
 */
export function PlanTree(props: { todos: Todo[]; depth?: number }) {
  const depth = () => props.depth ?? 0;

  return (
    <For each={props.todos}>
      {(t) => {
        const st = () => stateOf(t);
        const branch = () => !!t.children?.length;
        const [open, setOpen] = createSignal(st() !== "done");
        const done = () => leaves(t.children ?? []).filter((l) => l.state === "done").length;
        const total = () => leaves(t.children ?? []).length;

        return (
          <Show when={branch()} fallback={<Step todo={t} state={st()} />}>
            <section class="mt-3 first:mt-0">
              <button
                class="group flex w-full items-center gap-2 py-0.5 text-left"
                onClick={() => setOpen(!open())}
                aria-expanded={open()}
              >
                <Icon
                  name={open() ? "chevronDown" : "chevronRight"}
                  size={11}
                  class="shrink-0 text-subtle group-hover:text-fg"
                />
                <span
                  class="min-w-0 flex-1 truncate text-sm font-medium"
                  classList={{ "text-muted": st() === "done" || st() === "pending" }}
                >
                  {t.text}
                </span>
                <span class="shrink-0 font-mono text-xs tabular-nums text-subtle">
                  {done()}/{total()}
                </span>
              </button>

              <Show when={open()}>
                <div class="mt-1 ml-[5px] border-l border-line pl-3">
                  <PlanTree todos={t.children!} depth={depth() + 1} />
                </div>
              </Show>
            </section>
          </Show>
        );
      }}
    </For>
  );
}

/** One step: a state mark, the text, and what is carrying it underneath. */
function Step(props: { todo: Todo; state: TodoState }) {
  const t = () => props.todo;
  return (
    <div class="flex gap-2 py-[3px]">
      <span class="flex h-5 w-3.5 shrink-0 items-center justify-center" title={TODO[props.state].label}>
        <Show
          when={props.state === "done"}
          fallback={<span class={`size-1.5 rounded-full ${TODO[props.state].dot}`} />}
        >
          <Icon name="check" size={12} class="text-subtle" />
        </Show>
      </span>
      <div class="min-w-0 flex-1">
        <div
          class="text-sm leading-5 wrap-words"
          classList={{
            "text-muted": props.state === "done" || props.state === "pending",
            "font-medium": props.state === "active",
          }}
        >
          {t().text}
        </div>
        <Show when={t().eph || t().note}>
          <div class="flex min-w-0 items-baseline gap-1.5 truncate text-xs leading-4 text-subtle">
            <Show when={t().eph}>{(id) => <span class="font-mono text-accent">{id()}</span>}</Show>
            <Show when={t().eph && t().note}><span class="text-line">·</span></Show>
            <Show when={t().note}>{(n) => <span class="truncate">{n()}</span>}</Show>
          </div>
        </Show>
      </div>
    </div>
  );
}
