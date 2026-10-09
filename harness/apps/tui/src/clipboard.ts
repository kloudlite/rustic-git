import { execFileSync } from "node:child_process";
import { readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

export type ClipImage = { type: "image"; data: string; mimeType: string };

const png = (bytes: Buffer): ClipImage | null =>
  bytes.length ? { type: "image", data: bytes.toString("base64"), mimeType: "image/png" } : null;

/** Linux (and the bench browser terminal's xclip stand-in). */
function fromXclip(): ClipImage | null {
  const xclip = (target: string) =>
    execFileSync("xclip", ["-selection", "clipboard", "-t", target, "-o"], {
      stdio: ["ignore", "pipe", "ignore"],
      maxBuffer: 32 << 20,
      env: process.env, // Bun snapshots env at boot; pass the live one
    });
  if (!xclip("TARGETS").toString().split("\n").includes("image/png")) return null;
  return png(xclip("image/png"));
}

/**
 * macOS. `pbpaste` cannot emit PNG bytes, so AppleScript coerces the
 * clipboard to PNG and writes it to a temp file we read back and delete.
 */
function fromPasteboard(): ClipImage | null {
  const path = join(tmpdir(), `kloudlite-clip-${process.pid}.png`);
  try {
    execFileSync(
      "osascript",
      [
        "-e",
        `set f to open for access POSIX file "${path}" with write permission
set eof f to 0
write (the clipboard as «class PNGf») to f
close access f`,
      ],
      { stdio: ["ignore", "ignore", "ignore"], env: process.env },
    );
    return png(readFileSync(path));
  } finally {
    rmSync(path, { force: true });
  }
}

/**
 * The clipboard's PNG image, or null when it holds no image (or the platform's
 * clipboard tool is missing). Never throws — a failed paste is a no-op.
 */
export function readClipboardImage(): ClipImage | null {
  for (const read of process.platform === "darwin" ? [fromPasteboard, fromXclip] : [fromXclip, fromPasteboard]) {
    try {
      const img = read();
      if (img) return img;
    } catch {
      // wrong platform, missing tool, or no image on the clipboard — try the next
    }
  }
  return null;
}

/**
 * Put text on the clipboard. Order matters: kl-tui runs on the laptop, where
 * Terminal.app has no OSC 52, so pbcopy goes first; the fallback TUI runs on
 * the bench (no pbcopy) inside the bench-proxy or ttyd terminal, where OSC 52 travels back to the
 * user's terminal; xclip is the last resort.
 */
export function copyText(
  text: string,
  osc52: (t: string) => boolean,
  run: typeof execFileSync = execFileSync,
): boolean {
  const opts = { input: text, stdio: ["pipe", "ignore", "ignore"] as ["pipe", "ignore", "ignore"], env: process.env };
  if (process.platform === "darwin") {
    try {
      run("pbcopy", [], opts);
      return true;
    } catch {}
  }
  try {
    if (osc52(text)) return true;
  } catch {}
  try {
    run("xclip", ["-selection", "clipboard", "-i"], opts);
    return true;
  } catch {
    return false;
  }
}

let copier = copyText;
/** Tests only: keep drag-select tests off the real clipboard. */
export function setCopier(fn: typeof copyText | null) {
  copier = fn ?? copyText;
}
export const copySelection: typeof copyText = (...a) => copier(...a);
