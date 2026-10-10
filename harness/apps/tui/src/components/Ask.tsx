import { useState } from "react";
import { useKeyboard } from "@opentui/react";
import { TextAttributes } from "@opentui/core";
import { theme } from "../theme.ts";
import { SPECIAL } from "./Input.tsx";
import { SplitBorder } from "../ui/border.ts";
import { DiffView } from "./Diff.tsx";
import type { FileDiff } from "../diff.ts";

export type Ask = {
  /** the daemon's card id; absent on local panels (help) */
  id?: string;
  /** key of the session that asks; the card shows only in that workspace's view */
  key: string;
  /** e.g. "Permission required" or the question text */
  title: string;
  /** dim secondary line, e.g. "Shell command" or the file path */
  subtitle?: string;
  /** body block, e.g. "$ git status" */
  body?: string;
  /** diff hunk (edit/write permission prompts) */
  diff?: FileDiff;
  options: { id: string; label: string; hint?: string }[];
  /** "buttons": horizontal strip (few options); "list": vertical, scrolls (many) */
  layout?: "buttons" | "list";
  /** option chosen when the user presses esc (defaults to the last one) */
  escapeId?: string;
  resolve: (id: string) => void;
};

/** Rows shown at once in list layout; the window follows the selection. */
const LIST_MAX = 8;
/** Widest button strip, in cells, before the options stack as a list. */
const STRIP_MAX = 48;

/**
 * opencode's permission panel, ported 1:1: warning ┃ border on the panel bg,
 * "△ title" + "# subtitle" header, body, and a raised button strip —
 * highlighted button on the warning color, "⇆ select · enter confirm" hints
 * right. ⇆/arrows select, enter confirms, esc picks `escapeId`.
 */
export function AskPanel({ ask }: { ask: Ask }) {
  const [sel, setSel] = useState(0);
  const [query, setQuery] = useState("");

  // A strip of long labels runs off the right edge (2026-10-10: a question's second answer was cut),
  // so anything wider than a short permission strip ("Allow once / Always / Reject") becomes a list.
  const list = ask.layout === "list" || ask.options.reduce((w, o) => w + o.label.length + 3, 0) > STRIP_MAX;
  // list layout is searchable: typing filters the options
  const options =
    list && query
      ? ask.options.filter((o) => o.label.toLowerCase().includes(query.toLowerCase()))
      : ask.options;
  const n = options.length;
  const cur = Math.min(sel, Math.max(0, n - 1));

  useKeyboard((key) => {
    const prev = list ? key.name === "up" : key.name === "left" || (key.name === "tab" && key.shift);
    const next = list ? key.name === "down" : key.name === "right" || key.name === "tab";
    if (prev) return setSel((i) => (i - 1 + Math.max(1, n)) % Math.max(1, n));
    if (next) return setSel((i) => (i + 1) % Math.max(1, n));
    if (key.name === "return") {
      if (options[cur]) ask.resolve(options[cur].id);
      return;
    }
    if (key.name === "escape")
      return ask.resolve(ask.escapeId ?? ask.options[ask.options.length - 1]!.id);
    if (!list || key.ctrl || key.meta || key.option) return;
    if (key.name === "backspace" || key.name === "delete") {
      setQuery((q) => q.slice(0, -1));
      setSel(0);
      return;
    }
    if (SPECIAL.has(key.name)) return;
    const text = key.sequence;
    if (text && !text.startsWith("\x1b") && text >= " ") {
      setQuery((q) => q + text);
      setSel(0);
    }
  });

  return (
    <box
      width="100%"
      flexDirection="column"
      border={["left"]}
      customBorderChars={SplitBorder.customBorderChars}
      borderColor={theme.warning}
      backgroundColor={theme.surface}
    >
      {/* opencode: gap 1, paddingLeft 1, paddingRight 3, paddingY 1 */}
      <box
        flexDirection="column"
        gap={1}
        paddingLeft={1}
        paddingRight={3}
        paddingTop={1}
        paddingBottom={1}
      >
        <box flexDirection="column">
          {/* one text, so a wrapped title keeps the space after △ */}
          <box paddingLeft={1}>
            <text selectable={false} fg={theme.fg}>
              <span fg={theme.warning}>△ </span>
              {ask.title}
            </text>
          </box>
          {ask.subtitle && (
            <box flexDirection="row" gap={1} paddingLeft={2}>
              <text selectable={false} fg={theme.muted}># {ask.subtitle}</text>
            </box>
          )}
        </box>
        {ask.diff && (
          <box flexDirection="column" paddingLeft={1}>
            <DiffView diff={ask.diff} />
          </box>
        )}
        {ask.body && (
          <box paddingLeft={1}>
            <text selectable={false} fg={theme.fg}>{ask.body}</text>
          </box>
        )}
        {list && (
          <box flexDirection="column" paddingLeft={1}>
            <text selectable={false}>
              <span fg={theme.accent}>› </span>
              <span fg={theme.fg}>{query}</span>
              <span attributes={TextAttributes.INVERSE}> </span>
              {query === "" && <span fg={theme.placeholder}>type to search…</span>}
            </text>
            {n === 0 && <text selectable={false} fg={theme.muted}>No matches</text>}
            {(() => {
              // scroll window that follows the selection
              const start = Math.min(Math.max(0, cur - LIST_MAX + 1), Math.max(0, n - LIST_MAX));
              return options.slice(start, start + LIST_MAX).map((opt, offset) => {
                const i = start + offset;
                const active = i === cur;
                return (
                  <box
                    key={opt.id}
                    flexDirection="row"
                    justifyContent="space-between"
                    paddingLeft={1}
                    paddingRight={1}
                    backgroundColor={active ? theme.selection : undefined}
                    onMouseDown={() => ask.resolve(opt.id)}
                  >
                    <text selectable={false} fg={active ? theme.bg : theme.fg}>{opt.label}</text>
                    <text selectable={false} fg={active ? theme.bg : theme.muted}>{opt.hint ?? ""}</text>
                  </box>
                );
              });
            })()}
            {n > LIST_MAX && (
              <box paddingLeft={1}>
                <text selectable={false} fg={theme.muted}>{cur + 1}/{n}</text>
              </box>
            )}
          </box>
        )}
      </box>

      {/* button strip: raised bg, buttons left, hints right */}
      <box
        flexDirection="row"
        flexShrink={0}
        gap={1}
        paddingTop={1}
        paddingBottom={1}
        paddingLeft={2}
        paddingRight={3}
        backgroundColor={theme.surfaceRaised}
        justifyContent="space-between"
        alignItems="center"
      >
        <box flexDirection="row" gap={1} flexShrink={0}>
          {!list &&
            ask.options.map((opt, i) => {
              const active = i === cur;
              return (
                <box
                  key={opt.id}
                  paddingLeft={1}
                  paddingRight={1}
                  backgroundColor={active ? theme.warning : undefined}
                  onMouseDown={() => ask.resolve(opt.id)}
                >
                  <text selectable={false} fg={active ? theme.bg : theme.muted}>{opt.label}</text>
                </box>
              );
            })}
        </box>
        <box flexDirection="row" gap={2} flexShrink={0}>
          <text selectable={false} fg={theme.fg}>
            {list ? "↑↓" : "⇆"} <span fg={theme.muted}>select</span>
          </text>
          <text selectable={false} fg={theme.fg}>
            enter <span fg={theme.muted}>confirm</span>
          </text>
          <text selectable={false} fg={theme.fg}>
            esc <span fg={theme.muted}>cancel</span>
          </text>
        </box>
      </box>
    </box>
  );
}
