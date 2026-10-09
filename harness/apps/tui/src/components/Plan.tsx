import { theme } from "../theme.ts";
import { clip } from "../ui/text.ts";
import type { PlanRow, Tone } from "../tasks.ts";
import { Heading } from "./Sidebar.tsx";

export const toneColor = (t?: Tone) => (t ? theme[t] : theme.fg);

/** One plan line: guides and text left, state right. Group headers are bold. */
export function PlanLine({ row, width }: { row: PlanRow; width: number }) {
  const right = row.right ?? "";
  const left = clip(row.text, Math.max(1, width - right.length - (right ? 2 : 0)));
  return (
    <box flexDirection="row" justifyContent="space-between" height={1} overflow="hidden" paddingLeft={1} paddingRight={1}>
      <text fg={row.head ? theme.fg : toneColor(row.tone)}>{row.head ? <b>{left}</b> : left}</text>
      <text fg={row.tone === "accent" || row.tone === "error" ? toneColor(row.tone) : theme.muted}>{right}</text>
    </box>
  );
}

/** The plan of the session on screen (rows are already folded by planRows), above the queue; nothing when empty. */
export function Plan({ rows, width }: { rows: PlanRow[]; width: number }) {
  if (rows.length === 0) return null;
  return (
    <box flexDirection="column" flexShrink={0} marginBottom={1}>
      <Heading width={width} flush>Plan</Heading>
      {rows.map((r, i) => (
        <PlanLine key={i} row={r} width={width} />
      ))}
    </box>
  );
}
