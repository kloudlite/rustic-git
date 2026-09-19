import { For, Show, createSignal, onCleanup } from "solid-js";
import * as live from "../../live";
import { Icon } from "../../ui/Icon";
import { Heading } from "../../ui/parts";
import { procsOf } from "../../rows";
import type { OperationProjection, OperationTaskRow } from "../../operations/index.ts";

/**
 * Only what was sent to the background (^B): a command still running in the
 * thread is visible there, on its own row; a backgrounded one is the thing a
 * person must not lose sight of, so it lives at the top of the side bar
 * whatever is selected. One that finishes lingers dim for a few seconds,
 * then leaves; its log stays in the thread. A row is the tool and its
 * argument, the state and the clock; hover shows the cancel; click opens the log.
 */
export function Tasks(props: { onOpen: (id: string) => void; onOpenOperation?: (projection: OperationProjection) => void; session: string; workspace?: string; operations?: OperationTaskRow[] | (() => OperationTaskRow[]) }) {
  const [tick, setTick] = createSignal(Date.now());
  const timer = setInterval(() => setTick(Date.now()), 1000);
  onCleanup(() => clearInterval(timer));

  const clock = (t: live.Task) => {
    const s = Math.max(0, Math.round(((t.ended ?? tick()) - t.started) / 1000));
    return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${s % 60}s`;
  };
  // A task belongs to the WORKSPACE its work runs in: two bench sessions see the same ones.
  const mine = () => procsOf(live.tasks, props.session, props.workspace);
  const background = () => mine().filter((t) => t.state === "background");
  // Just ended after being backgrounded: shown dim for a few seconds, then gone.
  const settling = () => mine().filter((t) => t.n && t.ended && tick() - t.ended < 4000 && t.state !== "background" && t.state !== "lost");
  // Found gone when the bench restarted: listed dim, never as running.
  const lost = () => mine().filter((t) => t.state === "lost");

  return (
    <>
    <Show when={(typeof props.operations === "function" ? props.operations() : props.operations)?.length}>
      <Heading meta={`${(typeof props.operations === "function" ? props.operations() : props.operations)!.filter((row) => !row.ended).length} active`}>Background operations</Heading>
      <For each={typeof props.operations === "function" ? props.operations() : props.operations}>
        {(row) => (
          <button type="button" data-operation-id={row.id} class="group flex h-11 w-full cursor-pointer items-center gap-2 px-3 text-left hover:bg-hover" onClick={() => props.onOpenOperation?.(row.projection)}>
            <span class="flex w-4 shrink-0 items-center justify-center"><span class={`size-1.5 rounded-full ${row.state === "failed" ? "bg-danger" : row.ended ? "bg-subtle" : "bg-accent"}`} /></span>
            <div class="flex min-w-0 flex-1 flex-col">
              <span class="truncate font-mono text-sm font-bold text-fg-strong">Operate <span class="font-normal text-fg">{row.title}</span></span>
              <span class="text-xs capitalize text-subtle">{row.state}</span>
            </div>
          </button>
        )}
      </For>
      <div class="h-3" />
    </Show>
    <Show when={background().length || settling().length || lost().length}>
      <Heading meta={background().length ? `${background().length} running` : undefined}>Background tasks</Heading>
      <Group items={background()} tone="bg-accent animate-pulse" onOpen={props.onOpen} clock={clock} />
      <Group items={lost()} onOpen={props.onOpen} clock={clock} dim />
      <Group items={settling()} onOpen={props.onOpen} clock={clock} dim />
      <div class="h-3" />
    </Show>
    </>
  );
}

/** `{"header":"Clear stuck s…` is not a title. What a person can read is. */
const taskArg = (arg: string | undefined) => {
  const t = (arg ?? "").trim();
  if (!t.startsWith("{")) return t;
  try {
    const o = JSON.parse(t) as Record<string, unknown>;
    const said = ["command", "title", "name", "path", "task", "header", "query", "pattern"].map((k) => o[k]).find((v) => typeof v === "string" && v);
    return String(said ?? Object.keys(o).join(", "));
  } catch {
    return t.replace(/[{}"]/g, "").slice(0, 60);
  }
};

const DOT: Record<live.Task["state"], string> = { running: "bg-success", background: "bg-accent", done: "bg-subtle", failed: "bg-danger", cancelled: "bg-warning", lost: "bg-warning" };

function Group(props: { label?: string; items: live.Task[]; tone?: string; dim?: boolean; onOpen: (id: string) => void; clock: (t: live.Task) => string }) {
  return (
    <Show when={props.items.length}>
      <Show when={props.label}>
        <div class="flex h-5.5 items-center px-5 text-xs text-subtle">{props.label}</div>
      </Show>
      <For each={props.items}>
        {(t) => (
          <div
            class="group flex h-11 cursor-pointer items-center gap-2 px-3 hover:bg-hover"
            classList={{ "opacity-60": props.dim }}
            onClick={() => props.onOpen(t.id)}
            title="Open the log"
          >
            <span class="flex w-4 shrink-0 items-center justify-center"><span class={`size-1.5 rounded-full ${props.tone ?? DOT[t.state]}`} /></span>
            <div class="flex min-w-0 flex-1 flex-col">
              <span class="truncate font-mono text-sm leading-[18px]">
                {/* A readable title: the verb and what it is on, never the raw JSON a tool was
                    called with (owner, 2026-09-17). */}
                <span class="font-bold text-fg-strong">{t.tool}</span> <span class="text-fg">{taskArg(t.arg)}</span>
              </span>
              <span class="flex items-center gap-1.5 text-xs leading-4 text-subtle">
                <Show when={t.n}>{(n) => <span class="font-mono">#{n()}</span>}</Show>
                <Show when={live.sessionCount() > 1}><span class="font-mono">{t.session}</span><span>·</span></Show>
                <span class="capitalize">{t.state}</span>
                <span>·</span>
                <span class="font-mono tabular-nums">{props.clock(t)}</span>
              </span>
            </div>
            <Show when={t.state === "running" || t.state === "background"}>
              <button
                class="hidden size-5 shrink-0 items-center justify-center rounded-[2px] text-subtle group-hover:inline-flex hover:bg-toolbar-hover hover:text-danger"
                title="Cancel"
                onClick={(e) => (e.stopPropagation(), live.cancel(t))}
              >
                <Icon name="x" size={16} />
              </button>
            </Show>
          </div>
        )}
      </For>
    </Show>
  );
}
