/**
 * The whole keymap, in one place.
 *
 * Only bindings that do something are here: a shortcut that opens a thing which
 * is not built yet teaches a person a lie. The hints under the composer are
 * generated from this list, so the two can never drift.
 */
export type Binding = {
  keys: string;      // as a person reads it
  label: string;     // what it does
  hint?: boolean;    // shown under the composer
  /**
   * The same command under opencode's leader (`ctrl+x`, `keybind.ts:39`). It is an ALIAS, not a
   * replacement: a person who came from opencode presses `ctrl+x b` and a person who came from an
   * editor presses ⌘B, and both open the same thing.
   */
  leader?: string;
  match: (e: KeyboardEvent) => boolean;
};

const meta = (e: KeyboardEvent) => e.metaKey || e.ctrlKey;
const key = (e: KeyboardEvent, k: string) => e.key.toLowerCase() === k;

export const KEYS = {
  send: { keys: "↩", label: "send", hint: true, match: (e) => e.key === "Enter" && !e.shiftKey && !e.altKey && !e.isComposing && (e.target as HTMLElement | null)?.hasAttribute("data-composer") === true },
  newline: { keys: "⇧↩", label: "newline", hint: true, match: (e) => e.key === "Enter" && e.shiftKey },
  steer: { keys: "⌘↩", label: "steer now", match: (e) => meta(e) && e.key === "Enter" && (e.target as HTMLElement | null)?.hasAttribute("data-composer") === true },
  composer: { keys: "⌘L", label: "prompt", hint: true, match: (e) => meta(e) && key(e, "l") },
  shell: { keys: "⌘J", label: "shell", hint: true, match: (e) => meta(e) && key(e, "j") },
  environment: { keys: "⌘E", label: "environment", match: (e) => meta(e) && key(e, "e") },
  panel: { keys: "⌘B", label: "workspaces", leader: "b", match: (e) => meta(e) && !e.altKey && key(e, "b") },
  inspector: { keys: "⌘⌥B", label: "inspector", leader: "s", match: (e) => meta(e) && e.altKey && key(e, "b") },
  prevThread: { keys: "⌘[", label: "previous thread", match: (e) => meta(e) && e.key === "[" },
  nextThread: { keys: "⌘]", label: "next thread", match: (e) => meta(e) && e.key === "]" },
  close: { keys: "⌘W", label: "close the tab", leader: "q", match: (e) => meta(e) && key(e, "w") },
  back: { keys: "esc", label: "back", match: (e) => e.key === "Escape" },
  quickOpen: { keys: "⌘P", label: "go to…", leader: "l", match: (e) => meta(e) && !e.shiftKey && key(e, "p") },
  commands: { keys: "⌘⇧P", label: "commands", match: (e) => meta(e) && e.shiftKey && key(e, "p") },
  settings: { keys: "⌘,", label: "settings", match: (e) => meta(e) && e.key === "," },
  background: { keys: "^B", label: "background", match: (e) => e.ctrlKey && !e.metaKey && key(e, "b") },
  split: { keys: "⌘\\", label: "split right", match: (e) => meta(e) && e.key === "\\" },
  focusPane: { keys: "⌘⌥→", label: "next pane", match: (e) => meta(e) && e.altKey && e.key === "ArrowRight" },
  find: { keys: "⌘F", label: "find", match: (e) => meta(e) && key(e, "f") },
  workspaces: { keys: "⌘T", label: "switch workspace", match: (e) => meta(e) && key(e, "t") },
  // opencode's own three, on our shapes: Build/Plan, how hard the model thinks, and the palette.
  mode: { keys: "⇧tab", label: "cycle mode", hint: true, match: (e) => e.key === "Tab" && e.shiftKey && !meta(e) && !e.altKey },
  level: { keys: "^T", label: "thinking level", match: (e) => e.ctrlKey && !e.metaKey && key(e, "t") },
  palette: { keys: "^P", label: "commands", leader: "p", hint: true, match: (e) => e.ctrlKey && !e.metaKey && key(e, "p") },
} as const satisfies Record<string, Binding>;

/**
 * opencode's leader key. Pressing it arms the next keystroke: `ctrl+x b` is the sidebar, `ctrl+x l`
 * is the session list, `ctrl+x 1…9` a session by slot. It forgets after three seconds, because a
 * leader that stays armed eats the next thing a person types.
 */
export const LEADER: Binding = { keys: "^X", label: "leader", match: (e) => e.ctrlKey && !e.metaKey && !e.altKey && key(e, "x") };
export const LEADER_FORGET_MS = 3000;

/** Which binding a key means while the leader is armed, if any. */
export function underLeader(e: KeyboardEvent): Binding | undefined {
  return Object.values(KEYS).find((b) => "leader" in b && b.leader && key(e, b.leader));
}

/** `ctrl+x 1…9` picks a session by slot, the way ⌘1…9 picks a tab. */
export function leaderIndex(e: KeyboardEvent): number | undefined {
  const n = Number(e.key);
  return n >= 1 && n <= 9 ? n - 1 : undefined;
}

/** Both ways of saying it, for the palette: `⌘B  ^X B`. A hidden alias is not an alias. */
export const keyHint = (b: Binding): string => (b.leader ? `${b.keys}  ${LEADER.keys} ${b.leader.toUpperCase()}` : b.keys);

/** ⌘1…⌘9 select a thread tab by position; listed in the command palette as one row. */
export function threadIndex(e: KeyboardEvent): number | undefined {
  if (!meta(e) || e.altKey) return undefined;
  const n = Number(e.key);
  return n >= 1 && n <= 9 ? n - 1 : undefined;
}

export const HINTS: Binding[] = Object.values(KEYS).filter((b) => "hint" in b && b.hint);


/**
 * Whether an app shortcut may act at all. A terminal is a program that owns the keyboard: while
 * its element has focus, every unmodified key belongs to the shell — the owner typed `c` and zsh
 * went into `bck-i-search`, because an app handler on `document` acted on a key xterm was also
 * delivering (2026-09-17).
 *
 * The exceptions are the chords a terminal cannot mean: ⌘/^ with a letter, and shift+tab. Plain
 * Escape, plain Enter, plain arrows and every bare letter go to the PTY and nowhere else.
 */
export function mayAct(e: { key?: string; metaKey?: boolean; ctrlKey?: boolean; shiftKey?: boolean }, inTerminal: boolean): boolean {
  if (!inTerminal) return true;
  if (e.metaKey || e.ctrlKey) return true;
  return e.key === "Tab" && !!e.shiftKey;
}

/** Is the keyboard inside a terminal? xterm focuses its own textarea inside `.xterm`. */
export const inTerminal = (target: unknown): boolean =>
  !!target && typeof (target as { closest?: unknown }).closest === "function" && !!(target as Element).closest(".xterm");
