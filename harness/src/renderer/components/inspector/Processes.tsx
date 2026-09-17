import { For, Show, createSignal, onCleanup } from "solid-js";
import * as live from "../../live";
import { procLabel, procsOf, procState } from "../../rows";
import { Icon } from "../../ui/Icon";
import { Heading } from "../../ui/parts";

/**
 * Long-lived processes, under the tasks: a dev server or a watcher the bench
 * started and left running. A row is the name, the command and the uptime;
 * one that exited shows its code for a minute and then goes. Hover shows the
 * stop; click opens the live log.
 */
export function Processes(props: { onOpen: (id: string) => void; session: string }) {
  const [tick, setTick] = createSignal(Date.now());
  const timer = setInterval(() => setTick(Date.now()), 1000);
  onCleanup(() => clearInterval(timer));
  const up = (p: live.Proc) => {
    const s = Math.max(0, Math.round(((p.ended ?? tick()) - p.started) / 1000));
    return s < 60 ? `${s}s` : s < 3600 ? `${Math.floor(s / 60)}m ${s % 60}s` : `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
  };
  // This session's, and what is RUNNING: an ended process has its row in the transcript, and a
  // panel that keeps yesterday's exits is a panel nobody reads (owner, 2026-09-17).
  const KEEP_ENDED_MS = 10_000;
  const mine = () => procsOf(live.procs, props.session).filter((p) => !p.ended || tick() - p.ended < KEEP_ENDED_MS);
  const running = () => mine().filter((p) => !p.ended).length;
  return (
    <Show when={mine().length}>
      <Heading meta={running() ? `${running()} running` : undefined}>Processes</Heading>
      <For each={mine()}>
        {(p) => (
          <div
            class="group flex h-11 cursor-pointer items-center gap-2 px-3 hover:bg-hover"
            classList={{ "opacity-60": !!p.ended }}
            onClick={() => props.onOpen(p.id)}
            title="Open the log"
          >
            <span class="flex w-4 shrink-0 items-center justify-center">
              <span class={`size-1.5 rounded-full ${{ running: "bg-success", done: "bg-subtle", failed: "bg-danger", lost: "bg-warning" }[procState(p)]}`} />
            </span>
            <div class="flex min-w-0 flex-1 flex-col">
              <span class="truncate font-mono text-sm leading-[18px]">
                <span class="font-bold text-fg-strong">{p.name}</span> <span class="text-muted">{p.command}</span>
              </span>
              <span class="flex items-center gap-1.5 text-xs leading-4 text-subtle">
                <span class="font-mono">{p.id}</span>
                <span>·</span>
                <span>{procLabel(p)}</span>
                <span>·</span>
                <span class="font-mono tabular-nums">{up(p)}</span>
              </span>
            </div>
            <Show when={!p.ended}>
              <button
                class="hidden size-5 shrink-0 items-center justify-center rounded-[2px] text-subtle group-hover:inline-flex hover:bg-toolbar-hover hover:text-danger"
                title="Stop"
                onClick={(e) => (e.stopPropagation(), live.stopProc(p))}
              >
                <Icon name="x" size={16} />
              </button>
            </Show>
          </div>
        )}
      </For>
      <div class="h-3" />
    </Show>
  );
}
