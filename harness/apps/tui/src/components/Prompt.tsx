import { useEffect, useState } from "react";
import { useKeyboard } from "@opentui/react";
import { Input } from "./Input.tsx";
import { theme } from "../theme.ts";
import { MENU_MAX, type MenuItem } from "../slash.ts";
import { EmptyBorder, SplitBorder } from "../ui/border.ts";

/**
 * Input card, opencode's prompt pattern: native left border ┃ capped by ╹,
 * raised background, paddingX 2, context row under the input.
 */
export function Prompt({
  value,
  onChange,
  onSubmit,
  placeholder,
  model,
  provider,
  session,
  inputActive = true,
  onPasteImage,
  onHistory,
  menu,
  overlay,
  onPick,
}: {
  value: string;
  onChange: (v: string) => void;
  onSubmit: (v: string, steer?: boolean) => void;
  placeholder: string;
  model: string;
  provider: string;
  /** the session being typed into — its inherited title, never an agent name */
  session: string;
  inputActive?: boolean;
  /** ctrl+v: the placeholder token to insert at the caret, or null. */
  onPasteImage?: () => string | null;
  /** ↑/↓ at the first/last line of the value: recall prompt history. */
  onHistory?: (dir: -1 | 1) => boolean;
  menu: MenuItem[];
  /** "jump"/"command": filter overlays (Enter runs via onPick). "normal": dimmed card. */
  overlay?: "jump" | "command" | "normal";
  onPick?: (insert: string) => void;
}) {
  const matches = menu;
  const [sel, setSel] = useState(0);
  const normal = overlay === "normal";
  const bar = normal ? theme.border : theme.accent;

  // Clamp selection when the filter narrows.
  useEffect(() => {
    if (sel >= matches.length) setSel(0);
  }, [matches.length, sel]);

  useKeyboard((key) => {
    if (matches.length === 0 || !inputActive) return;
    if (key.name === "down") setSel((i) => (i + 1) % matches.length);
    if (key.name === "up") setSel((i) => (i - 1 + matches.length) % matches.length);
    if (key.name === "tab") onChange(matches[sel]!.insert);
  });

  function handleSubmit(text: string, steer?: boolean) {
    // Menu open → run the highlighted entry, not the partial text.
    if (matches.length > 0 && onPick) return onPick(matches[sel]!.insert);
    onSubmit(matches.length > 0 ? matches[sel]!.insert : text, steer);
  }

  return (
    <box flexDirection="column" width="100%" flexShrink={0}>
      {matches.length > 0 && (
        // opencode autocomplete: ┃ rails both sides, menu bg, paddingX 1
        <box {...SplitBorder} borderColor={theme.border} backgroundColor={theme.surfaceRaised}>
          <box flexDirection="column">
            {(() => {
              // scroll window that follows the selection
              const start = Math.min(
                Math.max(0, sel - MENU_MAX + 1),
                Math.max(0, matches.length - MENU_MAX),
              );
              return matches.slice(start, start + MENU_MAX).map((c, offset) => {
                const i = start + offset;
                const active = i === sel;
                return (
                  <box
                    key={c.insert}
                    flexDirection="row"
                    backgroundColor={active ? theme.selection : undefined}
                    paddingLeft={1}
                    paddingRight={1}
                    onMouseDown={onPick ? () => onPick(c.insert) : () => onSubmit(c.insert)}
                  >
                    <text selectable={false} fg={active ? theme.bg : theme.fg}>{c.label.padEnd(10)}</text>
                    <text selectable={false} fg={active ? theme.bg : theme.muted}> {c.hint}</text>
                    <box flexGrow={1} />
                  </box>
                );
              });
            })()}
          </box>
        </box>
      )}

      <box
        border={["left"]}
        borderColor={bar}
        customBorderChars={SplitBorder.customBorderChars}
      >
        <box
          flexDirection="column"
          paddingLeft={2}
          paddingRight={2}
          paddingTop={1}
          backgroundColor={theme.surfaceRaised}
        >
          <box minHeight={1}>
            <Input
              value={value}
              onChange={onChange}
              onSubmit={handleSubmit}
              placeholder={placeholder}
              showCursor={inputActive}
              active={inputActive}
              onPasteImage={onPasteImage}
              onHistory={onHistory}
            />
          </box>
          <text> </text>
          <text>
            <span fg={normal ? theme.muted : bar}>
              {/* every session is an agent, so naming the agent said nothing
                  twice — this is the session you are typing into, and it stays
                  put in NORMAL too; the mode goes in the hint that follows */}
              <b>{overlay === "command" ? "Commands" : overlay === "jump" ? "Jump" : session}</b>
            </span>
            {normal ? (
              <span fg={theme.muted}> · NORMAL · i to type · ? for help</span>
            ) : (
              // the model is which model answers, so it stays on screen while
              // an overlay is open — the overlay's hint is appended, not swapped in
              <span>
                <span fg={theme.muted}> · {model} </span>
                <span fg={theme.placeholder}>{provider}</span>
                {overlay ? (
                  <span fg={theme.muted}> · type to filter, esc to close</span>
                ) : null}
              </span>
            )}
          </text>
        </box>
      </box>

      {/* opencode's closing strip: ╹ cap in the border color, ▀ row in the
          card bg — a half-height bottom edge under the card */}
      <box
        height={1}
        border={["left"]}
        borderColor={bar}
        customBorderChars={{ ...EmptyBorder, vertical: "╹" }}
      >
        <box
          height={1}
          border={["bottom"]}
          borderColor={theme.surfaceRaised}
          customBorderChars={{ ...EmptyBorder, horizontal: "▀" }}
        />
      </box>
    </box>
  );
}
