import { useState } from "react";
import { useKeyboard } from "@opentui/react";
import { theme } from "../theme.ts";
import { SplitBorder } from "../ui/border.ts";
import { clip } from "../ui/text.ts";
import type { DoneTask } from "../tasks.ts";

/** ^q. Finished tasks newest first on the left, the selected one's title, session and note on the right. */
export function DoneScreen({ tasks, main, onClose }: { tasks: DoneTask[]; main: boolean; onClose: () => void }) {
  const [sel, setSel] = useState(0);
  const at = Math.min(sel, Math.max(0, tasks.length - 1));
  const t = tasks[at];
  useKeyboard((key) => {
    if ((key.ctrl && key.name === "q") || key.name === "escape") return onClose();
    if (key.ctrl || key.meta || key.option) return;
    if (key.name === "j" || key.name === "down") setSel(Math.min(tasks.length - 1, at + 1));
    if (key.name === "k" || key.name === "up") setSel(Math.max(0, at - 1));
  });
  return (
    <box flexDirection="column" flexGrow={1} minHeight={0}>
      <box flexDirection="row" justifyContent="space-between" paddingLeft={1} paddingRight={1}>
        <text fg={theme.fg}><b>Done</b></text>
        <text fg={theme.muted}>{tasks.length} done · ^q close · esc</text>
      </box>
      <box flexDirection="row" flexGrow={1} minHeight={0} marginTop={1}>
        <box flexDirection="column" width="42%" minWidth={24} flexShrink={0} paddingLeft={1}>
          {tasks.map((d, i) => (
            <box key={`${d.session}${d.id}`} height={1} overflow="hidden" paddingLeft={1} paddingRight={1} backgroundColor={i === at ? theme.selection : undefined} onMouseDown={() => setSel(i)}>
              <text selectable={false} fg={i === at ? theme.bg : theme.fg}>{clip(main ? `${d.name} ${d.id} ${d.title}` : `${d.id} ${d.title}`, 60)}</text>
            </box>
          ))}
        </box>
        <box flexDirection="column" flexGrow={1} minHeight={0} {...SplitBorder} border={["left"]} borderColor={theme.border} paddingLeft={2}>
          {!t ? (
            <text fg={theme.muted}>nothing finished yet</text>
          ) : (
            <>
              <text fg={theme.fg}><b>{t.id} {t.title}</b></text>
              <text fg={theme.muted}>{t.name}</text>
              <box marginTop={1}><text fg={t.note ? theme.fg : theme.muted}>{t.note || "no note"}</text></box>
            </>
          )}
        </box>
      </box>
    </box>
  );
}
