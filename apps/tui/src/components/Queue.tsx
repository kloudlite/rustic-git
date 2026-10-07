import { TextAttributes } from "@opentui/core";
import { theme } from "../theme.ts";
import { clip } from "../ui/text.ts";
import type { QueuedMessage } from "../sessions.ts";


/**
 * Messages waiting to be delivered to the running agent, newest last. Shown
 * above the prompt while a turn streams; `q` selects one to edit, `d` drops
 * it. A steering message interrupts the turn, a follow-up waits for it.
 */
export function Queue({
  messages,
  selected,
  width,
  onSelect,
}: {
  messages: QueuedMessage[];
  /** index being targeted in NORMAL mode, or null when the list is just shown */
  selected: number | null;
  width: number;
  onSelect?: (index: number) => void;
}) {
  if (messages.length === 0) return null;

  return (
    <box flexDirection="column" flexShrink={0} marginBottom={1}>
      <box flexDirection="row" justifyContent="space-between" height={1} paddingLeft={1} paddingRight={1}>
        <text selectable={false}>
          <span fg={theme.warning} attributes={TextAttributes.BOLD}>
            QUEUED
          </span>
          <span fg={theme.border}>
            {"  "}
            {String(messages.length)}
          </span>
        </text>
        {selected !== null && (
          <text selectable={false} fg={theme.muted}>
            enter edit · d drop · esc done
          </text>
        )}
      </box>
      {messages.map((m, i) => {
        const on = selected === i;
        return (
          <box
            key={`${i}-${m.text}`}
            flexDirection="row"
            height={1}
            overflow="hidden"
            paddingLeft={1}
            paddingRight={1}
            backgroundColor={on ? theme.surfaceRaised : undefined}
            onMouseDown={onSelect ? () => onSelect(i) : undefined}
          >
            <text selectable={false}>
              <span fg={on ? theme.accent : theme.border}>{on ? "› " : "  "}</span>
              <span fg={theme.muted}>
                {m.kind === "steer" ? "steer " : "after "}
              </span>
              <span fg={on ? theme.fg : theme.muted}>{clip(m.text, width - 12)}</span>
            </text>
          </box>
        );
      })}
    </box>
  );
}
