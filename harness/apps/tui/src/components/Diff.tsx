import { theme } from "../theme.ts";
import type { FileDiff } from "../diff.ts";

const MAX_LINES = 20;

/**
 * Unified-diff hunk: line-number gutter, +/- signs, added/removed rows on
 * tinted backgrounds (Claude Code's edit rendering).
 */
export function DiffView({ diff, maxLines = MAX_LINES }: { diff: FileDiff; maxLines?: number }) {
  const shown = diff.lines.slice(0, maxLines);
  const hidden = diff.lines.length - shown.length;
  return (
    <box flexDirection="column">
      {shown.map((line, i) => {
        if (line.mark === "deleted-gap") {
          // a thin rule where lines were removed — readable, and it says how many
          return (
            <box key={i} flexDirection="row">
              <box width={6} flexShrink={0} />
              <text fg={theme.diffRemoved}>
                {"─".repeat(2)} {line.count} deleted {"─".repeat(2)}
              </text>
            </box>
          );
        }
        const fg =
          line.sign === "+" ? theme.diffAdded : line.sign === "-" ? theme.diffRemoved : theme.fg;
        const bg =
          line.sign === "+" ? theme.diffAddedBg : line.sign === "-" ? theme.diffRemovedBg : undefined;
        // full-file view: the gutter carries the change — the number goes
        // green for a new line, and a red tick marks where lines were deleted
        const gutter =
          line.mark === "added" ? theme.diffAdded : line.sign === " " ? theme.muted : fg;
        return (
          <box key={i} flexDirection="row" backgroundColor={bg}>
            <box width={6} flexShrink={0}>
              <text fg={gutter}>{String(line.no).padStart(4)}</text>
            </box>
            <box width={2} flexShrink={0}>
              <text fg={fg}>{line.sign === " " ? "" : line.sign}</text>
            </box>
            <text fg={fg}>{line.text || " "}</text>
          </box>
        );
      })}
      {hidden > 0 && (
        <box paddingLeft={6}>
          <text fg={theme.muted}>… +{hidden} more lines</text>
        </box>
      )}
    </box>
  );
}
