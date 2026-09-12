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
 * `write` and `onData` below are the whole seam to the session: when the
 * transport lands it replaces the local echo and nothing else here changes.
 * A tab stays mounted while another is shown, so its scrollback survives.
 */
export function TerminalView(props: { tab: TermTab; visible: boolean }) {
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

    const prompt = () => term.write("\r\n\x1b[34m$\x1b[0m ");
    term.writeln(props.tab.banner);
    prompt();

    let line = "";
    term.onData((d) => {
      if (d === "\r") {
        term.write("\r\n");
        if (line.trim()) term.writeln("\x1b[2m" + line.trim() + ": no session transport yet\x1b[0m");
        line = "";
        prompt();
      } else if (d === "\u007f") {
        if (line) {
          line = line.slice(0, -1);
          term.write("\b \b");
        }
      } else if (d >= " ") {
        line += d;
        term.write(d);
      }
    });

    const ro = new ResizeObserver(() => props.visible && fit.fit());
    ro.observe(host);
    onCleanup(() => {
      ro.disconnect();
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
