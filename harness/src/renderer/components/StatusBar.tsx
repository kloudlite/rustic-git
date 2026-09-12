import { Icon } from "../ui/Icon";
import type { Machine } from "../model";

export function StatusBar(props: {
  machine: Machine;
  shells: number;
  leftOpen: boolean;
  rightOpen: boolean;
  onToggleLeft: () => void;
  onToggleRight: () => void;
}) {
  const eph = () => props.machine.workspaces.flatMap((w) => w.ephemerals);
  const running = () => eph().filter((e) => e.state === "running").length;
  const failed = () => eph().filter((e) => e.state === "failed").length;
  const up = () => props.machine.workspaces.filter((w) => w.state === "running").length;

  const item = "inline-flex h-5 items-center gap-1.5 rounded-sm px-1.5 whitespace-nowrap";
  const btn = `${item} hover:bg-line hover:text-fg aria-pressed:bg-active aria-pressed:text-fg`;

  return (
    <footer class="flex items-center gap-0.5 border-t border-line bg-chrome px-2 text-sm text-muted">
      <button class={btn} aria-pressed={props.leftOpen} onClick={props.onToggleLeft} title="Machine panel (⌘B)"><Icon name="panelLeft" /></button>
      <button class={btn} aria-pressed={props.rightOpen} onClick={props.onToggleRight} title="Inspector (⌘⌥B)"><Icon name="panelRight" /></button>
      <span class="mx-1 h-3.5 w-px bg-line" />
      <span class={`${item} text-success`}><Icon name="check" size={12} /> connected</span>
      <span class={item}><Icon name="server" size={12} /> 12 ms</span>
      {props.shells > 0 && (
        <span class={item} title="Open shells (⌘J)"><Icon name="terminal" size={12} /> {props.shells}</span>
      )}
      <span class="flex-1" />
      <span class={`${item} text-success`}><Icon name="dotFilled" size={10} /> {running()} running</span>
      {failed() > 0 && <span class={`${item} text-danger`}><Icon name="x" size={11} /> {failed()} failed</span>}
      <span class={item}>{up()} of {props.machine.workspaces.length} workspaces up</span>
    </footer>
  );
}
