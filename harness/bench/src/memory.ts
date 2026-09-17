import fs from "node:fs";
import path from "node:path";

/**
 * What the person told us, kept where their work is kept: `{bench dir}/memory/`, one file per
 * memory plus `MEMORY.md`, an index of one line each. The index rides in every session's identity,
 * so a preference stated once is honoured in every session afterwards — it is the PERSON's memory,
 * not the session's (owner, 2026-09-17: "just like claude code will update the knowledge").
 *
 * The bench owns the files because a workspace session has no bench filesystem: it saves through
 * the same door (`POST /memory`) and the bench writes. Being under the bench folder, a memory is
 * snapshotted and replicated with everything else there.
 */
export type MemoryType = "user" | "feedback" | "project" | "reference";
export type Memory = { name: string; description: string; type: MemoryType };

const TYPES: MemoryType[] = ["user", "feedback", "project", "reference"];
/** A name becomes a file name, so it is a slug and nothing else — never a path, never a walk. */
const NAME = /^[a-z0-9][a-z0-9-]{0,63}$/;

export class Memories {
  private dir: string;
  constructor(benchDir: string) {
    this.dir = path.join(benchDir, "memory");
  }

  private file(name: string) {
    if (!NAME.test(name)) throw new Error(`a memory is named in lowercase words with dashes, not ${JSON.stringify(name)}`);
    return path.join(this.dir, `${name}.md`);
  }

  save(m: Memory & { body: string }): Memory[] {
    if (!TYPES.includes(m.type)) throw new Error(`a memory is ${TYPES.join(", ")} — not ${JSON.stringify(m.type)}`);
    if (!m.description?.trim()) throw new Error("a memory needs a one-line description; it is what the index shows");
    const at = this.file(m.name);
    fs.mkdirSync(this.dir, { recursive: true });
    // Frontmatter first so the file reads on its own, in an editor or in a diff.
    fs.writeFileSync(at, `---\nname: ${m.name}\ndescription: ${m.description.trim()}\ntype: ${m.type}\n---\n\n${m.body.trim()}\n`);
    return this.reindex();
  }

  forget(name: string): Memory[] {
    fs.rmSync(this.file(name), { force: true });
    return this.reindex();
  }

  all(): Memory[] {
    if (!fs.existsSync(this.dir)) return [];
    return fs
      .readdirSync(this.dir)
      .filter((f) => f.endsWith(".md") && f !== "MEMORY.md")
      .map((f) => {
        const head = fs.readFileSync(path.join(this.dir, f), "utf8").split("---")[1] ?? "";
        const field = (k: string) => new RegExp(`^${k}:\\s*(.*)$`, "m").exec(head)?.[1]?.trim() ?? "";
        return { name: field("name") || path.basename(f, ".md"), description: field("description"), type: (field("type") || "reference") as MemoryType };
      })
      .sort((a, b) => a.name.localeCompare(b.name));
  }

  /** The body of one memory, for a person reading it in Settings. */
  read(name: string): string {
    return fs.readFileSync(this.file(name), "utf8");
  }

  /** The index, rewritten from the files: they are the record, this is the view of them. */
  private reindex(): Memory[] {
    const rows = this.all();
    fs.mkdirSync(this.dir, { recursive: true });
    fs.writeFileSync(path.join(this.dir, "MEMORY.md"), rows.length ? `# Memory\n\n${rows.map((m) => `- **${m.name}** (${m.type}) — ${m.description}`).join("\n")}\n` : "");
    return rows;
  }

  /** What every session's identity carries: the index, or nothing at all when there is none. */
  index(): string {
    const at = path.join(this.dir, "MEMORY.md");
    try {
      return fs.readFileSync(at, "utf8").trim();
    } catch {
      return "";
    }
  }
}
