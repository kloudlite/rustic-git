/**
 * Diffs, read into what a diff view draws (`resolveFileDiff` / `patchFiles`, the render contract
 * §1.9–§1.11). Pure, because everything interesting here is parsing: which file a patch touches,
 * whether it created, deleted or moved it, and how many lines each way.
 *
 * A patch arrives as ordinary unified text; an `edit` arrives as old/new pairs instead, and both
 * end up in the same shape so one view draws both.
 */
export type DiffLine = { kind: "context" | "add" | "del" | "sep"; text: string; old?: number; new?: number };
export type DiffFile = {
  path: string;
  /** Where it went, when it moved: the accordion header says `old → new`. */
  from?: string;
  type: "add" | "delete" | "move" | "edit";
  additions: number;
  deletions: number;
  lines: DiffLine[];
};

const HUNK = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(.*)$/;

/** `a/bins/x.rs` → `bins/x.rs`; `/dev/null` stays, it is how a create or a delete is told. */
const strip = (p: string) => (p === "/dev/null" ? p : p.replace(/^[ab]\//, ""));

/**
 * A unified diff, per file. Anything that is not a recognised header or body line is ignored:
 * `diff --git`, `index`, mode lines and a tool's own chatter are not part of what is drawn.
 */
export function patchFiles(text: string): DiffFile[] {
  const out: DiffFile[] = [];
  let f: DiffFile | undefined;
  let oldPath = "";
  let oldN = 0;
  let newN = 0;
  for (const line of String(text ?? "").split("\n")) {
    if (line.startsWith("--- ")) {
      oldPath = strip(line.slice(4).trim());
      continue;
    }
    if (line.startsWith("+++ ")) {
      const newPath = strip(line.slice(4).trim());
      const type = oldPath === "/dev/null" ? "add" : newPath === "/dev/null" ? "delete" : oldPath !== newPath ? "move" : "edit";
      f = {
        path: newPath === "/dev/null" ? oldPath : newPath,
        from: type === "move" ? oldPath : undefined,
        type,
        additions: 0,
        deletions: 0,
        lines: [],
      };
      out.push(f);
      continue;
    }
    if (!f) continue;
    const h = HUNK.exec(line);
    if (h) {
      oldN = Number(h[1]);
      newN = Number(h[3]);
      // The separator between hunks IS the line info, the way opencode's `line-info-basic` draws it.
      f.lines.push({ kind: "sep", text: line.replace(/\s+$/, "") });
      continue;
    }
    if (line.startsWith("+")) f.lines.push({ kind: "add", text: line.slice(1), new: newN++ }), f.additions++;
    else if (line.startsWith("-")) f.lines.push({ kind: "del", text: line.slice(1), old: oldN++ }), f.deletions++;
    else if (line.startsWith(" ") || line === "") f.lines.push({ kind: "context", text: line.slice(1), old: oldN++, new: newN++ });
  }
  return out;
}

/**
 * The `edit` tool's own shape: replacements, not a patch. Each pair is one hunk — the old lines
 * then the new ones — which is exactly what the person asked for and all the tool told us.
 */
export function editFile(path: string, edits: { oldText: string; newText: string }[]): DiffFile {
  const f: DiffFile = { path, type: "edit", additions: 0, deletions: 0, lines: [] };
  edits.forEach((e, i) => {
    if (i) f.lines.push({ kind: "sep", text: "" });
    for (const t of e.oldText.replace(/\n$/, "").split("\n")) f.lines.push({ kind: "del", text: t }), f.deletions++;
    for (const t of e.newText.replace(/\n$/, "").split("\n")) f.lines.push({ kind: "add", text: t }), f.additions++;
  });
  return f;
}

/** `Created` / `Deleted` / `Moved`, or the +/− counts (`message-part.tsx:2404`). */
export function badge(f: DiffFile): { text: string; type: "added" | "removed" | "modified" } | undefined {
  if (f.type === "add") return { text: "Created", type: "added" };
  if (f.type === "delete") return { text: "Deleted", type: "removed" };
  if (f.type === "move") return { text: "Moved", type: "modified" };
  return undefined;
}

/** The last segment and what is above it — the accordion header's two halves. */
export function split(path: string): { dir: string; name: string } {
  const i = path.lastIndexOf("/");
  return i < 0 ? { dir: "", name: path } : { dir: path.slice(0, i + 1), name: path.slice(i + 1) };
}
