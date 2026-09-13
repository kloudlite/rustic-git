import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

/**
 * The folder-level fence: one harness-bench writes a bench folder. Node has no
 * flock, so flock(1) holds it for us — a child that prints "locked" once it has
 * the lock and then blocks on our stdin, so the lock dies with this process
 * (pipe closes → cat exits → fd closes) and never outlives it. On NFS 4.1 the
 * lock is a lease; a dead node's lock goes when the lease expires.
 */
export class FolderLocked extends Error {
  holder: string;
  constructor(holder: string) {
    super(`bench folder is locked by ${holder}`);
    this.holder = holder;
  }
}
export type Lock = { release(): void };

export function takeLock(dir: string, opts: { wait?: boolean; onWaiting?: (holder: string) => void } = {}): Promise<Lock> {
  const file = path.join(dir, ".lock");
  const holderFile = path.join(dir, ".lock.holder");
  fs.mkdirSync(dir, { recursive: true });
  const holder = () => {
    try { return fs.readFileSync(holderFile, "utf8").trim(); } catch { return "an unknown holder"; }
  };
  return new Promise((resolve, reject) => {
    const child = spawn("flock", ["-x", ...(opts.wait ? [] : ["-n"]), file, "-c", "echo locked; exec cat"], { stdio: ["pipe", "pipe", "inherit"] });
    let got = false;
    const waiting = opts.wait ? setTimeout(() => opts.onWaiting?.(holder()), 100) : undefined;
    child.stdout.on("data", (d: Buffer) => {
      if (got || !d.toString().includes("locked")) return;
      got = true;
      clearTimeout(waiting);
      fs.writeFileSync(holderFile, `${process.env.NODE_NAME ?? os.hostname()} pid ${process.pid}`);
      resolve({ release: () => child.stdin.end() });
    });
    child.on("error", (e) => reject(new Error(`flock(1) is required: ${e.message}`)));
    child.on("exit", (code) => {
      clearTimeout(waiting);
      if (!got) reject(code === 1 ? new FolderLocked(holder()) : new Error(`flock exited ${code}`));
    });
  });
}
