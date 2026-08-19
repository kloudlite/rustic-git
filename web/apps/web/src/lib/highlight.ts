import "server-only";
import { createHighlighter, type Highlighter, type BundledLanguage } from "shiki";

/** Server-side syntax highlighting. One highlighter for the process; themes are
 *  emitted as CSS variables (--shiki-light / --shiki-dark) so the page's theme
 *  class picks the colour with no re-render and no client JavaScript. */
let instance: Promise<Highlighter> | null = null;

function highlighter() {
  instance ??= createHighlighter({
    themes: ["github-light", "github-dark"],
    langs: ["rust", "toml", "yaml", "json", "markdown", "bash", "typescript", "tsx", "javascript", "hcl", "dockerfile", "diff"],
  });
  return instance;
}

const BY_EXT: Record<string, BundledLanguage> = {
  rs: "rust", toml: "toml", yaml: "yaml", yml: "yaml", json: "json", md: "markdown",
  sh: "bash", ts: "typescript", tsx: "tsx", js: "javascript", tf: "hcl", diff: "diff",
};

export function langFor(path: string): BundledLanguage | "text" {
  const base = path.split("/").pop() ?? "";
  if (base === "Dockerfile") return "dockerfile";
  return BY_EXT[base.split(".").pop() ?? ""] ?? "text";
}

/** Returns `<pre class="shiki"><code>…</code></pre>` with one `<span class="line"
 *  id="L<n>" data-line="<n>">` per line, so numbers and anchors are CSS and links. */
export async function highlight(code: string, lang: BundledLanguage | "text") {
  const h = await highlighter();
  return h.codeToHtml(code, {
    lang: lang === "text" ? "text" : lang,
    themes: { light: "github-light", dark: "github-dark" },
    defaultColor: false,
    transformers: [
      {
        line(node, line) {
          node.properties.id = `L${line}`;
          node.properties["data-line"] = String(line);
        },
      },
    ],
  });
}
