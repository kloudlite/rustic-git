/**
 * A prompt's inline parts (`message-file.ts`, and the `<span data-highlight>` spans
 * `user-message.tsx` renders from `source.text.start/end`): a person writes `@bins/agent/src/lib.rs`
 * or `@svelte` and means a FILE or an AGENT, not those characters. The pane highlights them, so a
 * prompt reads as what it addressed.
 *
 * We have no offsets from pi, so the mention is found in the text itself — one pass, pure, tested.
 */
export type Segment = { type: "text" | "file" | "agent"; text: string };

/** `@` then a run of path characters; a mention that looks like a path is a file, else an agent. */
const MENTION = /@([A-Za-z0-9._\-/]*[A-Za-z0-9_\-/])/g;

export function mentions(text: string): Segment[] {
  const out: Segment[] = [];
  let at = 0;
  for (const m of text.matchAll(MENTION)) {
    const i = m.index ?? 0;
    // `a@b` is an address, not a mention: only a mention at a word boundary counts.
    if (i > 0 && /[A-Za-z0-9_]/.test(text[i - 1] ?? "")) continue;
    if (i > at) out.push({ type: "text", text: text.slice(at, i) });
    out.push({ type: /[/.]/.test(m[1]) ? "file" : "agent", text: m[0] });
    at = i + m[0].length;
  }
  if (at < text.length) out.push({ type: "text", text: text.slice(at) });
  return out;
}

/** What an attachment is called in its chip (`typeLabel`, `message-file.ts`). */
export function typeLabel(mime: string | undefined): string {
  if (!mime) return "File";
  if (mime.startsWith("image/")) return "Image";
  if (mime === "application/pdf") return "PDF";
  if (mime.startsWith("text/")) return "Text";
  return "File";
}
