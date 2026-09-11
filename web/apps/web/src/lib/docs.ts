import "server-only";
import fs from "node:fs/promises";
import path from "node:path";
import { Marked, type Token, type Tokens } from "marked";
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

/** The sidebar, in reading order. Slugs are paths under `docs/product` without `.md`; a
 *  section's own page is generated from its children when no `index.md` exists. Titles are the
 *  files' own `# ` lines, read at request time, so a rename in the markdown is a rename here. */
export const NAV: { section: string; items: string[] }[] = [
  { section: "Start here", items: ["", "concepts/overview", "tutorials/01-first-workspace", "best-practices"] },
  {
    section: "Concepts",
    items: [
      "concepts/workspaces",
      "concepts/environments",
      "concepts/snapshots",
      "concepts/connections",
      "concepts/agents",
      "concepts/git-repositories",
      "concepts/container-repositories",
    ],
  },
  { section: "Tutorials", items: ["tutorials/01-first-workspace", "tutorials/02-agent-driven-flow"] },
  {
    section: "How-to · Workspaces",
    items: [
      "how-to/workspaces/create",
      "how-to/workspaces/update",
      "how-to/workspaces/exec-commands",
      "how-to/workspaces/read-and-write-files",
      "how-to/workspaces/run-background-processes",
      "how-to/workspaces/install-packages",
      "how-to/workspaces/clone",
      "how-to/workspaces/work-in-many",
      "how-to/workspaces/discard",
    ],
  },
  {
    section: "How-to · Environments",
    items: [
      "how-to/environments/create",
      "how-to/environments/update",
      "how-to/environments/inspect-services",
      "how-to/environments/snapshot",
      "how-to/environments/clone",
      "how-to/environments/clone-from-snapshot",
    ],
  },
  {
    section: "How-to · Connections",
    items: [
      "how-to/connections/connect",
      "how-to/connections/switch-environment",
      "how-to/connections/intercept",
      "how-to/connections/release-intercept",
    ],
  },
  {
    section: "Troubleshooting",
    items: [
      "how-to/troubleshooting/workspace-wont-start",
      "how-to/troubleshooting/cannot-reach-service",
      "how-to/troubleshooting/dead-intercept",
    ],
  },
  {
    section: "Reference",
    items: ["reference/cli/index", "reference/api/workspaces", "reference/api/environments", "reference/limits-and-defaults", "reference/glossary"],
  },
];

/** A directory that is linked to as a section (`how-to/`, `reference/`) but has no page of its
 *  own: its index is generated from the NAV items beneath it. */
const SECTION_DIRS = ["concepts", "tutorials", "how-to", "how-to/workspaces", "how-to/environments", "how-to/connections", "how-to/troubleshooting", "reference", "reference/api", "reference/cli"];

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

/** Every page once, in sidebar order, for prev/next — the two "Start here" repeats are skipped
 *  where they recur. */
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
  const dir = fromSlug.includes("/") ? fromSlug.slice(0, fromSlug.lastIndexOf("/")) : fromSlug === "" ? "" : "";
  let joined = path.posix.normalize(path.posix.join(dir, target));
  joined = joined.replace(/\/$/, "").replace(/\.md$/, "").replace(/(^|\/)index$/, "");
  if (joined === ".") joined = "";
  return hrefOf(joined) + (hash ? `#${hash}` : "");
}

async function render(md: string, slug: string): Promise<{ html: string; headings: Heading[] }> {
  const headings: Heading[] = [];
  const body = md.replace(/^#\s+.+\n?/, "");
  const m = new Marked({ gfm: true, async: true });
  m.use({
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
        const html = t.html ?? `<pre class="shiki"><code>${escapeHtml(t.text)}</code></pre>`;
        return `<div class="docs-code" data-lang="${lang}"><div class="docs-code-bar"><span>${lang || "text"}</span><button type="button" class="docs-copy" data-copy>Copy</button></div>${html}</div>\n`;
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
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

/** The first paragraph, for the page header and `<meta name="description">`. */
function describe(md: string): string {
  const body = md.replace(/^#\s+.+\n?/, "").trim();
  const para = body.split(/\n\s*\n/).find((p) => p && !p.startsWith("#") && !p.startsWith("-") && !p.startsWith("|") && !p.startsWith("```")) ?? "";
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
    const list = items.map((i) => `<li><a href="${i.href}">${i.title}</a></li>`).join("");
    return {
      slug,
      title: humanize(slug),
      description: `${items.length} pages`,
      html: `<ul class="docs-section-list">${list}</ul>`,
      headings: [],
      section: humanize(slug),
      prev: null,
      next: null,
    };
  }
  return null;
}

/** Every page's title, section and h2/h3 headings: the search index the palette filters. */
export async function searchIndex(): Promise<{ title: string; section: string; href: string; headings: { text: string; id: string }[] }[]> {
  const out = [];
  for (const it of await flat()) {
    const md = await readFile(it.slug);
    const headings = md
      ? [...md.matchAll(/^(##|###)\s+(.+)$/gm)].map((m) => ({ text: m[2].replace(/[*_`]/g, "").trim(), id: slugify(m[2]) }))
      : [];
    out.push({ title: it.title, section: (await nav()).find((s) => s.items.some((x) => x.slug === it.slug))?.section ?? "Docs", href: it.href, headings });
  }
  return out;
}
