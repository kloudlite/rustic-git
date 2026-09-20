import { createSignal, onCleanup } from "solid-js";

/**
 * One shared `now`, not one `setInterval` per panel: ToolCall and TaskView both mount an
 * OperationPanel, and each used to run its own clock. `useOperationClock` ref-counts callers so
 * exactly one timer runs while at least one panel is visible, and none does otherwise.
 */
const TICK_MS = 500;
const [now, setNow] = createSignal(Date.now());
let refs = 0;
let timer: ReturnType<typeof setInterval> | undefined;

export function useOperationClock() {
  if (refs === 0) {
    setNow(Date.now());
    timer = setInterval(() => setNow(Date.now()), TICK_MS);
  }
  refs += 1;
  onCleanup(() => {
    refs -= 1;
    if (refs === 0 && timer !== undefined) {
      clearInterval(timer);
      timer = undefined;
    }
  });
  return now;
}
