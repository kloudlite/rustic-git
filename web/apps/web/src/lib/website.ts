/** A team website worth turning into a link, or `undefined`.
 *
 *  The value lands as an `href` on a PUBLIC page. React refuses `javascript:` on its own,
 *  but `data:`, `vbscript:` and `file:` walk straight through, so the scheme is decided here —
 *  once, for the save and for the render — and anything else is shown as plain text. */
export function safeWebsite(website?: string): string | undefined {
  if (!website) return undefined;
  try {
    const u = new URL(website);
    return u.protocol === "http:" || u.protocol === "https:" ? website : undefined;
  } catch {
    return undefined;
  }
}
