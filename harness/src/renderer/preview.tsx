import { render } from "solid-js/web";
import { Show, createSignal, onMount } from "solid-js";
import "./fonts.css";
import "./styles/app.css";
import "./theme";
import { Icon } from "./ui/Icon";
import type { Preview, PreviewState } from "../preview-preload";

declare global {
  interface Window { preview: Preview }
}

/**
 * The title bar of a preview window, drawn by the harness rather than the page:
 * the address as the environment sees it, back / forward / reload, and the
 * annotate tools — pick a block, how many are marked, clear. Picking copies a
 * reference to the clipboard; the toolbar says so and nothing else does.
 */
function Bar() {
  const [s, setS] = createSignal<PreviewState>({ title: "", picking: false, marks: 0, canBack: false, canForward: false });
  const [flash, setFlash] = createSignal("");
  let timer: ReturnType<typeof setTimeout> | undefined;
  onMount(() => {
    window.preview.onState((next) => {
      setS(next);
      if (next.copied) {
        setFlash(next.copied);
        clearTimeout(timer);
        timer = setTimeout(() => setFlash(""), 1600);
      }
    });
  });
  const btn = "inline-flex h-6 items-center gap-1.5 rounded-md px-2 text-sm text-muted hover:bg-hover hover:text-fg disabled:opacity-40 disabled:hover:bg-transparent aria-pressed:bg-accent aria-pressed:text-on-accent";
  return (
    <header class="flex h-full items-center gap-1 border-b border-line bg-chrome pr-2.5 pl-[78px] [-webkit-app-region:drag] [&>*]:[-webkit-app-region:no-drag]">
      <button class={btn} disabled={!s().canBack} onClick={() => window.preview.nav("back")} title="Back"><Icon name="chevronLeft" size={14} /></button>
      <button class={btn} disabled={!s().canForward} onClick={() => window.preview.nav("forward")} title="Forward"><Icon name="chevronRight" size={14} /></button>
      <button class={btn} onClick={() => window.preview.nav("reload")} title="Reload"><Icon name="history" size={13} /></button>
      <span class="mx-1 h-3.5 w-px bg-line" />
      <span class="min-w-0 flex-1 truncate text-center font-mono text-sm text-fg" title={s().title}>
        <Show when={!flash()} fallback={<span class="text-success">copied · {flash()}</span>}>{s().title}</Show>
      </span>
      <span class="mx-1 h-3.5 w-px bg-line" />
      <button class={btn} aria-pressed={s().picking} onClick={() => window.preview.pick(!s().picking)} title="Pick a block; the reference goes to the clipboard (esc stops)">
        <Icon name="pick" size={13} /> Pick
      </button>
      <Show when={s().marks}>
        {(n) => <span class="px-1 font-mono text-xs tabular-nums text-subtle">{n()}</span>}
      </Show>
      <button class={btn} disabled={!s().marks} onClick={() => window.preview.clear()} title="Clear marks"><Icon name="x" size={13} /></button>
    </header>
  );
}

void Promise.all([document.fonts.load('13px "IBM Plex Sans"'), document.fonts.load("13px Lilex")])
  .catch(() => undefined)
  .then(() => render(() => <Bar />, document.getElementById("root")!));
