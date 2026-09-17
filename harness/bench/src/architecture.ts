import fs from "node:fs";
import path from "node:path";

/**
 * The space's architecture, as ONE living document (spec §24). The bench holds it so it can answer
 * "what talks to what, on which port, with which shape" itself — the owner's rule is that the top
 * level already knows the architecture and the contracts, and asks a workspace only about
 * implementation.
 *
 * Markdown, because a person reads it and a model writes it: one `##` per component, plus a
 * `## Contracts` table that is the one structured part. Everything here is pure file work over
 * `{bench}/.bench/architecture.md`; who may write it is the bench's business, not this file's.
 */
export type Contract = { method: string; path: string; shape: string; owner: string };

export const CONTRACTS = "Contracts";
const HEAD = "# Architecture";
const TABLE_HEAD = "| Endpoint | Request → response | Owner |";
const TABLE_RULE = "| --- | --- | --- |";

/** `METHOD /path — request → response — owner`, which is what a reply's `contracts:` line carries. */
export function parseContract(line: string): Contract | undefined {
  const t = line.trim().replace(/^[-*]\s*/, "");
  if (!t || /^none$/i.test(t)) return undefined;
  const m = /^([A-Z]+)\s+(\S+)\s*(?:—|--|-)\s*([\s\S]*?)\s*(?:(?:—|--|-)\s*([^—-]+))?$/.exec(t);
  if (!m) return undefined;
  return { method: m[1], path: m[2], shape: (m[3] ?? "").trim(), owner: (m[4] ?? "").trim() };
}

/** A contract's identity is the METHOD and the PATH; a second sighting updates it, never doubles it. */
const key = (c: Contract) => `${c.method} ${c.path}`;

export class Architecture {
  private file: string;
  constructor(benchDir: string) {
    this.file = path.join(benchDir, ".bench", "architecture.md");
  }

  path_(): string {
    return this.file;
  }

  read(): string {
    try {
      return fs.readFileSync(this.file, "utf8");
    } catch {
      return "";
    }
  }

  write(text: string): string {
    fs.mkdirSync(path.dirname(this.file), { recursive: true });
    fs.writeFileSync(this.file, text.endsWith("\n") ? text : `${text}\n`);
    return this.read();
  }

  /**
   * One section, replaced or added. A section is a `##` heading and everything under it, so a
   * workspace can rewrite what it owns without touching what it does not.
   */
  setSection(section: string, text: string): string {
    const name = section.trim().replace(/^#+\s*/, "");
    const body = `## ${name}\n\n${text.trim()}\n`;
    const doc = this.read() || `${HEAD}\n\n`;
    const sections = split(doc);
    const at = sections.findIndex((s) => heading(s).toLowerCase() === name.toLowerCase());
    if (at >= 0) sections[at] = body;
    else sections.push(body);
    return this.write(join(doc, sections));
  }

  /** The Contracts table, merged: an endpoint already there is updated in place (dedupe by method+path). */
  mergeContracts(rows: Contract[]): string {
    if (!rows.length) return this.read();
    const have = new Map(this.contracts().map((c) => [key(c), c] as const));
    for (const c of rows) have.set(key(c), { ...have.get(key(c)), ...c });
    const table = [
      TABLE_HEAD,
      TABLE_RULE,
      ...[...have.values()].map((c) => `| \`${c.method} ${c.path}\` | ${c.shape || "—"} | ${c.owner || "—"} |`),
    ].join("\n");
    return this.setSection(CONTRACTS, table);
  }

  /** What the table says now, read back as rows. */
  contracts(): Contract[] {
    const section = split(this.read()).find((s) => heading(s).toLowerCase() === CONTRACTS.toLowerCase());
    if (!section) return [];
    return section
      .split("\n")
      .map((l) => /^\|\s*`?([A-Z]+)\s+(\S+?)`?\s*\|\s*([^|]*)\|\s*([^|]*)\|/.exec(l.trim()))
      .filter((m): m is RegExpExecArray => !!m)
      .map((m) => ({ method: m[1], path: m[2], shape: m[3].trim().replace(/^—$/, ""), owner: m[4].trim().replace(/^—$/, "") }));
  }

  /**
   * The first version of the document, from what the bench can already see: its workspaces and the
   * environment's services. A document that starts empty is a document nobody writes; one that
   * starts with the machines in it is a document somebody corrects.
   */
  seed(input: { workspaces?: { name?: string; id: string; packages?: string[] }[]; services?: { name: string; image?: string; ports?: (number | { port?: number })[] }[] }): string {
    if (this.read().trim()) return this.read();
    const out = [HEAD, "", "What runs where, and what talks to what. The bench keeps this; a workspace", "updates its own section with `architecture` or the `contracts:` line of a reply.", ""];
    for (const w of input.workspaces ?? []) {
      out.push(`## ${w.name || w.id}`, "", `- workspace \`${w.id}\``, ...(w.packages?.length ? [`- packages: ${w.packages.join(", ")}`] : []), "- what runs here: not said yet", "");
    }
    for (const s of input.services ?? []) {
      const ports = (s.ports ?? []).map((p) => (typeof p === "number" ? p : p.port)).filter(Boolean);
      out.push(`## ${s.name} (service)`, "", ...(s.image ? [`- image: \`${s.image}\``] : []), ...(ports.length ? [`- ports: ${ports.join(", ")}`] : []), "- used by: not said yet", "");
    }
    out.push(`## ${CONTRACTS}`, "", TABLE_HEAD, TABLE_RULE, "");
    return this.write(out.join("\n"));
  }
}

/** The document, cut into its `##` sections; everything before the first one is the preamble. */
function split(doc: string): string[] {
  const parts = doc.split(/\n(?=## )/);
  return parts.slice(1).map((s) => (s.endsWith("\n") ? s : `${s}\n`));
}
const heading = (section: string) => (/^##\s+(.*)$/m.exec(section)?.[1] ?? "").trim();
const join = (doc: string, sections: string[]) => {
  const preamble = doc.split(/\n(?=## )/)[0];
  return [preamble.replace(/\s+$/, ""), "", ...sections].join("\n");
};

/**
 * A work reply must end with a `contracts:` line (§24): `none`, or one item per line. This reads it
 * back — the rows it names, and whether the line was there at all, because a reply without one is
 * bounced once rather than quietly losing what changed.
 */
export function readContractsLine(reply: string): { said: boolean; rows: Contract[] } {
  const lines = String(reply ?? "").split("\n");
  // The LAST one wins: an agent that quotes the instruction earlier in its report has not answered.
  const at = lines.map((l) => /^\s*contracts:\s*(.*)$/i.exec(l)).reduce((best, m, i) => (m ? i : best), -1);
  if (at < 0) return { said: false, rows: [] };
  const head = /^\s*contracts:\s*(.*)$/i.exec(lines[at])![1];
  const rest = lines.slice(at + 1).filter((l) => /^\s*[-*]|^\s*[A-Z]+\s+\//.test(l));
  return { said: true, rows: [head, ...rest].map(parseContract).filter((c): c is Contract => !!c) };
}

/** What the harness says back when a reply forgot the line. Said once, never twice. */
export const CONTRACTS_BOUNCE = "[harness] add the contracts: line — `none`, or one item per line as `METHOD /path — request → response — owner`";

/**
 * What to do with a reply: the contracts it named, and whether to ask for the line. Only a §18 WORK
 * REPLY is asked — a workspace answering a person in its own tab is a conversation, not a report —
 * and it is asked once, because a missing line must never turn into a stuck ask.
 */
export function onReply(answer: string, alreadyBounced: boolean): { rows: Contract[]; nudge: boolean } {
  const { said, rows } = readContractsLine(answer);
  const isReport = /^(DONE_WITH_CONCERNS|DONE|BLOCKED|NEEDS_CONTEXT)\b/.test(answer.trim().replace(/^\[reply [^\]]+\]\s*/, ""));
  return { rows, nudge: isReport && !said && !alreadyBounced };
}
