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
  panel: { keys: "⌘B", label: "workspaces", match: (e) => meta(e) && !e.altKey && key(e, "b") },
  inspector: { keys: "⌘⌥B", label: "inspector", match: (e) => meta(e) && e.altKey && key(e, "b") },
  prevThread: { keys: "⌘[", label: "previous thread", match: (e) => meta(e) && e.key === "[" },
  nextThread: { keys: "⌘]", label: "next thread", match: (e) => meta(e) && e.key === "]" },
  close: { keys: "⌘W", label: "close the tab", match: (e) => meta(e) && key(e, "w") },
  back: { keys: "esc", label: "back", match: (e) => e.key === "Escape" },
  quickOpen: { keys: "⌘P", label: "go to…", match: (e) => meta(e) && !e.shiftKey && key(e, "p") },
  commands: { keys: "⌘⇧P", label: "commands", match: (e) => meta(e) && e.shiftKey && key(e, "p") },
  settings: { keys: "⌘,", label: "settings", match: (e) => meta(e) && e.key === "," },
  background: { keys: "^B", label: "background", match: (e) => e.ctrlKey && !e.metaKey && key(e, "b") },
  split: { keys: "⌘\\", label: "split right", match: (e) => meta(e) && e.key === "\\" },
  focusPane: { keys: "⌘⌥→", label: "next pane", match: (e) => meta(e) && e.altKey && e.key === "ArrowRight" },
  find: { keys: "⌘F", label: "find", match: (e) => meta(e) && key(e, "f") },
  workspaces: { keys: "⌘T", label: "switch workspace", match: (e) => meta(e) && key(e, "t") },
} as const satisfies Record<string, Binding>;

/** ⌘1…⌘9 select a thread tab by position; listed in the command palette as one row. */
export function threadIndex(e: KeyboardEvent): number | undefined {
  if (!meta(e) || e.altKey) return undefined;
  const n = Number(e.key);
  return n >= 1 && n <= 9 ? n - 1 : undefined;
}

export const HINTS: Binding[] = Object.values(KEYS).filter((b) => "hint" in b && b.hint);
