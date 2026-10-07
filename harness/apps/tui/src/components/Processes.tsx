import { useEffect, useMemo, useRef, useState } from "react";
import { useKeyboard } from "@opentui/react";
import { TextAttributes, type ScrollBoxRenderable } from "@opentui/core";
import { theme } from "../theme.ts";
import { SplitBorder } from "../ui/border.ts";
import type { Process, ProcessStatus } from "../workspaces.ts";
import { useWheelAccel } from "../wheel.ts";

const color = (status: ProcessStatus) =>
  ({
    running: theme.success,
    starting: theme.warning,
    exited: theme.muted,
    crashed: theme.error,
  })[status];

const detail = (p: Process) =>
  p.status === "exited" || p.status === "crashed" ? `${p.status} (${p.code ?? 0})` : p.status;

/**
 * Processes view: what the workspace is running on the left, that process's
 * output on the right. Same shape and keys as the files view — j/k move, l or
 * enter focuses the log, tab swaps panes, esc goes back.
 */
export function Processes({
  workspace,
  processes,
  onClose,
  onCycle,
}: {
  workspace: string;
  processes: Process[];
  onClose: () => void;
  /** `f` moves on to the next view, same as outside. */
  onCycle: () => void;
}) {
  const wheel = useWheelAccel();
  const [sel, setSel] = useState(0);
  const [pane, setPane] = useState<"list" | "log">("list");
  const logRef = useRef<ScrollBoxRenderable>(null);
  const cur = Math.min(sel, Math.max(0, processes.length - 1));
  const proc = processes[cur];
  const lines = useMemo(() => proc?.logs ?? [], [proc]);

  // a new selection starts at the tail of that process's output
  useEffect(() => {
    logRef.current?.scrollTo(Math.max(0, lines.length - 1));
  }, [cur, lines.length]);

  useKeyboard((key) => {
    if (key.ctrl || key.meta || key.option) return;
    if (key.name === "escape") return onClose();
    if (key.name === "f") return onCycle();
    if (key.name === "tab") return setPane((p) => (p === "list" ? "log" : "list"));
    if (pane === "list") {
      if (key.name === "j" || key.name === "down") return setSel(Math.min(processes.length - 1, cur + 1));
      if (key.name === "k" || key.name === "up") return setSel(Math.max(0, cur - 1));
      if (key.name === "l" || key.name === "return") return setPane("log");
      return;
    }
    const sb = logRef.current;
    const page = Math.max(1, (sb?.viewport.height ?? 20) - 2);
    if (key.name === "j" || key.name === "down") sb?.scrollBy(1);
    if (key.name === "k" || key.name === "up") sb?.scrollBy(-1);
    if (key.name === "d") sb?.scrollBy(Math.ceil(page / 2));
    if (key.name === "u") sb?.scrollBy(-Math.ceil(page / 2));
    if (key.name === "g") sb?.scrollTo(0);
    if (key.name === "G") sb?.scrollTo(Math.max(0, lines.length - 1));
    if (key.name === "h") setPane("list");
  });

  const running = processes.filter((p) => p.status === "running").length;

  return (
    <box flexDirection="column" flexGrow={1} minHeight={0}>
      <box flexDirection="row" justifyContent="space-between" paddingLeft={1} paddingRight={1}>
        <text fg={theme.muted}>{workspace}</text>
        <text fg={theme.muted}>
          {running}/{processes.length} running
        </text>
      </box>

      <box flexDirection="row" flexGrow={1} minHeight={0} marginTop={1}>
        <box
          flexDirection="column"
          width="38%"
          minWidth={22}
          flexShrink={0}
          paddingLeft={1}
          onMouseDown={() => setPane("list")}
        >
          {processes.length === 0 ? (
            <text fg={theme.muted}>nothing running in this workspace</text>
          ) : (
            processes.map((p, i) => {
              const on = i === cur && pane === "list";
              return (
                <box
                  key={p.name}
                  flexDirection="column"
                  onMouseDown={() => {
                    setSel(i);
                    setPane("list");
                  }}
                >
                  <box height={1} overflow="hidden" paddingLeft={1} backgroundColor={on ? theme.selection : undefined}>
                    <text selectable={false}>
                      <span fg={on ? theme.bg : color(p.status)}>•</span>{" "}
                      <span fg={on ? theme.bg : theme.fg}>{p.name}</span>
                      {p.port ? <span fg={on ? theme.bg : theme.muted}>:{p.port}</span> : ""}
                      <span fg={on ? theme.bg : theme.muted}>
                        {"  "}
                        {detail(p)}
                      </span>
                    </text>
                  </box>
                  <box height={1} overflow="hidden" paddingLeft={3}>
                    <text fg={theme.muted}>{p.command}</text>
                  </box>
                </box>
              );
            })
          )}
        </box>

        <box
          flexDirection="column"
          flexGrow={1}
          minHeight={0}
          {...SplitBorder}
          border={["left"]}
          borderColor={pane === "log" ? theme.accent : theme.border}
          onMouseDown={() => setPane("log")}
        >
          {!proc ? (
            <box paddingLeft={2} paddingTop={1}>
              <text fg={theme.muted}>no process selected</text>
            </box>
          ) : (
            <>
              <box flexDirection="row" justifyContent="space-between" paddingLeft={2} paddingRight={2}>
                <text>
                  <span fg={theme.fg}>{proc.name}</span>
                  <span fg={theme.muted}>  {proc.command}</span>
                </text>
                <text fg={color(proc.status)}>{detail(proc)}</text>
              </box>
              <scrollbox
                ref={logRef}
                flexGrow={1}
                flexBasis={0}
                marginTop={1}
                paddingLeft={2}
                scrollbarOptions={{ visible: false }} scrollAcceleration={wheel}
              >
                <box flexDirection="column" flexShrink={0}>
                  {lines.length === 0 ? (
                    <text fg={theme.muted}>(no output yet)</text>
                  ) : (
                    lines.map((line, i) => (
                      <text key={i} fg={theme.muted}>
                        {line}
                      </text>
                    ))
                  )}
                </box>
              </scrollbox>
            </>
          )}
        </box>
      </box>

      <box flexDirection="row" gap={2} paddingLeft={1} marginTop={1}>
        <text fg={theme.muted}>processes › {proc?.name ?? "—"}</text>
        <box flexGrow={1} />
        <text fg={theme.fg}>j k <span fg={theme.muted}>move</span></text>
        <text fg={theme.fg}>l <span fg={theme.muted}>logs</span></text>
        <text fg={theme.fg}>h <span fg={theme.muted}>back</span></text>
        <text fg={theme.fg}>u d <span fg={theme.muted}>scroll</span></text>
        <text fg={theme.fg}>esc <span fg={theme.muted}>back</span></text>
      </box>
    </box>
  );
}
