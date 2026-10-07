import { useEffect, useRef, useState } from "react";
import { useKeyboard, usePaste } from "@opentui/react";
import { TextAttributes } from "@opentui/core";
import { theme } from "../theme.ts";

export const SPECIAL = new Set([
  "return", "enter", "linefeed", "tab", "backspace", "delete", "escape",
  "up", "down", "left", "right", "pageup", "pagedown", "home", "end", "insert",
]);

/**
 * Owned multiline input. Consumes ONLY plain printable keys and edit keys;
 * anything ctrl/meta is left for the app-level handler. There is no focus
 * state — this is always the text consumer.
 *
 * Newlines: Shift+Enter or Alt/Option+Enter inserts one; a trailing "\" +
 * Enter continues on the next line; plain Enter submits.
 */
/** Start of the `[Image N]` token ending at `p`, else one char back. */
function tokenStart(value: string, p: number): number {
  const m = /\[Image \d+\] ?$/.exec(value.slice(0, p));
  return m ? p - m[0].length : Math.max(0, p - 1);
}

/** End of the `[Image N]` token starting at `p`, else one char on. */
function tokenEnd(value: string, p: number): number {
  const m = /^\[Image \d+\] ?/.exec(value.slice(p));
  return m ? p + m[0].length : Math.min(value.length, p + 1);
}

export function Input({
  value,
  onChange,
  onSubmit,
  placeholder,
  showCursor,
  active = true,
  mask = false,
  onPasteImage,
  onHistory,
}: {
  value: string;
  onChange: (v: string) => void;
  /** `steer` is true when the submit asked to interrupt (ctrl+enter). */
  onSubmit: (v: string, steer?: boolean) => void;
  placeholder: string;
  showCursor: boolean;
  active?: boolean;
  /** Render bullets instead of the value (secrets). */
  mask?: boolean;
  /**
   * ctrl+v: hand back a placeholder to insert at the caret, or null when the
   * clipboard holds no image. Like opencode, the attachment is a token in the
   * value, so the cursor lands after it and later text goes where it is typed.
   */
  onPasteImage?: () => string | null;
  /**
   * ↑/↓ at the top/bottom line: the caret has nowhere to go inside the value,
   * so the recall belongs to whoever owns the history. Returns true when it
   * took the key.
   */
  onHistory?: (dir: -1 | 1) => boolean;
}) {
  const [cursor, setCursor] = useState(value.length);
  // Distinguish our own edits from external value changes (menu insert,
  // history recall): external changes move the cursor to the end.
  const expected = useRef(value);
  useEffect(() => {
    if (value !== expected.current) {
      expected.current = value;
      cursorRef.current = value.length;
      setCursor(value.length);
    }
  }, [value]);
  const cursorRef = useRef(cursor);
  const change = (v: string) => {
    expected.current = v;
    onChange(v);
  };
  const moveCursor = (c: number) => {
    cursorRef.current = c;
    setCursor(c);
  };
  const pos = Math.min(cursor, value.length);

  const insert = (text: string) => {
    const v = expected.current;
    const p = Math.min(cursorRef.current, v.length);
    change(v.slice(0, p) + text + v.slice(p));
    moveCursor(p + text.length);
  };

  usePaste((event) => {
    if (active)
      insert(new TextDecoder().decode(event.bytes).replace(/\r\n?/g, "\n"));
  });

  useKeyboard((key) => {
    if (!active) return;
    if (key.ctrl && key.name === "v") {
      const token = onPasteImage?.();
      if (token) insert(token);
      return;
    }
    // Live refs, not render props: a burst of keys in one stdin chunk runs
    // every callback against the same stale render.
    const v = expected.current;
    const p = Math.min(cursorRef.current, v.length);

    if (key.name === "return") {
      if (key.meta || key.shift || key.option) return insert("\n");
      if (v.endsWith("\\")) {
        // continuation: swap the trailing backslash for a newline
        change(v.slice(0, -1) + "\n");
        moveCursor(v.length);
        return;
      }
      // ctrl+enter steers (interrupt) rather than queueing. Only terminals
      // running the kitty protocol report the modifier — without it ctrl+enter
      // arrives as a bare \r, which is why ^s steers too.
      onSubmit(v, key.ctrl === true);
      // reset the live refs without onChange: the submit handler owns the
      // next value (may immediately set e.g. "/login "), and the external-
      // change effect re-syncs when that lands.
      expected.current = "";
      moveCursor(0);
      return;
    }
    if (key.ctrl || key.meta || key.option) return;
    if (key.name === "up" || key.name === "down") {
      // move within a multiline value first; only the edge falls through to
      // history, the way every editor-shaped prompt behaves
      const dir = key.name === "up" ? -1 : 1;
      const bol = v.lastIndexOf("\n", p - 1) + 1;
      const col = p - bol;
      if (dir === -1) {
        if (bol === 0) return void onHistory?.(-1);
        const prev = v.lastIndexOf("\n", bol - 2) + 1;
        return moveCursor(Math.min(prev + col, bol - 1));
      }
      const eol = v.indexOf("\n", p);
      if (eol === -1) return void onHistory?.(1);
      const nextEnd = v.indexOf("\n", eol + 1);
      return moveCursor(Math.min(eol + 1 + col, nextEnd === -1 ? v.length : nextEnd));
    }
    if (key.name === "left") return moveCursor(tokenStart(v, p));
    if (key.name === "right") return moveCursor(tokenEnd(v, p));
    if (key.name === "backspace" || key.name === "delete") {
      if (p > 0) {
        // an [Image N] badge is one thing on screen, so it deletes as one
        const from = tokenStart(v, p);
        change(v.slice(0, from) + v.slice(p));
        moveCursor(from);
      }
      return;
    }
    if (SPECIAL.has(key.name)) return;
    const text = key.sequence;
    if (text && !text.startsWith("\x1b") && text >= " ") insert(text);
  });

  if (value === "") {
    return (
      <text>
        {showCursor && <span attributes={TextAttributes.INVERSE}> </span>}
        <span fg={theme.placeholder}>{placeholder || " "}</span>
      </text>
    );
  }

  // An attachment token in the value renders as a highlighted badge, so the
  // text keeps the attachment's place and the cursor moves across it normally.
  const badged = (text: string, key: string) =>
    text.split(/(\[Image \d+\])/).map((part, i) =>
      /^\[Image \d+\]$/.test(part) ? (
        <span key={`${key}-${i}`} bg={theme.warning} fg={theme.bg} attributes={TextAttributes.BOLD}>
          {part}
        </span>
      ) : (
        <span key={`${key}-${i}`}>{part}</span>
      ),
    );

  // Render lines with the cursor on the right one.
  const display = mask ? "•".repeat(value.length) : value;
  const lines = display.split("\n");
  let offset = 0;
  return (
    <box flexDirection="column">
      {lines.map((line, i) => {
        const start = offset;
        const end = start + line.length;
        offset = end + 1; // account for the newline
        const cursorHere = showCursor && pos >= start && pos <= end;
        const col = pos - start;
        const lineStart = start;
        return (
          <text
            key={i}
            fg={theme.fg}
            // e.x is absolute; the renderable's own x makes it a column
            onMouseDown={(e: { x: number; target: { x: number } | null }) =>
              moveCursor(Math.min(lineStart + Math.max(0, e.x - (e.target?.x ?? 0)), end))
            }
          >
            {cursorHere ? (
              <>
                {badged(line.slice(0, col), `${i}a`)}
                <span attributes={TextAttributes.INVERSE}>{line[col] ?? " "}</span>
                {badged(line.slice(col + 1), `${i}b`)}
              </>
            ) : (
              badged(line || " ", `${i}`)
            )}
          </text>
        );
      })}
    </box>
  );
}
