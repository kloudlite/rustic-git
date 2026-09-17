import { createEffect, createSignal, onCleanup } from "solid-js";

/**
 * The prompt's cursor, as the TUI draws it: a full cell, in the TEXT colour while the prompt is
 * live and the panel tone while it is disabled (`packages/tui/src/component/prompt/index.tsx:252-253`),
 * in the terminal's block style. Nothing in their code blinks it, so this does not blink either.
 *
 * Chromium's `caret-shape: block` is not that cursor, so it is drawn: a mirror of the textarea in
 * the same metrics, with the character at the caret in reverse video.
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
    const events = ["input", "keyup", "click", "select", "scroll"] as const;
    for (const ev of events) el.addEventListener(ev, sync);
    el.addEventListener("focus", on);
    el.addEventListener("blur", off);
    document.addEventListener("selectionchange", sync);
    setFocused(document.activeElement === el);
    sync();
    onCleanup(() => {
      for (const ev of events) el.removeEventListener(ev, sync);
      el.removeEventListener("focus", on);
      el.removeEventListener("blur", off);
      document.removeEventListener("selectionchange", sync);
    });
  });

  /** The character under the block; past the end it sits on a space, as a terminal's does. */
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
      <span data-slot="box-cursor-cell" class={props.disabled ? "bg-line text-bg" : "bg-fg text-bg"}>
        {under() === "\n" ? " " : under()}
      </span>
    </div>
  );
}
