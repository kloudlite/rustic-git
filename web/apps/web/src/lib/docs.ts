import "server-only";
import fs from "node:fs/promises";
import path from "node:path";
import { Marked, type Token, type Tokens, type TokenizerAndRendererExtension } from "marked";
import { fenceLang, highlight } from "@/lib/highlight";

/** The product documentation, `docs/product` in the repository, served at `/docs`.
 *
 *  The markdown is the source of truth and lives outside the web workspace on purpose: it is
 *  read here, never copied into the app tree by hand. A build that cannot see the repository
 *  (the web image is built from `web/` alone) ships the same files under `content/docs`, which
 *  `deploy/dev/pod/ship.sh` and `web.yml` copy in before building; that directory wins when it
 *  exists so a production build and a dev server read the same tree. */
const CANDIDATES = [path.join(process.cwd(), "content", "docs"), path.resolve(process.cwd(), "..", "..", "..", "docs", "product")];

async function root(): Promise<string> {
  for (const c of CANDIDATES) {
    try {
      await fs.access(path.join(c, "index.md"));
      return c;
    } catch {}
  }
  return CANDIDATES[0];
}

/** The sidebar, in reading order. Slugs are paths under `docs/product` without `.md`. Titles
 *  are the files' own `# ` lines, read at request time, so a rename in the markdown is a rename
 *  here. A group is one product surface — the same shape a reader already knows from every
 *  other platform's docs — so a page is where its name says it is. */
export const NAV: { section: string; items: string[] }[] = [
  { section: "Introduction", items: ["", "quick-start", "authentication"] },
  { section: "Concepts", items: ["concepts/workspaces", "concepts/environments", "concepts/snapshots", "concepts/storage", "concepts/teams-and-quota"] },
  { section: "Workspaces", items: ["workspaces/create", "workspaces/packages", "workspaces/lifecycle", "workspaces/clone-and-restore", "workspaces/ssh"] },
  { section: "Environments", items: ["environments/create", "environments/services", "environments/lifecycle", "environments/clone-and-restore"] },
  { section: "Snapshots", items: ["snapshots/push", "snapshots/history", "snapshots/volumes"] },
  { section: "Connections", items: ["connections/attach", "connections/intercepts"] },
  { section: "Agent tools", items: ["agent-tools/exec", "agent-tools/files", "agent-tools/images", "agent-tools/git"] },
  { section: "Human tools", items: ["human-tools/console", "human-tools/kl-connect", "human-tools/editors"] },
  { section: "Platform", items: ["platform/regions", "platform/teams", "platform/quota", "platform/requests"] },
  {
    section: "Reference",
    items: ["reference/api/index", "reference/api/workspaces", "reference/api/environments", "reference/api/snapshots", "reference/api/platform", "reference/cli/kl-connect", "reference/cli/kl", "reference/limits", "reference/glossary"],
  },
];

/** A directory that is linked to as a section but has no page of its own: its index is
 *  generated from the NAV items beneath it. */
const SECTION_DIRS = ["concepts", "workspaces", "environments", "snapshots", "connections", "agent-tools", "human-tools", "platform", "reference", "reference/cli"];

export type NavItem = { slug: string; href: string; title: string };
export type NavSection = { section: string; items: NavItem[] };
export type Heading = { id: string; text: string; depth: number };
export type Page = {
  slug: string;
  title: string;
  description: string;
  html: string;
  headings: Heading[];
  section: string;
  prev: NavItem | null;
  next: NavItem | null;
};

export function hrefOf(slug: string): string {
  const s = slug.replace(/\/index$/, "");
  return s ? `/docs/${s}` : "/docs";
}

const titleCache = new Map<string, string>();

async function readFile(slug: string): Promise<string | null> {
  const file = path.join(await root(), `${slug || "index"}.md`);
  try {
    return await fs.readFile(file, "utf8");
  } catch {
    return null;
  }
}

async function titleOf(slug: string): Promise<string> {
  const hit = titleCache.get(slug);
  if (hit && process.env.NODE_ENV === "production") return hit;
  const md = await readFile(slug);
  const t = md?.match(/^#\s+(.+)$/m)?.[1]?.trim() ?? humanize(slug);
  titleCache.set(slug, t);
  return t;
}

function humanize(slug: string): string {
  const last = slug.split("/").filter(Boolean).pop() ?? "Docs";
  return last.replace(/^\d+-/, "").replace(/-/g, " ").replace(/\b\w/g, (c) => c.toUpperCase());
}

export async function nav(): Promise<NavSection[]> {
  return Promise.all(
    NAV.map(async (s) => ({
      section: s.section,
      items: await Promise.all(s.items.map(async (slug) => ({ slug, href: hrefOf(slug), title: slug === "" ? "Introduction" : await titleOf(slug) }))),
    })),
  );
}

/** Every page once, in sidebar order, for prev/next. */
async function flat(): Promise<NavItem[]> {
  const seen = new Set<string>();
  const out: NavItem[] = [];
  for (const s of await nav()) for (const it of s.items) if (!seen.has(it.slug)) { seen.add(it.slug); out.push(it); }
  return out;
}

function slugify(text: string): string {
  return text.toLowerCase().replace(/<[^>]+>/g, "").replace(/[^\w\s-]/g, "").trim().replace(/\s+/g, "-");
}

/** A markdown link, as written in the file, to the route it lands on. Relative `.md` links are
 *  resolved against the page's own directory; a link to a directory is that section's page. */
function rewriteHref(href: string, fromSlug: string): string {
  if (/^(https?:|mailto:|#)/.test(href)) return href;
  const [target, hash] = href.split("#");
  const dir = fromSlug.includes("/") ? fromSlug.slice(0, fromSlug.lastIndexOf("/")) : "";
  let joined = path.posix.normalize(path.posix.join(dir, target));
  joined = joined.replace(/\/$/, "").replace(/\.md$/, "").replace(/(^|\/)index$/, "");
  if (joined === ".") joined = "";
  return hrefOf(joined) + (hash ? `#${hash}` : "");
}

/** `::: tabs` … `:::`, `::: cards` … `:::`, `::: note|tip|warning [Title]` … `:::` — the three
 *  block containers the pages use. A tabs container holds fenced blocks whose info string names
 *  the tab, ```` ```bash [kl-connect] ````; a cards container holds one list whose items are
 *  `[Title](link) — one line`; a callout holds ordinary markdown. */
type Container = Tokens.Generic & { kind: string; title: string; tokens: Token[] };
const CONTAINER_RE = /^:::\s*(tabs|cards|note|tip|warning)(?:[ \t]+([^\n]+?))?[ \t]*\n([\s\S]*?)\n:::[ \t]*(?:\n|$)/;
const container: TokenizerAndRendererExtension = {
  name: "container",
  level: "block",
  start(src: string) {
    return src.match(/^:::/m)?.index;
  },
  tokenizer(src: string) {
    const m = CONTAINER_RE.exec(src);
    if (!m) return undefined;
    const tok: Container = { type: "container", raw: m[0], kind: m[1], title: m[2] ?? "", tokens: [] };
    this.lexer.blockTokens(m[3], tok.tokens);
    return tok;
  },
  renderer(token) {
    const t = token as Container;
    if (t.kind === "tabs") {
      const codes = t.tokens.filter((x): x is Tokens.Code => x.type === "code");
      const names = codes.map((c, i) => tabName(c.lang) || `Tab ${i + 1}`);
      const bar = names.map((n, i) => `<button type="button" role="tab" data-tab="${escapeHtml(n)}" aria-selected="${i === 0}">${escapeHtml(n)}</button>`).join("");
      const panels = codes.map((c, i) => `<div role="tabpanel" data-tab="${escapeHtml(names[i])}"${i === 0 ? "" : " hidden"}>${this.parser.parse([c])}</div>`).join("");
      return `<div class="docs-tabs"><div class="docs-tabs-bar" role="tablist">${bar}</div>${panels}</div>\n`;
    }
    if (t.kind === "cards") {
      const list = t.tokens.find((x): x is Tokens.List => x.type === "list");
      const cards = (list?.items ?? []).map((it) => {
        const inline = it.tokens.find((x): x is Tokens.Text | Tokens.Paragraph => x.type === "text" || x.type === "paragraph");
        const html = inline ? this.parser.parseInline(inline.tokens ?? []) : "";
        const m = /^<a href="([^"]+)"[^>]*>(.*?)<\/a>\s*(?:—|-|:)?\s*([\s\S]*)$/.exec(html);
        if (!m) return `<div class="docs-card">${html}</div>`;
        return `<a class="docs-card" href="${m[1]}"><span class="docs-card-title">${m[2]}</span>${m[3] ? `<span class="docs-card-text">${m[3]}</span>` : ""}</a>`;
      });
      return `<div class="docs-cards">${cards.join("")}</div>\n`;
    }
    const label = t.title || { note: "Note", tip: "Tip", warning: "Warning" }[t.kind];
    return `<aside class="docs-callout is-${t.kind}"><div class="docs-callout-label">${escapeHtml(label ?? "")}</div><div class="docs-callout-body">${this.parser.parse(t.tokens)}</div></aside>\n`;
  },
};

function tabName(lang: string | undefined): string {
  return /\[(.+?)\]/.exec(lang ?? "")?.[1] ?? "";
}

async function render(md: string, slug: string): Promise<{ html: string; headings: Heading[] }> {
  const headings: Heading[] = [];
  const body = md.replace(/^#\s+.+\n?/, "");
  const m = new Marked({ gfm: true, async: true });
  m.use({
    extensions: [container],
    walkTokens: async (token: Token) => {
      if (token.type === "code") {
        const t = token as Tokens.Code & { html?: string };
        t.html = await highlight(t.text, fenceLang(t.lang?.split(/\s+/)[0]));
      }
    },
    renderer: {
      heading({ tokens, depth }: Tokens.Heading): string {
        const text = this.parser.parseInline(tokens);
        const id = slugify(text);
        if (depth === 2 || depth === 3) headings.push({ id, text: text.replace(/<[^>]+>/g, ""), depth });
        return `<h${depth} id="${id}"><a class="docs-anchor" href="#${id}" aria-label="Link to this section">${text}</a></h${depth}>\n`;
      },
      link({ href, title, tokens }: Tokens.Link): string {
        const text = this.parser.parseInline(tokens);
        const to = rewriteHref(href, slug);
        const ext = /^https?:/.test(to) ? ` target="_blank" rel="noreferrer"` : "";
        return `<a href="${to}"${title ? ` title="${title}"` : ""}${ext}>${text}</a>`;
      },
      code(token: Tokens.Code): string {
        const t = token as Tokens.Code & { html?: string };
        const lang = t.lang?.split(/\s+/)[0] ?? "";
        const title = /title="([^"]+)"/.exec(t.lang ?? "")?.[1] ?? tabName(t.lang) ?? "";
        const html = t.html ?? `<pre class="shiki"><code>${escapeHtml(t.text)}</code></pre>`;
        return `<div class="docs-code" data-lang="${lang}"><div class="docs-code-bar"><span>${escapeHtml(title || lang || "text")}</span><button type="button" class="docs-copy" data-copy>Copy</button></div>${html}</div>\n`;
      },
      table({ header, rows }: Tokens.Table): string {
        const th = header.map((c) => `<th${c.align ? ` align="${c.align}"` : ""}>${this.parser.parseInline(c.tokens)}</th>`).join("");
        const trs = rows
          .map((r) => `<tr>${r.map((c) => `<td${c.align ? ` align="${c.align}"` : ""}>${this.parser.parseInline(c.tokens)}</td>`).join("")}</tr>`)
          .join("");
        return `<div class="docs-table"><table><thead><tr>${th}</tr></thead><tbody>${trs}</tbody></table></div>\n`;
      },
    },
  });
  const html = await m.parse(body);
  return { html, headings };
}

function escapeHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

/** The first paragraph, for the page header and `<meta name="description">`. */
function describe(md: string): string {
  const body = md.replace(/^#\s+.+\n?/, "").trim();
  const para = body.split(/\n\s*\n/).find((p) => p && !/^[#\-|`:>]/.test(p)) ?? "";
  return para.replace(/\s+/g, " ").replace(/[*_`]/g, "").replace(/\[([^\]]+)\]\([^)]+\)/g, "$1").slice(0, 200);
}

function sectionOf(slug: string, sections: NavSection[]): string {
  for (const s of sections) if (s.items.some((i) => i.slug === slug)) return s.section;
  return "Docs";
}

export async function page(slugParts: string[]): Promise<Page | null> {
  const slug = slugParts.join("/");
  const sections = await nav();
  const order = await flat();
  const at = order.findIndex((i) => i.slug === slug || i.slug === `${slug}/index`);
  const prev = at > 0 ? order[at - 1] : null;
  const next = at >= 0 && at < order.length - 1 ? order[at + 1] : null;
  const md = (await readFile(slug)) ?? (await readFile(`${slug}/index`));
  if (md) {
    const { html, headings } = await render(md, slug);
    return { slug, title: slug === "" ? "Kloudlite Documentation" : await titleOf(slug), description: describe(md), html, headings, section: sectionOf(slug, sections), prev, next };
  }
  if (SECTION_DIRS.includes(slug)) {
    const items = order.filter((i) => i.slug.startsWith(`${slug}/`));
    const list = items.map((i) => `<a class="docs-card" href="${i.href}"><span class="docs-card-title">${i.title}</span></a>`).join("");
    return { slug, title: humanize(slug), description: `${items.length} pages`, html: `<div class="docs-cards">${list}</div>`, headings: [], section: humanize(slug), prev: null, next: null };
  }
  return null;
}

/** Every page's title, section and h2/h3 headings: the search index the palette filters. */
export async function searchIndex(): Promise<{ title: string; section: string; href: string; headings: { text: string; id: string }[] }[]> {
  const out = [];
  const sections = await nav();
  for (const it of await flat()) {
    const md = await readFile(it.slug);
    const headings = md
      ? [...md.matchAll(/^(##|###)\s+(.+)$/gm)].map((m) => ({ text: m[2].replace(/[*_`]/g, "").trim(), id: slugify(m[2]) }))
      : [];
    out.push({ title: it.title, section: sections.find((s) => s.items.some((x) => x.slug === it.slug))?.section ?? "Docs", href: it.href, headings });
  }
  return out;
}
