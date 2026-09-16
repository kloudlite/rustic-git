/**
 * When a terminal's socket drops, reattaching is the terminal's job, not the
 * person's: tmux still holds the shell, so `pty:open` with the same session
 * name lands back in it with its scrollback redrawn.
 *
 * A pure reducer in its own file so the timing is testable against a fake
 * clock rather than a real five minutes. The view only feeds it events and
 * does what `open` says.
 */
export type Reconnect = { firstAt: number; attempt: number; retryAt: number; gaveUp: boolean };

/** 1, 2, 4, 8 s, then every 15 s — VS Code's shape, slow enough not to hammer a bench that is asleep. */
export const DELAYS = [1_000, 2_000, 4_000, 8_000];
export const STEADY = 15_000;
/** After this long the terminal stops trying on its own and waits for a keypress. */
export const GIVE_UP_MS = 5 * 60 * 1_000;

const delay = (attempt: number) => DELAYS[attempt] ?? STEADY;

export type Ev =
  /** the socket closed, or an open failed */
  | { type: "drop"; now: number }
  /** the clock moved; `connected` is the bench-client's own state */
  | { type: "tick"; now: number; connected: boolean }
  /** bytes arrived: whatever we did worked */
  | { type: "data" }
  /** Enter on a given-up terminal */
  | { type: "retry"; now: number };

/** `open` means: dial `pty:open` with this tab's session name now. */
export function step(s: Reconnect | undefined, ev: Ev): { state: Reconnect | undefined; open: boolean } {
  switch (ev.type) {
    case "data":
      return { state: undefined, open: false };
    case "drop": {
      if (s?.gaveUp) return { state: s, open: false };
      const firstAt = s?.firstAt ?? ev.now;
      const attempt = s ? s.attempt + 1 : 0;
      // The five minutes run from the FIRST drop, so a flapping tunnel gives up
      // at the same moment a dead one does.
      if (ev.now - firstAt >= GIVE_UP_MS) return { state: { firstAt, attempt, retryAt: ev.now, gaveUp: true }, open: false };
      return { state: { firstAt, attempt, retryAt: ev.now + delay(attempt), gaveUp: false }, open: false };
    }
    case "tick": {
      if (!s || s.gaveUp || ev.now < s.retryAt) return { state: s, open: false };
      // A bench that is asleep is waited for, not retried against: the wait is
      // pushed out without spending an attempt.
      if (!ev.connected) return { state: { ...s, retryAt: ev.now + delay(s.attempt) }, open: false };
      if (ev.now - s.firstAt >= GIVE_UP_MS) return { state: { ...s, gaveUp: true }, open: false };
      return { state: s, open: true };
    }
    case "retry":
      if (!s?.gaveUp) return { state: s, open: false };
      return { state: { firstAt: ev.now, attempt: 0, retryAt: ev.now + delay(0), gaveUp: false }, open: true };
  }
}

export function banner(s: Reconnect): string {
  return s.gaveUp ? "[disconnected — press Enter to retry]" : "[disconnected — reconnecting…]";
}
