#!/usr/bin/env bun
/**
 * SSH server: every connection gets its own live kloudlite TUI, rendered by
 * a per-session opentui renderer over the SSH channel.
 *
 *   bun run src/server.tsx           # listens on :2222
 *   ssh -p 2222 anything@host       # any user/password (LAN dev tool)
 */
import { generateKeyPairSync } from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { Server, type ServerChannel } from "ssh2";
import { createCliRenderer } from "@opentui/core";
import { createRoot } from "@opentui/react";
import { Registry, webFetch, webSearch } from "@kloudlite-tui/tools";
import { App } from "./app.tsx";

const PORT = Number(process.env.KLOUDLITE_SSH_PORT ?? 2222);

/** Persistent host key so clients don't see a new fingerprint every start. */
function hostKey(): string {
  const dir = join(homedir(), ".config", "kloudlite");
  const path = join(dir, "host_key.pem");
  try {
    return readFileSync(path, "utf8");
  } catch {
    const { privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
    const pem = privateKey.export({ type: "pkcs1", format: "pem" }) as string;
    mkdirSync(dir, { recursive: true });
    writeFileSync(path, pem, { mode: 0o600 });
    return pem;
  }
}

/** Dress an SSH channel up as the TTY streams opentui expects. */
function asTty(channel: ServerChannel, cols: number, rows: number) {
  const stream = channel as unknown as NodeJS.ReadStream & NodeJS.WriteStream;
  Object.assign(stream, {
    isTTY: true,
    columns: cols,
    rows: rows,
    setRawMode: () => stream,
    ref: () => stream,
    unref: () => stream,
  });
  return stream;
}

const server = new Server({ hostKeys: [hostKey()] }, (client) => {
  // ponytail: accepts any credentials — LAN dev tool; add publickey auth
  // against authorized_keys before exposing beyond localhost
  client.on("authentication", (ctx) => ctx.accept());

  client.on("session", (accept) => {
    const session = accept();
    let cols = 80;
    let rows = 24;

    session.on("pty", (accept, _reject, info) => {
      cols = info.cols || 80;
      rows = info.rows || 24;
      accept?.();
    });

    session.on("shell", async (accept) => {
      const channel = accept();
      const tty = asTty(channel, cols, rows);

      // enhanced key encodings (kitty is pushed by opentui itself; this
      // covers terminals that only speak xterm modifyOtherKeys)
      channel.write("\x1b[>4;2m");

      const renderer = await createCliRenderer({
        stdin: tty,
        stdout: tty,
        width: cols,
        height: rows,
        exitOnCtrlC: false,
        useMouse: true,
      });

      const close = () => {
        try {
          renderer.destroy();
        } catch {}
        try {
          channel.end();
        } catch {}
        client.end();
      };

      session.on("window-change", (accept, _reject, info) => {
        tty.columns = info.cols;
        tty.rows = info.rows;
        renderer.resize(info.cols, info.rows);
        accept?.();
      });

      channel.on("close", () => {
        try {
          renderer.destroy();
        } catch {}
      });

      // opentui's native setup downgrades modifyOtherKeys to mode 1 (which
      // keeps ctrl+h/ctrl+j as legacy bytes) — re-assert mode 2 once the
      // initial frames have gone out, same as cli.tsx does for the local TTY
      setTimeout(() => {
        try {
          channel.write("\x1b[>4;2m");
        } catch {}
      }, 200);

      createRoot(renderer).render(<App registry={new Registry().add(webFetch, webSearch)} onExit={close} />);
    });
  });
});

server.listen(PORT, "0.0.0.0", () => {
  console.log(`kloudlite ssh server on port ${PORT} — connect: ssh -p ${PORT} kloudlite@localhost`);
});
