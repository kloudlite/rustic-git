import { TextAttributes } from "@opentui/core";
import { theme } from "../theme.ts";
import { clip } from "../ui/text.ts";


/**
 * The bar above a chat session: its title, and under it the context the
 * session belongs to (environment › workspace) or its own description.
 * Clicking it opens `/session`, where the title and description are set.
 */
export function SessionTitle({
  title,
  description,
  busy,
  width,
  onOpen,
}: {
  title: string;
  description?: string;
  /** a turn is streaming in this session */
  busy?: boolean;
  width: number;
  onOpen?: () => void;
}) {
  return (
    // the session column sets gap 1 between its children; the hairline is the
    // separator here, so cancel that gap and let the transcript start right under it
    <box flexDirection="column" flexShrink={0} paddingTop={1} marginBottom={-1} onMouseDown={onOpen}>
      <box flexDirection="row" justifyContent="space-between" height={1} overflow="hidden">
        <text selectable={false}>
          <span fg={theme.fg} attributes={TextAttributes.BOLD}>
            {clip(title, width - 10)}
          </span>
        </text>
        {busy && (
          <text selectable={false}>
            <span fg={theme.accent}>working</span>
          </text>
        )}
      </box>
      {description ? (
        <text selectable={false} attributes={TextAttributes.DIM}>
          <span fg={theme.muted}>{clip(description, width)}</span>
        </text>
      ) : null}
      <text fg={theme.border}>
        {"─".repeat(Math.max(0, width))}
      </text>
    </box>
  );
}
