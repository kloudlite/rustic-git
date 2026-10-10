import { useEffect, useRef, useState } from "react";
import { useKeyboard } from "@opentui/react";
import type { ScrollBoxRenderable } from "@opentui/core";
import { theme } from "../theme.ts";
import { SplitBorder } from "../ui/border.ts";
import { useWheelAccel } from "../wheel.ts";
import type { PlanRow } from "../tasks.ts";
import { PlanLine } from "./Plan.tsx";

/** Keep row `i` of a list scrollbox on screen (the cursor follows j/k). */
export function follow(sb: ScrollBoxRenderable | null, i: number) {
  if (!sb) return;
  const h = Math.max(1, sb.viewport.height);
  if (i < sb.scrollTop) sb.scrollTo(i);
  else if (i >= sb.scrollTop + h) sb.scrollTo(i - h + 1);
}

/** ^g. Laid out like Files: every plan row on the left (session heads are labels, tasks are the
 *  cursor stops), the selected task's detail on the right; tab moves between panes. */
export function PlanScreen({ rows, width, onClose }: { rows: PlanRow[]; width: number; onClose: () => void }) {
  const wheel = useWheelAccel();
  const listRef = useRef<ScrollBoxRenderable>(null);
  const readRef = useRef<ScrollBoxRenderable>(null);
  const stops = rows.flatMap((r, i) => (r.task ? [i] : []));
  const [sel, setSel] = useState(0);
  const [pane, setPane] = useState<"list" | "detail">("list");
  const at = Math.min(sel, Math.max(0, stops.length - 1));
  const cur = stops[at];
  const row = cur === undefined ? undefined : rows[cur];
  const t = row?.task;
  useEffect(() => follow(listRef.current, cur ?? 0), [cur]);
  useKeyboard((key) => {
    if ((key.ctrl && key.name === "g") || key.name === "escape") return onClose();
    if (key.ctrl || key.meta || key.option) return;
    if (key.name === "tab") return setPane((p) => (p === "list" ? "detail" : "list"));
    if (pane === "detail") {
      const sb = readRef.current;
      const page = Math.max(1, (sb?.viewport.height ?? 20) - 2);
      if (key.name === "j" || key.name === "down") sb?.scrollBy(1);
      if (key.name === "k" || key.name === "up") sb?.scrollBy(-1);
      if (key.name === "d") sb?.scrollBy(Math.ceil(page / 2));
      if (key.name === "u") sb?.scrollBy(-Math.ceil(page / 2));
      if (key.name === "h") setPane("list");
      return;
    }
    if (key.name === "j" || key.name === "down") setSel(Math.min(stops.length - 1, at + 1));
    if (key.name === "k" || key.name === "up") setSel(Math.max(0, at - 1));
    if (key.name === "l" || key.name === "return") setPane("detail");
  });
  const sessions = rows.filter((r) => r.head).length;
  const listWidth = Math.max(24, Math.floor(width * 0.42) - 1);
  return (
    <box flexDirection="column" flexGrow={1} minHeight={0}>
      <box flexDirection="row" justifyContent="space-between" paddingLeft={1} paddingRight={1}>
        <text fg={theme.muted}>plan</text>
        <text fg={theme.muted}>{stops.length} tasks · {sessions} {sessions === 1 ? "session" : "sessions"}</text>
      </box>
      <box flexDirection="row" flexGrow={1} minHeight={0} marginTop={1}>
        {/* left: the boards, one tree per session */}
        <box flexDirection="column" width="42%" minWidth={24} flexShrink={0} paddingLeft={1} onMouseDown={() => setPane("list")}>
          <scrollbox ref={listRef} flexGrow={1} flexBasis={0} scrollbarOptions={{ visible: false }} scrollAcceleration={wheel}>
            {rows.length === 0 ? (
              <text fg={theme.muted}>no tasks yet — sessions plan on their own board</text>
            ) : (
              rows.map((r, i) => (
                <box key={i} onMouseDown={r.task ? () => setSel(stops.indexOf(i)) : undefined}>
                  <PlanLine row={r} width={listWidth} on={i === cur && pane === "list"} />
                </box>
              ))
            )}
          </scrollbox>
        </box>

        {/* right: the selected task */}
        <box flexDirection="column" flexGrow={1} minHeight={0} {...SplitBorder} border={["left"]} borderColor={pane === "detail" ? theme.accent : theme.border} onMouseDown={() => setPane("detail")}>
          {!t ? (
            <box paddingLeft={2}><text fg={theme.muted}>no task selected</text></box>
          ) : (
            <>
              <box flexDirection="row" justifyContent="space-between" paddingLeft={2} paddingRight={2}>
                <text fg={theme.fg}><b>{t.id} {t.title}</b></text>
                <text fg={theme.muted}>{row?.right ?? t.state}</text>
              </box>
              <scrollbox ref={readRef} flexGrow={1} flexBasis={0} marginTop={1} paddingLeft={2} scrollbarOptions={{ visible: false }} scrollAcceleration={wheel}>
                <text fg={theme.fg}><span fg={theme.muted}>session  </span>{row?.session ?? "—"}</text>
                <text fg={theme.fg}><span fg={theme.muted}>state    </span>{t.state}</text>
                <text fg={theme.fg}><span fg={theme.muted}>priority </span>{String(t.priority)}</text>
                <text fg={theme.fg}><span fg={theme.muted}>after    </span>{t.dependsOn.length ? t.dependsOn.join(", ") : "—"}</text>
                <box marginTop={1}><text fg={t.note ? theme.fg : theme.muted}>{t.note || "no note"}</text></box>
              </scrollbox>
            </>
          )}
        </box>
      </box>

      {/* footer hints */}
      <box flexDirection="row" gap={2} paddingLeft={1} marginTop={1}>
        <text fg={theme.muted}>plan › {t ? `${row?.session ?? ""} ${t.id}`.trim() : "—"}</text>
        <box flexGrow={1} />
        <text fg={theme.fg}>j k <span fg={theme.muted}>move</span></text>
        <text fg={theme.fg}>l <span fg={theme.muted}>open</span></text>
        <text fg={theme.fg}>tab <span fg={theme.muted}>pane</span></text>
        <text fg={theme.fg}>^g <span fg={theme.muted}>chat</span></text>
        <text fg={theme.fg}>esc <span fg={theme.muted}>back</span></text>
      </box>
    </box>
  );
}
