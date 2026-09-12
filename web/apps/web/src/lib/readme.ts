import "server-only";
import { Marked, type Token, type Tokens } from "marked";
import { fenceLang, highlight } from "@/lib/highlight";

/** A README, rendered to HTML on the server.
 *
 *  The app used to render READMEs through react-markdown and the docs through marked, so two
 *  markdown parsers, two sanitizing stances and two escaping bugs to keep track of shipped in one
 *  bundle (2026-09-12). This is the docs' pipeline — marked, gfm, shiki — with the page's own type
 *  scale as classes instead of the docs' stylesheet, so a README reads as part of the page.
 *
 *  What react-markdown gave for free and is therefore written out here: raw HTML in the source is
 *  DROPPED rather than rendered, every attribute this writes is escaped, and a link or image URL
 *  keeps only the protocols a document may name — so `javascript:` never reaches the page. That
 *  is the whole reason `dangerouslySetInnerHTML` is acceptable on the result.
 */
const SCHEME = /^([a-z][a-z0-9+.-]*):/i;
const ALLOWED = new Set(["http", "https", "mailto", "tel"]);

function escapeHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

/** A URL as an attribute value, or nothing. No scheme at all means a relative URL, which is a
 *  link into the repository and always fine; a scheme must be one a document may name. */
function safeUrl(href: string): string {
  const u = href.trim();
  const scheme = SCHEME.exec(u)?.[1].toLowerCase();
  return !scheme || ALLOWED.has(scheme) ? escapeHtml(u) : "";
}

export async function renderReadme(md: string): Promise<string> {
  const m = new Marked({ gfm: true, async: true });
  m.use({
    walkTokens: async (token: Token) => {
      if (token.type === "code") {
        const t = token as Tokens.Code & { html?: string };
        const word = t.lang?.split(/\s+/)[0];
        if (word?.toLowerCase() !== "mermaid") t.html = await highlight(t.text, fenceLang(word));
      }
    },
    renderer: {
      // Raw HTML is the author's bytes; dropping it is the stance react-markdown's `skipHtml` took.
      html: () => "",
      heading({ tokens, depth }: Tokens.Heading): string {
        const text = this.parser.parseInline(tokens);
        const cls = {
          1: "text-title font-semibold tracking-title",
          2: "mt-2 border-b border-border pb-1.5 text-body font-semibold",
          3: "mt-1 text-sm2 font-semibold",
        }[depth] ?? "text-sm2 font-semibold";
        return `<h${depth} class="${cls}">${text}</h${depth}>`;
      },
      paragraph({ tokens }: Tokens.Paragraph): string {
        return `<p class="text-foreground/90">${this.parser.parseInline(tokens)}</p>`;
      },
      link({ href, title, tokens }: Tokens.Link): string {
        const to = safeUrl(href);
        const text = this.parser.parseInline(tokens);
        if (!to) return text;
        return `<a href="${to}"${title ? ` title="${escapeHtml(title)}"` : ""} rel="noopener noreferrer" class="text-primary underline-offset-4 hover:underline">${text}</a>`;
      },
      // A README's images live wherever its author put them — no host list to allow, so the
      // optimizer has nothing to work with and a plain img is the honest element.
      image({ href, text }: Tokens.Image): string {
        const src = safeUrl(href);
        return src ? `<img src="${src}" alt="${escapeHtml(text ?? "")}" class="max-w-full" />` : "";
      },
      list(token: Tokens.List): string {
        const items = token.items.map((i) => this.listitem(i)).join("");
        return token.ordered
          ? `<ol class="grid list-decimal gap-1 pl-5">${items}</ol>`
          : `<ul class="grid list-square gap-1 pl-5">${items}</ul>`;
      },
      blockquote({ tokens }: Tokens.Blockquote): string {
        return `<blockquote class="grid gap-2 border-l-2 border-border pl-4 text-muted-foreground">${this.parser.parse(tokens)}</blockquote>`;
      },
      hr: () => `<hr class="border-border" />`,
      codespan({ text }: Tokens.Codespan): string {
        return `<code class="bg-muted px-1 font-mono text-caption">${escapeHtml(text)}</code>`;
      },
      table({ header, rows }: Tokens.Table): string {
        const cell = (c: Tokens.TableCell, tag: "th" | "td", cls: string) =>
          `<${tag} class="${cls}"${c.align ? ` align="${c.align}"` : ""}>${this.parser.parseInline(c.tokens)}</${tag}>`;
        const th = header.map((c) => cell(c, "th", "border border-border bg-muted/40 px-2.5 py-1 text-left font-semibold")).join("");
        const trs = rows.map((r) => `<tr>${r.map((c) => cell(c, "td", "border border-border px-2.5 py-1")).join("")}</tr>`).join("");
        return `<div class="overflow-x-auto"><table class="border-collapse text-caption"><thead><tr>${th}</tr></thead><tbody>${trs}</tbody></table></div>`;
      },
      code(token: Tokens.Code): string {
        const t = token as Tokens.Code & { html?: string };
        const word = t.lang?.split(/\s+/)[0]?.toLowerCase();
        // A diagram, not code: drawn client-side by `MermaidBlocks`, the fence's own source as
        // the fallback until it is — and for good, if it will not parse.
        if (word === "mermaid") {
          return `<div class="border border-border bg-muted/30"><pre data-mermaid class="max-h-96 overflow-auto px-4 py-3 font-mono text-caption">${escapeHtml(t.text)}</pre><div class="mermaid-block overflow-x-auto px-4 py-3" hidden></div></div>`;
        }
        const html = t.html ?? `<pre class="shiki"><code>${escapeHtml(t.text)}</code></pre>`;
        return `<div class="border border-border bg-muted/30"><div class="code-block w-full overflow-x-auto"><div>${html}</div></div></div>`;
      },
    },
  });
  return m.parse(md);
}
