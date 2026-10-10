import { useEffect, useRef, useState } from "react";
import { useKeyboard } from "@opentui/react";
import type { ScrollBoxRenderable } from "@opentui/core";
import { theme } from "../theme.ts";
import { SplitBorder } from "../ui/border.ts";
import { clip } from "../ui/text.ts";
import { useWheelAccel } from "../wheel.ts";
import type { DoneTask } from "../tasks.ts";
import { follow } from "./PlanScreen.tsx";

/** ^q. Laid out like Files: finished tasks newest first on the left, the selected one's title,
 *  session and note on the right; tab moves between panes. */
export function DoneScreen({ tasks, main, onClose }: { tasks: DoneTask[]; main: boolean; onClose: () => void }) {
  const wheel = useWheelAccel();
  const listRef = useRef<ScrollBoxRenderable>(null);
  const readRef = useRef<ScrollBoxRenderable>(null);
  const [sel, setSel] = useState(0);
  const [pane, setPane] = useState<"list" | "detail">("list");
  const at = Math.min(sel, Math.max(0, tasks.length - 1));
  const t = tasks[at];
  useEffect(() => follow(listRef.current, at), [at]);
  useKeyboard((key) => {
    if ((key.ctrl && key.name === "q") || key.name === "escape") return onClose();
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
    if (key.name === "j" || key.name === "down") setSel(Math.min(tasks.length - 1, at + 1));
    if (key.name === "k" || key.name === "up") setSel(Math.max(0, at - 1));
    if (key.name === "l" || key.name === "return") setPane("detail");
  });
  return (
    <box flexDirection="column" flexGrow={1} minHeight={0}>
      <box flexDirection="row" justifyContent="space-between" paddingLeft={1} paddingRight={1}>
        <text fg={theme.muted}>done</text>
        <text fg={theme.muted}>{tasks.length} finished</text>
      </box>
      <box flexDirection="row" flexGrow={1} minHeight={0} marginTop={1}>
        {/* left: finished tasks, newest first */}
        <box flexDirection="column" width="42%" minWidth={24} flexShrink={0} paddingLeft={1} onMouseDown={() => setPane("list")}>
          <scrollbox ref={listRef} flexGrow={1} flexBasis={0} scrollbarOptions={{ visible: false }} scrollAcceleration={wheel}>
            {tasks.length === 0 ? (
              <text fg={theme.muted}>nothing finished yet</text>
            ) : (
              tasks.map((d, i) => {
                const active = i === at && pane === "list";
                return (
                  <box key={`${d.session}${d.id}`} height={1} overflow="hidden" paddingLeft={1} paddingRight={1} backgroundColor={active ? theme.selection : undefined} onMouseDown={() => setSel(i)}>
                    <text selectable={false} fg={active ? theme.bg : theme.fg}>{clip(main ? `${d.name} ${d.id} ${d.title}` : `${d.id} ${d.title}`, 60)}</text>
                  </box>
                );
              })
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
                <text fg={theme.muted}>{t.name}</text>
              </box>
              <scrollbox ref={readRef} flexGrow={1} flexBasis={0} marginTop={1} paddingLeft={2} scrollbarOptions={{ visible: false }} scrollAcceleration={wheel}>
                <text fg={t.note ? theme.fg : theme.muted}>{t.note || "no note"}</text>
              </scrollbox>
            </>
          )}
        </box>
      </box>

      {/* footer hints */}
      <box flexDirection="row" gap={2} paddingLeft={1} marginTop={1}>
        <text fg={theme.muted}>done › {t ? (main ? `${t.name} ${t.id}` : t.id) : "—"}</text>
        <box flexGrow={1} />
        <text fg={theme.fg}>j k <span fg={theme.muted}>move</span></text>
        <text fg={theme.fg}>l <span fg={theme.muted}>open</span></text>
        <text fg={theme.fg}>tab <span fg={theme.muted}>pane</span></text>
        <text fg={theme.fg}>^q <span fg={theme.muted}>chat</span></text>
        <text fg={theme.fg}>esc <span fg={theme.muted}>back</span></text>
      </box>
    </box>
  );
}
