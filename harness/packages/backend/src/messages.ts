//! The log of messages sessions send each other (`workspace_ask`, its reply, `main_tell`), kept so the
//! Plan panel can show who said what to whom. A view, never the record: the message itself already
//! went into the receiver's session. The newest 100 survive. One JSON file, written tmp + rename like
//! asks.ts so a crash never leaves half a file.
import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import type { Message } from "./index.ts";

export const messagesFile = () => join(homedir(), ".kl", "messages.json");
const KEEP = 100;

export function readMessages(file: string): Message[] {
  try {
    const v = JSON.parse(readFileSync(file, "utf8"));
    return Array.isArray(v) ? v : [];
  } catch {
    return [];
  }
}

/** Append one message (text cut to 200 chars) and return it. */
export function recordMessage(file: string, m: Omit<Message, "id" | "at">): Message {
  const full: Message = { ...m, text: m.text.slice(0, 200), id: crypto.randomUUID().slice(0, 8), at: new Date().toISOString() };
  mkdirSync(dirname(file), { recursive: true });
  writeFileSync(`${file}.tmp`, JSON.stringify([...readMessages(file), full].slice(-KEEP)));
  renameSync(`${file}.tmp`, file);
  return full;
}
