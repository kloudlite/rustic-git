import { useEffect, useRef, useState } from "react";
import { useKeyboard } from "@opentui/react";
import { SyntaxStyle, TextAttributes, type ScrollBoxRenderable } from "@opentui/core";
import { useWheelAccel } from "../wheel.ts";
import { theme } from "../theme.ts";
import { SplitBorder } from "../ui/border.ts";
import { DiffView } from "./Diff.tsx";
import type { FileDiff } from "../diff.ts";
import { foldRepeats } from "../retry.ts";

export type Entry =
  | { kind: "user"; id?: string; text: string; images?: number }
  | { kind: "agent"; id?: string; text: string }
  | {
      kind: "thinking";
      id?: string;
      text: string;
      /** the message finished streaming — render the whole block, not a ticker */
      done?: boolean;
    }
  | {
      kind: "tool";
      id?: string;
      name: string;
      summary: string;
      status?: "running" | "ok" | "error";
      /** Streaming / final output (rendered for bash blocks). */
      output?: string;
      error?: string;
      /** Markdown a codemode script showed the user (`tools.display`), always expanded. */
      display?: string[];
      /** Unified diff hunk (edit/write tools). */
      diff?: FileDiff;
      /** Times this row replaced a failed one (see retry.ts). */
      retries?: number;
      /** Consecutive identical codemode inner calls folded into this row. */
      repeats?: number;
    }
  | { kind: "info"; text: string }
  | { kind: "error"; text: string };

const retryTag = (e: { retries?: number }) =>
  e.retries && e.retries > 0 ? <span fg={theme.muted}>{` · retry ${e.retries}`}</span> : "";

/** Rows kept when a block is collapsed (Claude Code shows a short head). */
const COLLAPSE_MAX = 10;
/** Columns a thinking block loses to its left rail: the border plus its padding. */
const RAIL = 3;

/**
 * Soft-wrap one source line to `width`, the way the terminal will draw it, so
 * a row count means rows on screen. Reasoning and bash output arrive as long
 * unbroken paragraphs: counting "\n" said 3 lines where the terminal drew 9,
 * and the block was then sized for 3 — the expander landed on top of the text.
 */
function wrap(line: string, width: number): string[] {
  if (line.length <= width) return [line];
  const rows: string[] = [];
  let rest = line;
  while (rest.length > width) {
    // break on the last space that fits; a word longer than the width is cut
    const cut = rest.lastIndexOf(" ", width);
    const at = cut > 0 ? cut : width;
    rows.push(rest.slice(0, at));
    rest = rest.slice(cut > 0 ? at + 1 : at);
  }
  if (rest) rows.push(rest);
  return rows;
}

/**
 * Collapse a block to its first `COLLAPSE_MAX` rows — Claude Code keeps the
 * head, not the tail, so a long block still reads from its beginning. Returns
 * the kept text and how many rows it hid, so the caller can offer the expander.
 * `width` is the column count the block is drawn into; rows are counted after
 * wrapping to it, since that is what the user actually sees.
 */
function collapse(
  text: string | undefined,
  open: boolean,
  width = Infinity,
): { text: string; hidden: number } {
  if (!text) return { text: "", hidden: 0 };
  const rows = text
    .trim()
    .split("\n")
    .flatMap((line) => wrap(line, width));
  if (open || rows.length <= COLLAPSE_MAX) return { text: rows.join("\n"), hidden: 0 };
  return { text: rows.slice(0, COLLAPSE_MAX).join("\n"), hidden: rows.length - COLLAPSE_MAX };
}

/**
 * Collapse a markdown block. Unlike `collapse`, this cuts on *source* lines and
 * never rejoins soft-wrapped fragments: a table row or a fenced block is one
 * source line that must reach the markdown parser whole, and splitting it at
 * the terminal width turns a table into loose pipes. The screen rows are still
 * what decides *whether* to cut, so a long paragraph collapses as before; only
 * the cut itself lands on a line boundary.
 *
 * An unterminated code fence in the kept head would swallow the rest of the
 * block, so one is closed off.
 */
function collapseMd(
  text: string | undefined,
  open: boolean,
  width: number,
): { text: string; hidden: number } {
  if (!text) return { text: "", hidden: 0 };
  const lines = text.trim().split("\n");
  if (open || rowCount(text, width) <= COLLAPSE_MAX) return { text: lines.join("\n"), hidden: 0 };
  // take source lines until their wrapped height reaches the head budget
  const kept: string[] = [];
  let rows = 0;
  for (const line of lines) {
    const h = wrap(line, width).length;
    if (rows + h > COLLAPSE_MAX && kept.length > 0) break;
    kept.push(line);
    rows += h;
  }
  if (kept.filter((l) => l.trimStart().startsWith("```")).length % 2 === 1) kept.push("```");
  return { text: kept.join("\n"), hidden: lines.length - kept.length };
}

/**
 * Whether this entry has a block long enough to collapse — the wrapper needs
 * to know before rendering the Row, so the whole cell can be the toggle and
 * only entries that actually collapse react to a click.
 */
/**
 * Rows the closed block actually cuts. Script and output collapse separately,
 * so together they can pass COLLAPSE_MAX while neither is cut: that drew
 * "… +0 lines  Click to expand" under a codemode poll (2026-10-09).
 */
function shellHidden(entry: Entry & { kind: "tool" }, width: number): number {
  return collapse(entry.summary, false, width - 2).hidden + collapse(entry.output, false, width - 2).hidden;
}

function collapsible(entry: Entry, width: number): boolean {
  if (entry.kind === "agent") return rowCount(entry.text, width - 3) > COLLAPSE_MAX;
  if (entry.kind === "thinking")
    return !!entry.done && rowCount(entry.text, width - RAIL) > COLLAPSE_MAX;
  if (entry.kind === "tool" && (entry.name === "bash" || entry.name === "codemode"))
    return shellHidden(entry, width) > 0;
  return false;
}

/** Rows a block will occupy once wrapped — decides if it needs an expander. */
function rowCount(text: string | undefined, width: number): number {
  if (!text) return 0;
  return text.trim().split("\n").flatMap((line) => wrap(line, width)).length;
}

/**
 * opencode's expander, under the block it belongs to: "… +N lines  Click to
 * expand", and once open the same row offers to collapse it again. Click only
 * — opencode has no expand key and neither do we.
 */
function More({
  hidden,
  open,
  onToggle,
  hover,
}: {
  hidden: number;
  open: boolean;
  onToggle?: () => void;
  /** the pointer is over this entry — the row brightens to say it is clickable */
  hover?: boolean;
}) {
  return (
    <box
      height={1}
      width="100%"
      // the cell around this row also toggles on a click (mouseup); without
      // the stop, one click opened here and closed again on release (2026-10-08)
      onMouseDown={onToggle && ((e: { stopPropagation(): void }) => (e.stopPropagation(), onToggle()))}
    >
      <text fg={hover ? theme.fg : theme.muted} selectable={false}>
        {open ? "" : `… +${hidden} lines  `}
        {open ? "Click to collapse" : "Click to expand"}
      </text>
    </box>
  );
}

/**
 * Syntax colours for fenced code inside a markdown block. opentui wants a
 * SyntaxStyle object rather than a palette, and building one allocates on the
 * native side, so it is cached. `setTheme` mutates the palette in place, so
 * the cache is keyed on the colours themselves — a module-level constant
 * would keep the colours the first theme happened to have.
 */
let mdStyleCache: { key: string; style: SyntaxStyle } | null = null;
function mdSyntaxStyle(): SyntaxStyle {
  const key = `${theme.fg}${theme.accent}${theme.success}${theme.warning}${theme.muted}`;
  if (mdStyleCache?.key !== key) {
    mdStyleCache = {
      key,
      style: SyntaxStyle.fromStyles({
        default: { fg: theme.fg },
        keyword: { fg: theme.accent },
        string: { fg: theme.success },
        number: { fg: theme.warning },
        comment: { fg: theme.muted, italic: true },
        function: { fg: theme.accent },
        type: { fg: theme.warning },
        variable: { fg: theme.fg },
        punctuation: { fg: theme.muted },
      }),
    };
  }
  return mdStyleCache.style;
}

/**
 * A markdown block, rendered by opentui's own parser — tables, lists,
 * headings, blockquotes and fenced code all draw properly. The hand-rolled
 * version this replaces understood only `**bold**` and `` `code` ``, so a
 * table arrived as its raw pipes and a list kept its literal dashes.
 *
 * `streaming` keeps the trailing block unstable while chunks are still
 * arriving, which is what stops a half-written table row from being parsed as
 * final; it is turned off once the message is done so the last token settles.
 */
export function Md({ text, fg, streaming }: { text: string; fg?: string; streaming?: boolean }) {
  return (
    <markdown
      content={text}
      syntaxStyle={mdSyntaxStyle()}
      fg={fg ?? theme.fg}
      streaming={streaming}
      // "grid" draws the full box rule around every cell, which is what makes
      // a table legible in a transcript that has no other column structure.
      tableOptions={{ style: "grid", borderColor: theme.border }}
    />
  );
}

const inlineIcon: Record<string, string> = {
  read: "→",
  glob: "→",
  grep: "→",
  write: "←",
  edit: "←",
  web_fetch: "↓",
  web_search: "⌕",
};

const inlineVerb: Record<string, string> = {
  read: "Read",
  glob: "Glob",
  grep: "Grep",
  write: "Write",
  edit: "Edit",
  web_fetch: "Fetch",
  web_search: "Search",
};

/** Is this entry a one-line inline tool row (stacks tight, opencode-style)? */
function isInlineTool(entry: Entry): boolean {
  return entry.kind === "tool" && entry.name !== "bash" && entry.name !== "codemode";
}

/** Cap on rendered entries; older ones fall out of the scrollback. */
const SCROLLBACK = 200;

function Row({
  entry,
  open,
  onOpen,
  width,
  hover,
  streaming,
}: {
  entry: Entry;
  /** this entry is expanded — render every line */
  open: boolean;
  onOpen?: () => void;
  /** columns the row is drawn into — collapse counts wrapped rows at it */
  width: number;
  /** the pointer is over this entry and it can collapse */
  hover?: boolean;
  /** the last entry, so markdown may still be mid-token */
  streaming?: boolean;
}) {
  switch (entry.kind) {
    case "user":
      // opencode UserMessage: native left border ┃ on the panel background
      return (
        <box
          border={["left"]}
          customBorderChars={SplitBorder.customBorderChars}
          borderColor={theme.accent}
        >
          <box flexDirection="column" paddingLeft={2} paddingTop={1} paddingBottom={1} backgroundColor={theme.surface}>
            <text fg={theme.fg}>{entry.text}</text>
            {entry.images ? (
              // where the attachments came from, as a label/value badge pair
              <>
                <text> </text>
                <box flexDirection="row" height={1}>
                  <box backgroundColor={theme.accent} paddingLeft={1} paddingRight={1}>
                    <text fg={theme.bg}>{entry.images === 1 ? "File" : `${entry.images} Files`}</text>
                  </box>
                  <box backgroundColor={theme.surfaceRaised} paddingLeft={1} paddingRight={1}>
                    <text fg={theme.muted}>clipboard</text>
                  </box>
                </box>
              </>
            ) : null}
          </box>
        </box>
      );
    case "agent": {
      // opencode TextPart: markdown, paddingLeft 3
      const body = collapseMd(entry.text, open, width - 3);
      // the expander stays visible once open, so the block can be re-collapsed
      const long = rowCount(entry.text, width - 3) > COLLAPSE_MAX;
      return (
        // hovering a collapsible block tints it, the way a desktop list row
        // lights up under the pointer — subtle, one step off the background
        // a long block keeps one row above and below at all times, so the tint
        // has room around the text and hovering never shifts the layout
        <box
          flexDirection="column"
          paddingLeft={3}
          paddingTop={long ? 1 : 0}
          paddingBottom={long ? 1 : 0}
          backgroundColor={hover ? theme.surface : undefined}
        >
          <Md text={body.text} streaming={streaming} />
          {long && <More hidden={body.hidden} open={open} onToggle={onOpen} hover={hover} />}
        </box>
      );
    }
    case "thinking": {
      // while streaming, reasoning is a progress ticker: its newest line only
      if (!entry.done) {
        const lines = entry.text.trim().split("\n");
        return (
          <box
            key="think-tick"
            border={["left"]}
            borderColor={theme.border}
            paddingLeft={2}
            height={1}
            overflow="hidden"
          >
            {/* one row, cut with an ellipsis */}
            <text fg={theme.muted} attributes={TextAttributes.ITALIC} wrapMode="none" truncate>
              {lines[lines.length - 1] ?? ""}
            </text>
          </box>
        );
      }
      // finished: readable, but it must not look like the answer. Reasoning is
      // prose the model wrote to itself, so it renders as dim italic text
      // behind a left rail — never through <markdown>, whose own heading and
      // bold colours override `fg` and made a thinking block indistinguishable
      // from an agent message.
      // the rail (1 col) plus its padding (2) — the same width `collapsible`
      // measures with, or the two disagree by a row and the expander is drawn
      // into a box that was never sized for it
      const body = collapse(entry.text, open, width - RAIL);
      const long = rowCount(entry.text, width - RAIL) > COLLAPSE_MAX;
      // distinct keys remount the box when `done` flips; a reused instance kept
      // height 1 (opentui ignores a prop removed to null) and drew the header
      // under the body ("Ihshould", 2026-10-08)
      return (
        <box
          key="think-done"
          flexDirection="column"
          border={["left"]}
          borderColor={theme.border}
          paddingLeft={2}
          backgroundColor={hover ? theme.surface : undefined}
        >
          <text fg={theme.muted} attributes={TextAttributes.ITALIC | TextAttributes.BOLD}>
            Thinking
          </text>
          <text fg={theme.muted} attributes={TextAttributes.ITALIC}>
            {body.text}
          </text>
          {long && <More hidden={body.hidden} open={open} onToggle={onOpen} hover={hover} />}
        </box>
      );
    }
    case "tool": {
      const running = entry.status === "running";
      const failed = entry.status === "error";

      // codemode: the model writes a script that calls the other tools, so the
      // script is the interesting part — same block as bash, with the source
      // where the command goes. Nested tool calls never reach us; pi returns
      // only the script's own output.
      if (entry.name === "bash" || entry.name === "codemode") {
        // opencode Shell via BlockTool: panel bg block, $ command, output tail
        const out = collapse(entry.output, open, width - 2);
        const script = collapse(entry.summary, open, width - 2);
        return (
          <box
            flexDirection="column"
            paddingLeft={2}
            paddingTop={1}
            paddingBottom={1}
            backgroundColor={hover ? theme.surfaceRaised : theme.surface}
          >
            {entry.name === "codemode" ? (
              <box flexDirection="column">
                <text fg={running ? theme.fg : theme.accent}>
                  {running ? "⚙ codemode" : "⌁ codemode"}
                  {retryTag(entry)}
                </text>
                <text fg={theme.muted}>{script.text}</text>
              </box>
            ) : (
              <text fg={running ? theme.fg : theme.muted}>
                {running ? "⚙ " : "$ "}
                {script.text}
                {retryTag(entry)}
              </text>
            )}
            {entry.name === "codemode" && out.text !== "" && (
              <text fg={theme.border}>{"─".repeat(Math.max(0, width - 4))}</text>
            )}
            {out.text !== "" && <text fg={theme.muted}>{out.text}</text>}
            {shellHidden(entry, width) > 0 && (
              // a long codemode script collapses too, so its cut rows count as hidden
              <More hidden={script.hidden + out.hidden} open={open} onToggle={onOpen} hover={hover} />
            )}
            {entry.error && <text fg={theme.error}>{entry.error}</text>}
            {entry.display?.map((md, i) => (
              <box key={i} flexDirection="column" paddingTop={1}>
                <Md text={md} />
              </box>
            ))}
          </box>
        );
      }

      if (entry.diff) {
        // Claude Code's Update block: ● Update(path), stats line, diff hunk
        const verb = entry.name === "write" ? "Write" : "Update";
        const stats = [
          entry.diff.added && `Added ${entry.diff.added} line${entry.diff.added === 1 ? "" : "s"}`,
          entry.diff.removed && `removed ${entry.diff.removed} line${entry.diff.removed === 1 ? "" : "s"}`,
        ]
          .filter(Boolean)
          .join(", ");
        return (
          <box flexDirection="column" paddingLeft={1}>
            <text>
              <span fg={failed ? theme.error : theme.success}>●</span>
              <span fg={theme.fg}> {verb}</span>
              <span fg={theme.muted}>({entry.diff.path})</span>
            </text>
            <box paddingLeft={2}>
              <text fg={theme.muted}>⎿ {stats}</text>
            </box>
            <box paddingLeft={2} flexDirection="column">
              <DiffView diff={entry.diff} />
            </box>
            {entry.error && (
              <box paddingLeft={2}>
                <text fg={theme.error}>{entry.error}</text>
              </box>
            )}
          </box>
        );
      }

      // opencode InlineToolRow: icon column (width 2) + content, tight rows
      const icon = failed ? "✗" : (inlineIcon[entry.name] ?? "·");
      const verb = inlineVerb[entry.name] ?? entry.name;
      const fg = failed ? theme.error : running ? theme.fg : theme.muted;
      return (
        <box flexDirection="column" paddingLeft={3}>
          <box flexDirection="row">
            <box width={2} flexShrink={0}>
              <text fg={fg}>{running ? "⚙" : icon}</text>
            </box>
            <text fg={fg}>
              {verb}{" "}
              <span attributes={failed ? undefined : TextAttributes.DIM}>
                {entry.summary}
              </span>
              {retryTag(entry)}
              {entry.repeats && entry.repeats > 1 ? <span fg={theme.muted}>{` ×${entry.repeats}`}</span> : ""}
            </text>
          </box>
          {entry.error && (
            <box paddingLeft={2}>
              <text fg={theme.error}>{entry.error}</text>
            </box>
          )}
        </box>
      );
    }
    case "info":
      return (
        <box paddingLeft={3}>
          <text fg={theme.muted}>
            {entry.text}
          </text>
        </box>
      );
    case "error":
      return (
        <box
          border={["left"]}
          customBorderChars={SplitBorder.customBorderChars}
          borderColor={theme.error}
        >
          <box paddingLeft={2} paddingTop={1} paddingBottom={1} backgroundColor={theme.surface}>
            <text fg={theme.error}>{entry.text}</text>
          </box>
        </box>
      );
  }
}

/**
 * Message scrollback on opentui's scrollbox: cell-accurate compositing,
 * native mouse-wheel scrolling, sticky bottom while streaming (opencode's
 * own setup). PageUp/PageDown page manually via the scrollbox ref.
 */
export function Transcript({
  entries,
  keys = "page",
  width = 80,
  ready = true,
}: {
  entries: Entry[];
  /** false until the persisted transcript has been read — see `Session.restored` */
  ready?: boolean;
  /** content columns available — long blocks wrap, so row counts need it */
  width?: number;
  /** "off" while a modal owns keys; "page" = pgup/pgdn; "normal" adds u/d. */
  keys?: "off" | "page" | "normal";
}) {
  const scrollRef = useRef<ScrollBoxRenderable>(null);
  const wheel = useWheelAccel();
  // scrolled up far enough that new output lands off screen — shows the
  // jump-to-bottom affordance, which `stickyScroll` otherwise hides
  const [away, setAway] = useState(false);
  // keys of entries the user expanded, one click at a time — opencode has no
  // expand-everything key, so neither do we
  const [open, setOpen] = useState<Set<string>>(new Set());
  // the entry the pointer is over, so its expander can light up
  const [hover, setHover] = useState<string | null>(null);
  // where the button went down, so an up in the same cell is a click and an
  // up somewhere else is the end of a text selection. opentui marks every up
  // isDragging once a mousedown over selectable text has started a selection,
  // so that flag alone cannot tell the two apart.
  const downAt = useRef<{ x: number; y: number } | null>(null);

  // ponytail: opentui's scrollbox has no onScroll, so "am I at the bottom?"
  // is sampled on a timer — cheap, and the only hook the wheel also trips
  useEffect(() => {
    const id = setInterval(() => {
      const sb = scrollRef.current;
      if (sb) setAway(!atBottom(sb));
    }, 200);
    return () => clearInterval(id);
  }, []);

  const atBottom = (sb: ScrollBoxRenderable) =>
    sb.scrollTop >= sb.scrollHeight - sb.viewport.height - 1;
  const toBottom = () => {
    const sb = scrollRef.current;
    if (!sb) return;
    sb.scrollTop = sb.scrollHeight;
    setAway(false);
  };

  useKeyboard((key) => {
    if (keys === "off") return;
    const sb = scrollRef.current;
    if (!sb) return;
    const page = Math.max(1, sb.viewport.height - 2);
    if (key.name === "pageup") sb.scrollBy(-page);
    if (key.name === "pagedown") sb.scrollBy(page);
    if (key.name === "end" || (keys === "normal" && key.sequence === "G")) toBottom();
    if (keys === "normal" && !key.ctrl && !key.meta) {
      if (key.name === "u") sb.scrollBy(-Math.ceil(page / 2));
      if (key.name === "d") sb.scrollBy(Math.ceil(page / 2));
    }
    setAway(!atBottom(sb));
  });

  const visible = foldRepeats(entries).slice(-SCROLLBACK);

  // An unrestored session has nothing to say yet: the welcome screen means
  // "this session is empty", and showing it before the transcript is read
  // flashed it over every reloaded session.
  if (visible.length === 0 && !ready) return <box flexGrow={1} />;

  if (visible.length === 0) {
    return (
      <box flexGrow={1} flexDirection="column" paddingLeft={1} gap={1}>
        {/* opencode-style home: block wordmark, tagline, quiet shortcut table */}
        <box flexDirection="row">
          <ascii-font font="tiny" text="kloud" color={theme.muted} />
          <ascii-font font="tiny" text="lite" color={theme.accent} />
        </box>
        <text fg={theme.muted}>Orchestrate agents across your kloudlite workspaces.</text>
        <box flexDirection="column" marginTop={1}>
          {(
            [
              ["/", "commands"],
              ["^p", "jump to a workspace"],
              ["^1-9", "jump to workspace · ^0 main"],
              ["^j ^k", "cycle workspaces"],
              ["^f", "files, then processes & their logs"],
              ["^b", "back to the main context"],
            ] as const
          ).map(([key, label]) => (
            <box key={key} flexDirection="row" height={1} flexShrink={0} overflow="hidden">
              <box width={7} flexShrink={0}>
                <text fg={theme.fg}>{key}</text>
              </box>
              <text fg={theme.muted}>{label}</text>
            </box>
          ))}
        </box>
      </box>
    );
  }

  return (
    <box flexGrow={1} flexBasis={0} flexShrink={1} flexDirection="column">
    <scrollbox
      ref={scrollRef}
      // basis 0 + shrink: yoga's flex-basis auto would size the scrollbox to its
      // content and shove the prompt/hint bar off screen
      flexGrow={1}
      flexBasis={0}
      flexShrink={1}
      stickyScroll
      stickyStart="bottom"
      // no scrollbar anywhere in the TUI (owner ruling); wheel and keys still scroll
      scrollbarOptions={{ visible: false }}
      scrollAcceleration={wheel}
    >
      {/* opencode: one blank row above the first message */}
      <box height={1} />
      {visible.map((entry, i) => {
        const key = "id" in entry && entry.id ? entry.id : `e${i}`;
        // the whole cell is the expander, so a long block does not have to be
        // scrolled past to reach its "… +N lines" row
        const canCollapse = collapsible(entry, width);
        const toggle = () =>
          setOpen((prev) => {
            const next = new Set(prev);
            if (!next.delete(key)) next.add(key);
            return next;
          });
        return (
        <box
          key={key}
          flexDirection="column"
          // opencode sibling margins: consecutive inline tool rows stack
          // tight; everything else separates by one blank line
          marginTop={
            i === 0 ? 0 : isInlineTool(entry) && isInlineTool(visible[i - 1]!) ? 0 : 1
          }
          // mouseup, not mousedown: a mousedown on the body starts a text
          // selection, so toggling there would make a block impossible to
          // select from. A click is an up within a cell of where it went down.
          onMouseDown={canCollapse ? (e: { x: number; y: number }) => {
            downAt.current = { x: e.x, y: e.y };
          } : undefined}
          onMouseUp={canCollapse ? (e: { x: number; y: number }) => {
            const from = downAt.current;
            downAt.current = null;
            if (from && Math.abs(from.x - e.x) < 2 && from.y === e.y) toggle();
          } : undefined}
          onMouseOver={canCollapse ? () => setHover(key) : undefined}
          onMouseOut={canCollapse ? () => setHover((h) => (h === key ? null : h)) : undefined}
        >
          <Row
            width={width}
            entry={entry}
            hover={canCollapse && hover === key}
            // only the final entry can still be mid-token; settling the
            // earlier ones lets their trailing markdown parse as final
            streaming={i === visible.length - 1}
            open={open.has(key)}
            onOpen={toggle}
          />
        </box>
        );
      })}
    </scrollbox>
      {away && (
        <box height={1} justifyContent="center" onMouseDown={toBottom}>
          <text selectable={false} fg={theme.accent}>
            ↓ jump to bottom <span fg={theme.muted}>end</span>
          </text>
        </box>
      )}
    </box>
  );
}
