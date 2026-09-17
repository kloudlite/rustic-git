import { For, Show, createEffect, createSignal, type JSX } from "solid-js";
import { Icon } from "../../ui/Icon";
import { Spinner } from "../Motion";

/**
 * The row chassis every tool uses, transcribed from opencode's `basic-tool.tsx:196`: an icon, a
 * title that shimmers while the call is pending, a subtitle, at most three `k=v` args, an optional
 * action on the right, and a collapse arrow that appears only when there is something to open.
 *
 * The rule that matters: a PENDING row cannot be expanded (`:180`), because there is nothing there
 * yet — except a shell, which streams its output as it runs (`allowOpenWhilePending`).
 */
export function BasicTool(props: {
  icon: string;
  title: string;
  subtitle?: string;
  args?: string[];
  action?: JSX.Element;
  pending?: boolean;
  failed?: boolean;
  allowOpenWhilePending?: boolean;
  defaultOpen?: boolean;
  children?: JSX.Element;
}) {
  const [open, setOpen] = createSignal(!!props.defaultOpen);
  // A failure opens itself: it is the row a person was about to click anyway.
  createEffect(() => props.failed && setOpen(true));
  const canOpen = () => !!props.children && (!props.pending || !!props.allowOpenWhilePending);
  return (
    <div data-component="basic-tool" class="flex flex-col">
      <button
        data-slot="basic-tool-tool-info-structured"
        class="group flex w-full items-baseline gap-2 py-px text-left"
        onClick={() => canOpen() && setOpen((v) => !v)}
      >
        <span class={`w-4 shrink-0 ${props.failed ? "text-danger" : props.pending ? "text-accent" : "text-success"}`}>
          <Show when={!props.pending} fallback={<Spinner />}>⏺</Show>
        </span>
        <span data-slot="basic-tool-tool-info-main" class="flex min-w-0 flex-1 items-baseline gap-2">
          {/* The title shimmers while it runs — a word that is still moving says "not finished". */}
          <span data-slot="basic-tool-tool-title" class="shrink-0 text-fg" classList={{ shimmer: props.pending }}>{props.title}</span>
          {/* Hidden while pending unless there is something to say (`:196`'s own condition). */}
          <Show when={(!props.pending || props.subtitle || props.args?.length) && props.subtitle}>
            {(s) => <span data-slot="basic-tool-tool-subtitle" class="min-w-0 truncate text-muted">{s()}</span>}
          </Show>
          <For each={props.args ?? []}>
            {(a) => <span data-slot="basic-tool-tool-arg" class="shrink-0 text-subtle">{a}</span>}
          </For>
        </span>
        <Show when={!props.pending && props.action}>
          <span data-slot="basic-tool-tool-action" class="shrink-0 text-subtle">{props.action}</span>
        </Show>
        <Show when={canOpen()}>
          <Icon name={open() ? "chevronDown" : "chevronRight"} size={14} class="shrink-0 text-subtle opacity-40 group-hover:opacity-100" />
        </Show>
      </button>
      <Show when={open() && props.children}>
        <div class="springy flex min-w-0">
          <span class="w-4 shrink-0 text-subtle">⎿</span>
          <div class="min-w-0 flex-1">{props.children}</div>
        </div>
      </Show>
    </div>
  );
}
