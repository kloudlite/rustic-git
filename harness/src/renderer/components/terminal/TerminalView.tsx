import { createEffect, onCleanup, onMount } from "solid-js";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import "@xterm/xterm/css/xterm.css";
import { mode } from "../../theme";
import { connected } from "../../live";
import type { TermTab } from "./tabs";
import { banner as dropBanner, step, type Reconnect } from "./reconnect";

/**
 * One terminal. The emulator is xterm.js — the one VS Code and every other
 * Electron terminal uses — so ANSI, selection, links and reflow are its problem.
 *
 * The shell itself is a tmux session on the bench (or, for a workspace scope,
 * spliced through to its tool server): main owns the socket, this view only
 * names the tab and its session. A dropped socket is not a dead shell — tmux
 * still holds it — so the view reattaches by itself (`reconnect.ts`) and only
 * a real exit, or the tab's x, ends anything.
 * A tab stays mounted while another is shown, so its scrollback survives.
 */
export function TerminalView(props: { tab: TermTab; visible: boolean; onExited?: (id: string) => void; onEnded?: () => void }) {
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
    // Undefined while the shell is attached; a Reconnect while it is not.
    let drop: Reconnect | undefined;

    // A shell that ended took its tmux session with it, and a tab is a session:
    // the tab goes too, without a kill and without waiting for Enter (owner,
    // 2026-09-17: the tabs are a one-to-one map of the sessions).
    const end = () => {
      if (exited) return;
      exited = true;
      props.onExited?.(props.tab.id);
      props.onEnded?.();
    };

    const dial = () =>
      void window.harness.pty
        .open(props.tab.id, props.tab.scope, term.cols, term.rows, props.tab.session)
        .catch((e: Error) => feed({ type: "drop", now: Date.now() }, e.message));

    /** One door into the state machine, so the banner and the dial stay in step with it. */
    const feed = (ev: Parameters<typeof step>[1], why?: string) => {
      const was = drop;
      const r = step(drop, ev);
      drop = r.state;
      if (drop && drop !== was && (!was || was.gaveUp !== drop.gaveUp)) term.write(`\r\n\x1b[2m${dropBanner(drop)}${why ? ` (${why})` : ""}\x1b[0m\r\n`);
      if (r.open) dial();
    };

    dial();
    term.onData((d) => window.harness.pty.write(props.tab.id, enc.encode(d)));
    term.onResize(({ cols, rows }) => window.harness.pty.resize(props.tab.id, cols, rows));

    // One beat for the whole machine: cheap, and a second's granularity is well
    // under the shortest backoff.
    const beat = setInterval(() => drop && feed({ type: "tick", now: Date.now(), connected: connected() }), 1_000);

    // Straight through: xterm.js already batches writes into its own render
    // frame. Coalescing them ourselves split zsh's erase-and-redraw across two
    // frames, which the owner saw as flicker on backspace.
    const offData = window.harness.pty.onData((id, data) => {
      if (id !== props.tab.id) return;
      // Bytes are the only proof the reattach worked; tmux redraws the pane on attach.
      if (drop) feed({ type: "data" });
      term.write(data);
    });
    const offExit = window.harness.pty.onExit((id, code, error) => {
      if (id !== props.tab.id) return;
      // A shell that exited is gone for good; a socket that dropped is not —
      // tmux still holds the session, so that one is reattached, not mourned.
      if (typeof code === "number") return end();
      feed({ type: "drop", now: Date.now() }, error);
    });
    // Enter on a dead shell closes the tab; on one that gave up reconnecting it
    // tries again — the same key that would have run the next command.
    term.onBell(() => {}); // a PTY bell is not this app's notification channel
    term.onKey(({ domEvent }) => {
      if (domEvent.key !== "Enter") return;
      if (exited) props.onEnded?.();
      else if (drop?.gaveUp) feed({ type: "retry", now: Date.now() });
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
      clearInterval(beat);
      clearTimeout(refit);
      ro.disconnect();
      offData();
      offExit();
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
