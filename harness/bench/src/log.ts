import fs from "node:fs";
import path from "node:path";

/**
 * The bench folder's two write shapes. Logs are append-only, one JSON line per
 * write, O_APPEND + fsync: a crash loses at most the line being written. A
 * list is small and replaced whole through a temp file and rename, so a reader
 * sees the old list or the new one, never half of either.
 */
export function appendLine(file: string, value: unknown): void {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const fd = fs.openSync(file, "a");
  try {
    // A torn previous line has no LF; start ours on a fresh line so the torn
    // fragment stays one unreadable line instead of corrupting this one.
    const size = fs.fstatSync(fd).size;
    let lead = "";
    if (size > 0) {
      const b = Buffer.alloc(1);
      const rfd = fs.openSync(file, "r");
      try { fs.readSync(rfd, b, 0, 1, size - 1); } finally { fs.closeSync(rfd); }
      if (b[0] !== 0x0a) lead = "\n";
    }
    fs.writeSync(fd, lead + JSON.stringify(value) + "\n");
    fs.fsyncSync(fd);
  } finally {
    fs.closeSync(fd);
  }
}

export function readLines<T = unknown>(file: string): T[] {
  let text: string;
  try {
    text = fs.readFileSync(file, "utf8");
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code === "ENOENT") return [];
    throw e;
  }
  const out: T[] = [];
  for (const line of text.split("\n")) {
    if (!line.trim()) continue;
    try {
      out.push(JSON.parse(line) as T);
    } catch {
      /* a torn line from a crash mid-write */
    }
  }
  return out;
}

export function replaceJson(file: string, value: unknown): void {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const tmp = `${file}.${process.pid}.tmp`;
  const fd = fs.openSync(tmp, "w");
  try {
    fs.writeSync(fd, JSON.stringify(value, null, 2));
    fs.fsyncSync(fd);
  } finally {
    fs.closeSync(fd);
  }
  fs.renameSync(tmp, file);
}

export function readJson<T>(file: string, fallback: T): T {
  try {
    return JSON.parse(fs.readFileSync(file, "utf8")) as T;
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code === "ENOENT") return fallback;
    throw e;
  }
}
