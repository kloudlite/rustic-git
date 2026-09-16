import { createEffect, onCleanup, onMount } from "solid-js";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { mode } from "../../theme";
import type { TermTab } from "./tabs";

/**
 * One terminal. The emulator is xterm.js — the one VS Code and every other
 * Electron terminal uses — so ANSI, selection, links and reflow are its problem.
 *
 * The shell itself is a PTY on the bench (or, for a workspace scope, spliced
 * through to its tool server): main owns the socket, this view only names the
 * tab. One socket, one shell, one life — an exit or a dropped tunnel ends the
 * tab rather than reconnecting to something that is gone.
 * A tab stays mounted while another is shown, so its scrollback survives.
 */
export function TerminalView(props: { tab: TermTab; visible: boolean; onExited?: (id: string) => void; onClose?: () => void }) {
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
      fontFamily: "Lilex, ui-monospace, Menlo, monospace",
      fontSize: 12,
      lineHeight: 1.45,
      cursorBlink: false,
      cursorStyle: "block",
      convertEol: true,
      theme: palette(),
      scrollback: 5000,
    });
    fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host);
    fit.fit();

    term.writeln(props.tab.banner);
    const enc = new TextEncoder();
    let exited = false;

    const end = (code: number | undefined, error?: string) => {
      if (exited) return;
      exited = true;
      term.write(code === undefined ? `\r\n\x1b[2m[${error ?? "disconnected"} — reopen the shell]\x1b[0m` : `\r\n\x1b[2m[process exited with code ${code}]\x1b[0m`);
      props.onExited?.(props.tab.id);
    };

    void window.harness.pty
      .open(props.tab.id, props.tab.scope, term.cols, term.rows)
      .catch((e: Error) => end(undefined, e.message));
    term.onData((d) => window.harness.pty.write(props.tab.id, enc.encode(d)));
    term.onResize(({ cols, rows }) => window.harness.pty.resize(props.tab.id, cols, rows));

    const offData = window.harness.pty.onData((id, data) => id === props.tab.id && term.write(data));
    const offExit = window.harness.pty.onExit((id, code, error) => id === props.tab.id && end(code, error));
    // Enter on a dead shell closes the tab: the same key that would have run
    // the next command, since there is nothing left to run it.
    term.onKey(({ domEvent }) => exited && domEvent.key === "Enter" && props.onClose?.());

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
      window.harness.pty.close(props.tab.id);
      term.dispose();
    });
  });

  // A hidden terminal cannot measure itself, so refit and refocus on show.
  createEffect(() => {
    if (props.visible && term) {
      queueMicrotask(() => {
        fit.fit();
        term.focus();
      });
    }
  });

  // Re-theme in place rather than rebuilding the terminal.
  createEffect(() => {
    mode();
    if (term) queueMicrotask(() => (term.options.theme = palette()));
  });

  return <div ref={host} class="h-full min-h-0 px-2 py-1" classList={{ hidden: !props.visible }} />;
}
