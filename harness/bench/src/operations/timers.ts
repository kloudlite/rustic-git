/**
 * `setTimeout` clamps a delay above 2^31-1 ms (24.8 days) to a 1 ms timer instead of
 * refusing it (measured: Node fires it almost immediately), so a deadline a month out
 * would cancel a fresh operation. `setLongTimeout` re-arms in MAX_TIMER_MS chunks off an
 * absolute target computed once, so drift across re-arms never compounds.
 */
export const MAX_TIMER_MS = 2_147_483_647;

export function setLongTimeout(fn: () => void, delayMs: number, maxMs = MAX_TIMER_MS): { clear(): void } {
  const target = Date.now() + delayMs;
  let cleared = false;
  let handle: ReturnType<typeof setTimeout>;
  const arm = (ms: number) => {
    handle = setTimeout(() => {
      if (cleared) return;
      const remaining = target - Date.now();
      if (remaining > 0) {
        arm(Math.min(remaining, maxMs));
      } else {
        fn();
      }
    }, ms);
  };
  arm(Math.min(Math.max(0, delayMs), maxMs));
  return {
    clear() {
      cleared = true;
      clearTimeout(handle);
    },
  };
}
