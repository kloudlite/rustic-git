import { TextAttributes } from "@opentui/core";
import { theme } from "../theme.ts";

/** Bottom bar: connected environment left; tokens, hints right. */
export function HintBar({
  busy,
  tokens,
  queued,
  permMode,
  active,
  inWorkspace,
  normal,
  vim,
  compact,
  onHint,
}: {
  busy: boolean;
  tokens: number;
  queued: number;
  /** Permission mode; only shown when it is not "default". */
  permMode?: string;
  /** The connected environment. The session you are in is on the prompt card. */
  active: string;
  inWorkspace: boolean;
  normal: boolean;
  /** vim keys on: esc leads to NORMAL mode. Off: ctrl+<letter> commands. */
  vim: boolean;
  /** Narrow terminal: show only the essential hints so the row can't wrap. */
  compact: boolean;
  /** Hints are clickable: the id matches the key they stand for. */
  onHint?: (id: "type" | "jump" | "files" | "jobs" | "commands" | "help" | "queue") => void;
}) {
  const click = (id: Parameters<NonNullable<typeof onHint>>[0]) =>
    onHint ? () => onHint(id) : undefined;
  return (
    <box flexDirection="row" flexShrink={0} justifyContent="space-between" overflow="hidden">
      <box flexDirection="row" gap={2} marginLeft={1}>
        <text selectable={false} fg={theme.muted}>{active}</text>
        {permMode && permMode !== "default" && (
          <text selectable={false} fg={permMode === "bypass" ? theme.error : theme.warning}>
            {permMode} <span attributes={TextAttributes.DIM}>shift+tab</span>
          </text>
        )}
        {queued > 0 && (
          <box onMouseDown={click("queue")}>
            <text selectable={false} fg={theme.warning}>
              {queued} queued <span attributes={TextAttributes.DIM}>q edit</span>
            </text>
          </box>
        )}
        {busy && (
          // no spinner here — the transcript's own "working…" row is the one
          // that says a turn is live; two of them read as two things running
          <text selectable={false} fg={theme.muted}>
            <b>esc</b> <span attributes={TextAttributes.DIM}>interrupt</span>
          </text>
        )}
      </box>
      <box flexDirection="row" gap={2} flexShrink={1} overflow="hidden">
        <text selectable={false} fg={theme.muted}>{tokens.toLocaleString()} tok</text>
        {normal ? (
          <>
            <box onMouseDown={click("type")}><text selectable={false} fg={theme.fg}>i <span fg={theme.muted}>type</span></text></box>
            {!compact && <text selectable={false} fg={theme.fg}>j k <span fg={theme.muted}>workspaces</span></text>}
            {!compact && <box onMouseDown={click("jump")}><text selectable={false} fg={theme.fg}>p <span fg={theme.muted}>jump</span></text></box>}
            {inWorkspace && <box onMouseDown={click("files")}><text selectable={false} fg={theme.fg}>f <span fg={theme.muted}>files</span></text></box>}
            <box onMouseDown={click("jobs")}><text selectable={false} fg={theme.fg}>^j <span fg={theme.muted}>jobs</span></text></box>
            <box onMouseDown={click("commands")}><text selectable={false} fg={theme.fg}>/ <span fg={theme.muted}>commands</span></text></box>
            <box onMouseDown={click("help")}><text selectable={false} fg={theme.fg}>? <span fg={theme.muted}>help</span></text></box>
          </>
        ) : vim ? (
          <>
            <text selectable={false} fg={theme.fg}>enter <span fg={theme.muted}>send</span></text>
            <text selectable={false} fg={theme.fg}>esc <span fg={theme.muted}>normal mode</span></text>
          </>
        ) : (
          <>
            <text selectable={false} fg={theme.fg}>enter <span fg={theme.muted}>send</span></text>
            {!compact && <box onMouseDown={click("jump")}><text selectable={false} fg={theme.fg}>^p <span fg={theme.muted}>jump</span></text></box>}
            {inWorkspace && <box onMouseDown={click("files")}><text selectable={false} fg={theme.fg}>^f <span fg={theme.muted}>files</span></text></box>}
            <box onMouseDown={click("jobs")}><text selectable={false} fg={theme.fg}>^j <span fg={theme.muted}>jobs</span></text></box>
            <box onMouseDown={click("commands")}><text selectable={false} fg={theme.fg}>/ <span fg={theme.muted}>commands</span></text></box>
          </>
        )}
      </box>
    </box>
  );
}
