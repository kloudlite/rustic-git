import type { Entry } from "./components/Transcript.tsx";
import { DEFAULT_MODEL, type ModelRef } from "./models.ts";

/**
 * Hierarchical sessions: a working session has main sessions (orchestrators)
 * and every workspace has its own — both can be named and kept side by side.
 * Turns keep running in whichever session started them, regardless of what the
 * user is currently looking at. The connected environment is a property of the
 * session, not part of its address, so `/env` moves the whole session across
 * without swapping any transcript.
 */
export type Session = {
  entries: Entry[];
  /**
   * The persisted transcript has been read off disk. Until then an empty
   * `entries` means "not loaded yet", not "nothing to show" — rendering the
   * welcome screen on that guess flashed it over every reloaded session.
   */
  restored?: boolean;
  busy: boolean;
  tokens: number;
  /** Prompts submitted in this session, oldest first (↑/↓ recall). */
  history: string[];
  model: ModelRef;
  /**
   * Steering / follow-up prompts queued while the agent streams, in delivery
   * order. `kind` decides which SDK queue an edit re-queues them onto.
   */
  queued: QueuedMessage[];
};

export type QueuedMessage = { text: string; kind: "steer" | "followUp" };

const emptySession: Session = { entries: [], busy: false, tokens: 0, history: [], model: DEFAULT_MODEL, queued: [] };

/** The workspace a session key belongs to: `main`, `ws-…`. */
export function baseOf(key: string): string {
  return key.split(":")[0]!;
}

/** First pending ask for the workspace the user is looking at; asks keep arrival order. */
export function askFor<T extends { key: string }>(asks: T[], activeKey: string): T | undefined {
  const base = baseOf(activeKey);
  return asks.find((a) => baseOf(a.key) === base);
}

export function sessionKey(workspaceId?: string, id = "main"): string {
  const base = workspaceId ?? "main";
  return id === "main" ? base : `${base}:${id}`;
}

/** The session id inside a key, i.e. the inverse of `sessionKey`. */
export function sessionIdOf(base: string, key: string): string {
  return key === base ? "main" : key.slice(base.length + 1);
}

export type SessionMap = Record<string, Session>;

export function getSession(map: SessionMap, key: string): Session {
  return map[key] ?? emptySession;
}

export function patchSession(
  map: SessionMap,
  key: string,
  patch: Partial<Session> | ((s: Session) => Partial<Session>),
): SessionMap {
  const current = getSession(map, key);
  const applied = typeof patch === "function" ? patch(current) : patch;
  return { ...map, [key]: { ...current, ...applied } };
}
