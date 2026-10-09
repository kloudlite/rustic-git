//! Pending `workspace_ask`s on disk, so a bench restart does not lose a workspace's answer. The
//! daemon (daemon.ts) outlives every client but not its own crash or a pod roll; an ask is saved
//! before the workspace is opened, dropped just before its reply is handed to the caller, and
//! resent by delegate.ts `resumeAsks` on boot. One JSON file per ask, written tmp + rename so a
//! crash never leaves half a file.
import { mkdirSync, readdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import type { ModelRef, ThinkingLevel } from "./index.ts";

export type PendingAsk = {
  id: string;
  callerKey: string;
  /** The workspace asked. */
  key: string;
  text: string;
  /** What the caller's person typed in its current turn: lent to the workspace turn so a quote of it counts as asked (consent.ts). */
  words?: string[];
  /** Board task this ask carries; kept so a resend still marks and quotes it. */
  task?: string;
  /** Resends so far; resumeAsks gives up after two. */
  tries: number;
  /** The caller's session settings, to reopen it for the reply after a restart. */
  model?: ModelRef;
  codemode?: boolean;
  thinkingLevel?: ThinkingLevel;
  autoCompact?: boolean;
};

export const asksDir = () => join(homedir(), ".kl", "asks");

export function saveAsk(dir: string, a: PendingAsk): void {
  mkdirSync(dir, { recursive: true });
  const f = join(dir, `${a.id}.json`);
  writeFileSync(`${f}.tmp`, JSON.stringify(a));
  renameSync(`${f}.tmp`, f);
}

export function dropAsk(dir: string, id: string): void {
  rmSync(join(dir, `${id}.json`), { force: true });
}

export function listAsks(dir: string): PendingAsk[] {
  let names: string[];
  try {
    names = readdirSync(dir);
  } catch {
    return [];
  }
  const out: PendingAsk[] = [];
  for (const n of names) {
    if (!n.endsWith(".json")) continue;
    try {
      out.push(JSON.parse(readFileSync(join(dir, n), "utf8")));
    } catch {}
  }
  return out;
}
