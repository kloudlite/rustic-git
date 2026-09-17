import { createEffect, createSignal, onCleanup } from "solid-js";

/**
 * opencode's prompt cursor, on the web. Their prompt is a terminal input: it sets
 * `input.cursorColor = theme.text` when the prompt is live and `theme.backgroundElement` when it is
 * disabled (`packages/tui/src/component/prompt/index.tsx:252-253`), and the style is the terminal's
 * block. Nothing in their code blinks it — a terminal's own cursor blink is the terminal's setting,
 * not theirs — so this is STEADY.
 *
 * Chromium's `caret-shape: block` is not that cursor, so the block is drawn: a mirror of the
 * textarea holds the same text with the same metrics, and the character at the caret is painted in
 * reverse video, which is exactly what a block cursor is.
 */
export function BoxCursor(props: { input?: HTMLTextAreaElement; text: string; disabled?: boolean }) {
  const [at, setAt] = createSignal(0);
  const [focused, setFocused] = createSignal(false);

  createEffect(() => {
    const el = props.input;
    if (!el) return;
    const sync = () => setAt(el.selectionStart ?? el.value.length);
    const on = () => (setFocused(true), sync());
    const off = () => setFocused(false);
    for (const ev of ["input", "keyup", "click", "select", "scroll"] as const) el.addEventListener(ev, sync);
    el.addEventListener("focus", on);
    el.addEventListener("blur", off);
    document.addEventListener("selectionchange", sync);
    setFocused(document.activeElement === el);
    sync();
    onCleanup(() => {
      for (const ev of ["input", "keyup", "click", "select", "scroll"] as const) el.removeEventListener(ev, sync);
      el.removeEventListener("focus", on);
      el.removeEventListener("blur", off);
      document.removeEventListener("selectionchange", sync);
    });
  });

  // The character under the block; a caret past the end sits on a space, as a terminal's does.
  const under = () => props.text[at()] ?? " ";
  const before = () => props.text.slice(0, at());

  return (
    <div
      data-component="box-cursor"
      aria-hidden="true"
      class="pointer-events-none absolute inset-0 whitespace-pre-wrap break-words text-transparent select-none"
      classList={{ "opacity-0": !focused() }}
    >
      {before()}
      <span
        data-slot="box-cursor-cell"
        /* Their live colour is the TEXT token, not the accent; disabled is the panel tone. */
        class={props.disabled ? "bg-line text-bg" : "bg-fg text-bg"}
      >
        {under() === "\n" ? " " : under()}
      </span>
    </div>
  );
}
