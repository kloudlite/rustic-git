import { useRef } from "react";
import { useKeyboard } from "@opentui/react";
import type { ScrollBoxRenderable } from "@opentui/core";
import { theme } from "../theme.ts";
import { useWheelAccel } from "../wheel.ts";
import type { PlanRow } from "../tasks.ts";
import { PlanLine } from "./Plan.tsx";

/** ^g. Every plan row, no fold; j/k scroll, esc or ^g leaves. Shares the Jobs screen's shell. */
export function PlanScreen({ rows, width, onClose }: { rows: PlanRow[]; width: number; onClose: () => void }) {
  const wheel = useWheelAccel();
  const ref = useRef<ScrollBoxRenderable>(null);
  useKeyboard((key) => {
    if ((key.ctrl && key.name === "g") || key.name === "escape") return onClose();
    if (key.ctrl || key.meta || key.option) return;
    const page = Math.max(1, (ref.current?.viewport.height ?? 20) - 2);
    if (key.name === "j" || key.name === "down") ref.current?.scrollBy(1);
    if (key.name === "k" || key.name === "up") ref.current?.scrollBy(-1);
    if (key.name === "d") ref.current?.scrollBy(Math.ceil(page / 2));
    if (key.name === "u") ref.current?.scrollBy(-Math.ceil(page / 2));
  });
  return (
    <box flexDirection="column" flexGrow={1} minHeight={0}>
      <box flexDirection="row" justifyContent="space-between" paddingLeft={1} paddingRight={1}>
        <text fg={theme.fg}><b>Plan</b></text>
        <text fg={theme.muted}>^g chat · esc</text>
      </box>
      <scrollbox ref={ref} flexGrow={1} flexBasis={0} marginTop={1} scrollbarOptions={{ visible: false }} scrollAcceleration={wheel}>
        {rows.length === 0 ? (
          <box paddingLeft={1}><text fg={theme.muted}>no tasks yet — sessions plan on their own board</text></box>
        ) : (
          rows.map((r, i) => <PlanLine key={i} row={r} width={width} />)
        )}
      </scrollbox>
    </box>
  );
}
