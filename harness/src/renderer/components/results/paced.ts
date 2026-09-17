/**
 * Streaming text, paced as opencode paces it (`session-ui/src/components/message-part.tsx:252-306`,
 * transcribed). A model's answer arrives in bursts; shown raw it lurches. Shown word by word on a
 * 24 ms tick it reads like writing — and a burst bigger than 512 characters is applied at once,
 * because nobody wants to watch a page catch up one word at a time.
 *
 * Pure: `next(text, shown)` is where the whole behaviour lives, so it is a table test rather than a
 * thing you watch.
 */
export const TEXT_RENDER_PACE_MS = 24;
export const TEXT_RENDER_IMMEDIATE = 512;
const TEXT_RENDER_SNAP = /[\s.,!?;:)\]]/;

/** How much to add per tick, by how much is left (`step`, :258). */
export function step(size: number): number {
  if (size <= 12) return 2;
  if (size <= 48) return 4;
  if (size <= 96) return 8;
  return Math.min(256, Math.ceil(size / 4));
}

/**
 * Where the next tick ends: a step forward, then up to eight characters further to land on a word
 * boundary (`next`, :265) — so text grows by words, never mid-word.
 */
export function next(text: string, start: number): number {
  const end = Math.min(text.length, start + step(text.length - start));
  const max = Math.min(text.length, end + 8);
  for (let i = end; i < max; i++) {
    if (TEXT_RENDER_SNAP.test(text[i] ?? "")) return i + 1;
  }
  return end;
}

/**
 * What this tick should show, given what is shown now. `undefined` means "nothing to do".
 * Everything that is not plain forward growth — a rewrite, a shrink, a burst, a finished
 * message — is applied whole (`run`, :283).
 */
export function paced(text: string, shown: string, live: boolean): string | undefined {
  if (text === shown) return undefined;
  if (!live) return text;
  if (!text.startsWith(shown) || text.length <= shown.length) return text;
  if (text.length - shown.length <= TEXT_RENDER_IMMEDIATE) return text;
  return text.slice(0, next(text, shown.length));
}
