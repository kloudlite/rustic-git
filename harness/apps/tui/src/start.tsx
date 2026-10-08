import { writeSync } from "node:fs";
import { createCliRenderer } from "@opentui/core";
import { createRoot } from "@opentui/react";

/** Terminal setup shared by the pod's TUI and the laptop's kl-tui. Call after boot(). */
export async function start(onExit?: () => void) {
  // xterm modifyOtherKeys: terminals without kitty-protocol support enabled
  // (WezTerm default config) otherwise send ctrl+h as a bare backspace byte,
  // indistinguishable from Backspace. Must be written BEFORE createCliRenderer:
  // opentui patches process.stdout and buffers writes, so sequences written
  // afterwards never reach the terminal. opentui's parser understands the
  // CSI 27;…~ encodings this turns on.
  process.stdout.write("\x1b[>4;2m");
  process.on("exit", () => process.stdout.write("\x1b[>4;0m"));

  // opentui owns the terminal: alternate screen, kitty keyboard protocol,
  // SGR mouse, and a cell-diff compositor (no stale-cell artifacts).
  const renderer = await createCliRenderer({
    exitOnCtrlC: false, // the app handles ctrl+c (and /exit) itself
    useMouse: true, // wheel scrolling in the transcript
  });

  // Imported here, not at the top: app.tsx's models/theme read hello() at import.
  const { App } = await import("./app.tsx");
  createRoot(renderer).render(<App onExit={onExit} />);

  // Re-assert modifyOtherKeys mode 2: opentui's native setup writes CSI >4;1m
  // (mode 1), which downgrades the mode-2 enable above — and mode 1 keeps
  // ctrl+h/ctrl+j as legacy control bytes. The patched process.stdout would
  // swallow this, so write straight to the fd once the initial frames settle.
  setTimeout(() => writeSync(1, "\x1b[>4;2m"), 150);
}
