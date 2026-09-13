/**
 * Building `POST /import` bodies is pure: given rows and file bytes already
 * read from disk, decide what is safe to send and how to split it under the
 * bench's body cap. Reading files (main.ts) and posting them (bench-client.ts)
 * are the only parts that touch the outside world, so this is what
 * `node --test` can hold without an Electron runtime.
 */

export type ImportRow = { id: string; name: string; seq: number; lastActive?: number; archived?: boolean };
export type ImportItem = { row: { id: string; name: string; seq: number; created: number; lastActive: number; archived: boolean }; name: string; content?: string };
export type LooseFile = { name: string; content: string };
export type ImportBody = { items: ImportItem[]; loose: LooseFile[] };

const JSONL_BASENAME = /^[^/\\]+\.jsonl$/;

/** A file name the bench will accept: a plain `*.jsonl` basename, never a path. */
export function safeJsonlName(name: string): string | undefined {
  return JSONL_BASENAME.test(name) ? name : undefined;
}

/** Only the old laptop's own bench sessions (`bench`, `s-N`) are ever imported; a thread id never is. */
export function isLaptopRow(id: string): boolean {
  return /^(bench|s-\d+)$/.test(id);
}

export function toItem(r: ImportRow, fileName: string, content?: string): ImportItem {
  return {
    row: { id: r.id, name: r.name, seq: r.seq, created: r.lastActive ?? Date.now(), lastActive: r.lastActive ?? Date.now(), archived: !!r.archived },
    name: fileName,
    content,
  };
}

// Headroom under the server's 64 MiB cap (bench/src/server.ts MAX_BODY): batches split
// here, not at the cap itself, so JSON framing overhead never tips one over.
export const MAX_BATCH_BYTES = 60 * 1024 * 1024;

/**
 * Splits items and loose files into POST /import bodies, each kept under
 * maxBytes. ponytail: a single file bigger than maxBytes still ships alone
 * (and risks a 413) rather than being silently dropped — split that file
 * itself if that ever happens for real.
 */
export function batchImport(items: ImportItem[], loose: LooseFile[], maxBytes = MAX_BATCH_BYTES): ImportBody[] {
  const batches: ImportBody[] = [];
  let cur: ImportBody = { items: [], loose: [] };
  let curBytes = 2; // "{}"
  const add = (kind: "items" | "loose", v: ImportItem | LooseFile) => {
    const bytes = Buffer.byteLength(JSON.stringify(v)) + 1;
    if ((cur.items.length || cur.loose.length) && curBytes + bytes > maxBytes) {
      batches.push(cur);
      cur = { items: [], loose: [] };
      curBytes = 2;
    }
    (cur[kind] as (ImportItem | LooseFile)[]).push(v as never);
    curBytes += bytes;
  };
  for (const it of items) add("items", it);
  for (const l of loose) add("loose", l);
  if (cur.items.length || cur.loose.length) batches.push(cur);
  return batches;
}
