import { Show } from "solid-js";
import { Icon } from "../ui/Icon";
import type { Machine } from "../model";

export function StatusBar(props: {
  machine: Machine;
  shells: number;
  env: string;
}) {
  const eph = () => props.machine.workspaces.flatMap((w) => w.ephemerals);
  const running = () => eph().filter((e) => e.state === "running").length;
  const failed = () => eph().filter((e) => e.state === "failed").length;
  const up = () => props.machine.workspaces.filter((w) => w.state === "running").length;

  const item = "inline-flex h-5.5 items-center gap-1 px-1.25 leading-none whitespace-nowrap tabular-nums";
  const dot = (tone: string) => <span class={`h-1.5 w-1.5 rounded-full ${tone}`} />;

  return (
    <footer class="flex h-5.5 items-center border-t border-line bg-chrome px-1 text-sm text-fg">
      <span class={item} title={`Connected to ${props.env}`}>{dot("bg-success")} {props.env}<span class="text-subtle"> · 12 ms</span></span>
      <Show when={props.shells > 0}>
        <span class={item} title="Open shells (⌘J)"><Icon name="terminal" size={12} /> {props.shells}</span>
      </Show>
      <span class="flex-1" />
      <span class={item}>{dot("bg-success")} {running()} running</span>
      <Show when={failed() > 0}>
        <span class={item}>{dot("bg-danger")} {failed()} failed</span>
      </Show>
      <span class="mx-1 h-3 w-px bg-line" />
      <span class={item} title="Workspaces up">{`${up()}/${props.machine.workspaces.length} workspaces`}</span>
    </footer>
  );
}
