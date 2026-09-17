import { createEffect, onCleanup, onMount } from "solid-js";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import "@xterm/xterm/css/xterm.css";
import { mode } from "../../theme";
import type { TermTab } from "./tabs";

/**
 * One terminal. The emulator is xterm.js — the one VS Code and every other
 * Electron terminal uses — so ANSI, selection, links and reflow are its problem.
 *
 * The shell is the pod's `shell` sidecar, running ttyd with the person's home and nothing else
 * (spec §2.3): main owns the socket and this view only names the tab. The socket IS the shell —
 * there is no tmux behind it, so a dropped connection is a finished shell and a new tab is a new
 * one. A tab stays mounted while another is shown, so its scrollback survives.
 */
export function TerminalView(props: { tab: TermTab; visible: boolean; onExited?: (id: string) => void; onEnded?: () => void; onTitle?: (id: string, title: string) => void }) {
  let host!: HTMLDivElement;
  let term: Terminal;
  let fit: FitAddon;

  /** Read the live theme out of CSS, so the terminal never drifts from the app. */
  const palette = () => {
    const s = getComputedStyle(document.documentElement);
    const v = (n: string) => s.getPropertyValue(n).trim();
    return {
      background: v("--bg"),
      foreground: v("--fg"),
      cursor: v("--accent"),
      cursorAccent: v("--bg"),
      selectionBackground: v("--selected"),
      black: v("--bg"),
      red: v("--danger"),
      green: v("--success"),
      yellow: v("--warning"),
      blue: v("--accent"),
      magenta: v("--accent"),
      cyan: v("--accent"),
      white: v("--fg"),
      brightBlack: v("--subtle"),
      brightRed: v("--danger"),
      brightGreen: v("--success"),
      brightYellow: v("--warning"),
      brightBlue: v("--accent"),
      brightMagenta: v("--accent"),
      brightCyan: v("--accent"),
      brightWhite: v("--fg"),
    };
  };

  onMount(() => {
    term = new Terminal({
      // The same face as the pane: a terminal beside it in a different mono reads as a second product.
      fontFamily: "'IBM Plex Mono', Menlo, 'SF Mono', Monaco, ui-monospace, monospace",
      fontSize: 13,
      lineHeight: 1.2,
      letterSpacing: 0,
      cursorBlink: true,
      cursorStyle: "bar",
      convertEol: true,
      macOptionIsMeta: true,
      theme: palette(),
      scrollback: 10000,
      smoothScrollDuration: 0,
      // No `fastScrollModifier`: xterm 6 dropped the option (alt-scroll is built in).
    });
    fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host);

    // The DOM renderer repaints a row per keystroke, which the owner sees as
    // flicker on backspace. WebGL draws the whole grid; if the context is lost
    // (GPU reset, a headless run) disposing the addon puts the DOM renderer back.
    let webgl: WebglAddon | undefined;
    try {
      webgl = new WebglAddon();
      webgl.onContextLoss(() => {
        webgl?.dispose();
        webgl = undefined;
      });
      term.loadAddon(webgl);
    } catch (e) {
      webgl = undefined;
      console.warn("terminal: WebGL unavailable, using the DOM renderer", e);
    }

    fit.fit();

    term.writeln(props.tab.banner);
    const enc = new TextEncoder();
    let exited = false;

    /**
     * The shell is over. There is nothing to reattach to — the socket was the shell — so the view
     * says so once, stops taking input, and leaves the scrollback to be read (spec §2.3).
     */
    const end = (why?: string) => {
      if (exited) return;
      exited = true;
      term.write(`\r\n\x1b[2m[shell ended — open a new one]${why ? ` (${why})` : ""}\x1b[0m\r\n`);
      term.options.disableStdin = true;
      props.onExited?.(props.tab.id);
    };

    void window.harness.pty.open(props.tab.id, props.tab.scope, term.cols, term.rows).catch((e: Error) => end(e.message));
    term.onData((d) => window.harness.pty.write(props.tab.id, enc.encode(d)));
    term.onResize(({ cols, rows }) => window.harness.pty.resize(props.tab.id, cols, rows));

    // Straight through: xterm.js already batches writes into its own render
    // frame. Coalescing them ourselves split zsh's erase-and-redraw across two
    // frames, which the owner saw as flicker on backspace.
    const offData = window.harness.pty.onData((id, data) => {
      if (id !== props.tab.id) return;
      term.write(data);
    });
    const offExit = window.harness.pty.onExit((id, code, error) => {
      if (id !== props.tab.id) return;
      // Exit or drop, it is the same thing now: the socket was the shell.
      end(typeof code === "number" ? undefined : error);
    });
    // What the shell calls itself (ttyd's `1` frame): the tab shows it.
    const offTitle = window.harness.pty.onTitle((id, title) => {
      if (id !== props.tab.id) return;
      props.onTitle?.(props.tab.id, title.trim());
    });
    term.onBell(() => {}); // a PTY bell is not this app's notification channel
    // Enter on a finished shell closes the tab: the same key that would have run the next command.
    term.onKey(({ domEvent }) => {
      if (domEvent.key === "Enter" && exited) props.onEnded?.();
    });

    // Debounced: a drag resizes continuously, and every fit is a reflow plus a
    // resize frame to the shell.
    let refit: ReturnType<typeof setTimeout> | undefined;
    const ro = new ResizeObserver(() => {
      clearTimeout(refit);
      refit = setTimeout(() => props.visible && fit.fit(), 50);
    });
    ro.observe(host);
    onCleanup(() => {
      clearTimeout(refit);
      ro.disconnect();
      offData();
      offExit();
      offTitle();
      window.harness.pty.close(props.tab.id);
      webgl?.dispose();
      term.dispose();
    });
  });

  // A hidden terminal cannot measure itself, so refit and refocus on show — on the next FRAME,
  // not the next microtask: a microtask runs before the browser has laid the shown element out, so
  // the fit measured the box it had while hidden and the first frame came back the wrong size.
  createEffect(() => {
    if (props.visible && term) {
      const raf = requestAnimationFrame(() => {
        fit.fit();
        term.focus();
      });
      onCleanup(() => cancelAnimationFrame(raf));
    }
  });

  // Re-theme in place rather than rebuilding the terminal.
  createEffect(() => {
    mode();
    if (term) queueMicrotask(() => (term.options.theme = palette()));
  });

  return <div ref={host} class="h-full min-h-0 px-2 py-1" classList={{ hidden: !props.visible }} />;
}
