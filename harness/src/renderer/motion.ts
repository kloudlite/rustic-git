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
