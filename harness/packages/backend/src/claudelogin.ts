//! `claude auth login` run by the daemon under a pty (`script -qfec`), so the bench needs no ssh:
//! the sign-in URL goes to the TUI as an `auth_url` event, the pasted code comes back as the
//! prompt answer, the same path pi's OAuth logins take. Success is the promise resolving.
//!
//! Probe (macOS claude, isolated config; the Linux flags differ only in `script`): no method menu.
//! "Opening browser to sign in…", then the URL wrapped in an OSC 8 hyperlink and blue SGR (so it
//! appears twice, escapes included), then "Paste code here if prompted > " with no newline. The
//! success line was not seen (a login would have been real); "success" is the guess.
import type { AuthEvent } from "@kloudlite-tui/agent";

// OSC (hyperlinks, titles) and CSI (colours) sequences: the URL regex must see plain text
const ESCAPES = /\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-9;?]*[A-Za-z]/g;

export async function claudeLogin(
  onEvent: (e: AuthEvent) => void,
  ask: (message: string) => Promise<string>,
  spawnFn: typeof Bun.spawn = Bun.spawn,
): Promise<void> {
  const p: any = spawnFn(["script", "-qfec", "claude auth login", "/dev/null"], { stdin: "pipe", stdout: "pipe", stderr: "ignore" });
  const dec = new TextDecoder();
  let buf = "", sentUrl = false, asked = false;
  try {
    for await (const chunk of p.stdout as any) {
      buf += dec.decode(chunk, { stream: true });
      const text = buf.replace(ESCAPES, "");
      // the lookahead waits for the URL's end: a chunk can split it
      const url = text.match(/https:\/\/\S+(?=\s)/)?.[0];
      if (url && !sentUrl) { sentUrl = true; onEvent({ type: "auth_url", url }); }
      if (!asked && /paste code here/i.test(text)) {
        asked = true;
        const code = await ask("Paste the code from the browser");
        p.stdin.write(`${code.trim()}\n`);
      }
    }
    const code = await p.exited;
    const text = buf.replace(ESCAPES, "");
    if (code !== 0 || !/success/i.test(text)) throw new Error(`claude login failed: ${text.trim().split(/\r?\n/).at(-1) ?? code}`);
  } catch (e) {
    p.kill?.(); // cancelled prompt or read error: never leave the login waiting on a pty
    throw e;
  }
}
