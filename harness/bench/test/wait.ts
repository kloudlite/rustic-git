// Tests poll for the condition they need instead of sleeping a guessed time:
// a fixed sleep that a loaded suite outruns fails an assertion before stop() and leaks a child.

/** Resolves once fn() is truthy (sync or async), polling every 20 ms; rejects after timeoutMs naming what it waited for. */
export async function until(fn: () => unknown, timeoutMs = 5_000, what = String(fn)): Promise<void> {
  const end = Date.now() + timeoutMs;
  for (;;) {
    if (await fn()) return;
    if (Date.now() > end) throw new Error(`timed out after ${timeoutMs} ms waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 20));
  }
}
