import { For, createEffect, createSignal, on, onCleanup } from "solid-js";
import { DOT_GAP, DOT_GRID, DOT_SIZE, SCAN_MS, SPINNER_FRAMES, SPINNER_MS, SPINNER_STILL, TICK_MS, scanFrame } from "../motion";

/**
 * The animations opencode runs, at opencode's own constants. They live together because they are
 * one decision — how this app moves — and because every one of them has to answer
 * `prefers-reduced-motion` the same way.
 */


/**
 * The braille spinner. Reduced motion gets `⋯`, which is what opencode shows with animations off
 * (`spinner.tsx:17`) — a still mark that still says "running", rather than a frozen frame.
 */
export function Spinner(props: { class?: string }) {
  const still = typeof matchMedia === "function" && matchMedia("(prefers-reduced-motion: reduce)").matches;
  const [i, setI] = createSignal(0);
  if (!still) {
    const t = setInterval(() => setI((n) => (n + 1) % SPINNER_FRAMES.length), SPINNER_MS);
    onCleanup(() => clearInterval(t));
  }
  return <span class={props.class} aria-hidden="true">{still ? SPINNER_STILL : SPINNER_FRAMES[i()]}</span>;
}

/**
 * The status line's own spinner: the TUI's block scanner, 40 ms a frame
 * (`prompt/index.tsx:1525`). With animations off the TUI shows `[⋯]` (`:1524`), so that is the
 * reduced-motion face here too.
 */
export function Scanner(props: { class?: string }) {
  const still = typeof matchMedia === "function" && matchMedia("(prefers-reduced-motion: reduce)").matches;
  const [frame, setFrame] = createSignal(0);
  if (!still) {
    const t = setInterval(() => setFrame((n) => n + 1), SCAN_MS);
    onCleanup(() => clearInterval(t));
  }
  return (
    <span class={props.class} aria-hidden="true">
      {still ? "[⋯]" : <For each={scanFrame(frame())}>{(c) => <span style={{ opacity: String(c.alpha) }}>{c.glyph}</span>}</For>}
    </span>
  );
}

/**
 * The working indicator: a 5×5 grid of 2 px dots with a 1 px gap, each dot's own keyframe staggered
 * in 12.5 % steps over 1200 ms (`session-progress-indicator-v2.tsx:4-12`, `.css:1-14`). Reduced
 * motion stops every dot and pins the middle one lit, exactly as its own stylesheet does.
 */
const GRID = DOT_GRID;
const DOT = DOT_SIZE;
const GAP = DOT_GAP;
const DOTS = Array.from({ length: GRID * GRID }, (_, index) => ({
  index,
  x: 1.5 + (index % GRID) * (DOT + GAP),
  y: 1.5 + Math.floor(index / GRID) * (DOT + GAP),
}));

export function WorkingDots(props: { class?: string }) {
  return (
    <svg data-component="session-progress-indicator-v2" width="16" height="16" viewBox="0 0 16 16" class={props.class} aria-hidden="true">
      <For each={DOTS}>{(d) => <rect data-dot={d.index} x={d.x} y={d.y} width={DOT} height={DOT} rx="0.5" />}</For>
    </svg>
  );
}

/**
 * A count that changes while you watch it (`tool-status-title.tsx:31`): the new value arrives, the
 * row settles after 600 ms, and nothing moves under reduced motion.
 */
export function Ticker(props: { value: string; class?: string }) {
  const [moving, setMoving] = createSignal(false);
  let timer: ReturnType<typeof setTimeout> | undefined;
  createEffect(
    on(
      () => props.value,
      () => {
        setMoving(true);
        clearTimeout(timer);
        timer = setTimeout(() => setMoving(false), TICK_MS);
      },
      { defer: true },
    ),
  );
  onCleanup(() => clearTimeout(timer));
  return <span class={props.class} classList={{ tick: moving() }}>{props.value}</span>;
}
