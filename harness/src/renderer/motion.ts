/**
 * The constants every animation in this app runs at, taken from opencode's own source (contract
 * §8). They live apart from the components so they can be read — and tested — without a DOM.
 */

/** `packages/tui/src/component/spinner.tsx:10`, ticked at its own 80 ms (`:19`). */
export const SPINNER_FRAMES = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
export const SPINNER_MS = 80;
/** `⋯` is what opencode shows with animations off (`spinner.tsx:17`). */
export const SPINNER_STILL = "⋯";
/** The working indicator's grid (`session-progress-indicator-v2.tsx:4-12`). */
export const DOT_GRID = 5;
export const DOT_SIZE = 2;
export const DOT_GAP = 1;
/** How long a count takes to settle after it changes (`tool-status-title.tsx:31`). */
export const TICK_MS = 600;

/**
 * The status spinner under the prompt is not the braille one: it is a Knight-Rider scanner in
 * BLOCKS, ticked at 40 ms (`packages/tui/src/component/prompt/index.tsx:1525,1329-1343`,
 * `packages/tui/src/ui/spinner.ts:246-330`). Active cells are `■`, inactive `⬝`, the head runs out
 * and back, and it holds 30 frames at the start and 9 at the end.
 */
export const SCAN_MS = 40;
export const SCAN_WIDTH = 8;
export const SCAN_TRAIL = 6;
export const SCAN_HOLD_START = 30;
export const SCAN_HOLD_END = 9;
export const SCAN_MIN_ALPHA = 0.3;
export const SCAN_INACTIVE = 0.6;

/** Where the scanner's head is on a given frame, or `undefined` while it holds. */
export function scanHead(frame: number, width = SCAN_WIDTH, holdStart = SCAN_HOLD_START, holdEnd = SCAN_HOLD_END): number | undefined {
  const total = width + holdEnd + (width - 1) + holdStart;
  const at = ((frame % total) + total) % total;
  if (at < width) return at;                       // out
  if (at < width + holdEnd) return width - 1;      // hold at the far end
  // Back: from the cell before the end down to the first, one per frame.
  if (at < width + holdEnd + (width - 1)) return width - 2 - (at - width - holdEnd);
  return undefined;                                 // hold at the start
}

/**
 * One frame, as cells: the glyph and how bright it is. The head is full strength and the trail
 * fades over `SCAN_TRAIL` steps down to `SCAN_MIN_ALPHA`; everything else is the inactive dot.
 */
export function scanFrame(frame: number, width = SCAN_WIDTH): { glyph: string; alpha: number }[] {
  const head = scanHead(frame, width);
  return Array.from({ length: width }, (_, i) => {
    if (head === undefined) return { glyph: "⬝", alpha: SCAN_MIN_ALPHA };
    const behind = Math.abs(i - head);
    if (behind >= SCAN_TRAIL) return { glyph: "⬝", alpha: SCAN_MIN_ALPHA * SCAN_INACTIVE };
    return { glyph: "■", alpha: Math.max(SCAN_MIN_ALPHA, 1 - behind / SCAN_TRAIL) };
  });
}
