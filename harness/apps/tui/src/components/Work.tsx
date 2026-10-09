import { theme } from "../theme.ts";
import { clip } from "../ui/text.ts";
import type { WorkRow } from "../tasks.ts";
import { Heading } from "./Sidebar.tsx";

const MAX = 6;

/** The plan and background work of the session on screen, above the queue; nothing when idle. */
export function Work({ rows, width }: { rows: WorkRow[]; width: number }) {
  if (rows.length === 0) return null;
  const shown = rows.length > MAX ? [...rows.slice(0, MAX - 1), { text: `+${rows.length - MAX + 1} more in the processes view`, dim: true }] : rows;
  return (
    <box flexDirection="column" flexShrink={0} marginBottom={1}>
      <Heading width={width} flush>Work</Heading>
      {shown.map((r, i) => (
        <box key={i} paddingLeft={1} height={1}>
          <text fg={r.dim ? theme.muted : theme.fg}>{clip(r.text, width - 2)}</text>
        </box>
      ))}
    </box>
  );
}
