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
  send: { keys: "⌘↩", label: "send", hint: true, match: (e) => meta(e) && e.key === "Enter" },
  composer: { keys: "⌘L", label: "focus the prompt", hint: true, match: (e) => meta(e) && key(e, "l") },
  shell: { keys: "⌘J", label: "shell", hint: true, match: (e) => meta(e) && key(e, "j") },
  environment: { keys: "⌘E", label: "environment", hint: true, match: (e) => meta(e) && key(e, "e") },
  panel: { keys: "⌘B", label: "workspaces", match: (e) => meta(e) && !e.altKey && key(e, "b") },
  inspector: { keys: "⌘⌥B", label: "inspector", match: (e) => meta(e) && e.altKey && key(e, "b") },
  prevThread: { keys: "⌘[", label: "previous thread", match: (e) => meta(e) && e.key === "[" },
  nextThread: { keys: "⌘]", label: "next thread", match: (e) => meta(e) && e.key === "]" },
  close: { keys: "⌘W", label: "close what is open", match: (e) => meta(e) && key(e, "w") },
  back: { keys: "esc", label: "back", match: (e) => e.key === "Escape" },
} as const satisfies Record<string, Binding>;

/** ⌘1…⌘9 select a thread by position. */
export function threadIndex(e: KeyboardEvent): number | undefined {
  if (!meta(e) || e.altKey) return undefined;
  const n = Number(e.key);
  return n >= 1 && n <= 9 ? n - 1 : undefined;
}

export const HINTS: Binding[] = Object.values(KEYS).filter((b) => "hint" in b && b.hint);
