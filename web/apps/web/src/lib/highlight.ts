import "server-only";
import { createHighlighter, type Highlighter, type BundledLanguage } from "shiki";

/** Server-side syntax highlighting. One highlighter for the process; themes are
 *  emitted as CSS variables (--shiki-light / --shiki-dark) so the page's theme
 *  class picks the colour with no re-render and no client JavaScript. */
let instance: Promise<Highlighter> | null = null;

function highlighter() {
  instance ??= createHighlighter({
    themes: ["github-light", "github-dark"],
    langs: [
      "rust", "toml", "yaml", "json", "markdown", "bash", "typescript", "tsx",
      "javascript", "jsx", "hcl", "dockerfile", "diff", "python", "go", "java",
      "kotlin", "swift", "ruby", "php", "c", "cpp", "csharp", "sql", "css",
      "scss", "html", "vue", "svelte", "lua", "zig", "dart", "elixir", "haskell",
      "scala", "ini", "xml", "graphql", "proto", "nix", "make",
    ],
  });
  return instance;
}

/** Extension to grammar. Kept beside `lib/languages.ts`, which maps the same
 *  extensions to display names and colours — one answers "how do I colour this
 *  file", the other "what is this repo written in", and a file type missing from
 *  either is a file that looks wrong in one place and invisible in the other. */
const BY_EXT: Record<string, BundledLanguage> = {
  rs: "rust", toml: "toml", yaml: "yaml", yml: "yaml", json: "json", jsonc: "json",
  md: "markdown", mdx: "markdown", sh: "bash", bash: "bash", zsh: "bash", fish: "bash",
  ts: "typescript", mts: "typescript", cts: "typescript", tsx: "tsx",
  js: "javascript", mjs: "javascript", cjs: "javascript", jsx: "jsx",
  tf: "hcl", hcl: "hcl", diff: "diff", patch: "diff",
  py: "python", go: "go", java: "java", kt: "kotlin", kts: "kotlin", swift: "swift",
  rb: "ruby", php: "php", c: "c", h: "c", cpp: "cpp", cc: "cpp", cxx: "cpp",
  hpp: "cpp", hh: "cpp", cs: "csharp", sql: "sql", css: "css", scss: "scss",
  sass: "scss", html: "html", htm: "html", vue: "vue", svelte: "svelte",
  lua: "lua", zig: "zig", dart: "dart", ex: "elixir", exs: "elixir",
  hs: "haskell", scala: "scala", sbt: "scala", ini: "ini", cfg: "ini",
  xml: "xml", svg: "xml", graphql: "graphql", gql: "graphql", proto: "proto",
  nix: "nix", mk: "make",
};

/** Files whose type is their whole name. */
const BY_NAME: Record<string, BundledLanguage> = {
  Dockerfile: "dockerfile",
  Containerfile: "dockerfile",
  Makefile: "make",
  Gemfile: "ruby",
  Rakefile: "ruby",
  ".gitignore": "ini",
  ".dockerignore": "ini",
  ".env": "ini",
};

export function langFor(path: string): BundledLanguage | "text" {
  const base = path.split("/").pop() ?? "";
  if (BY_NAME[base]) return BY_NAME[base];
  // A dotfile with nothing after the leading dot has no extension to read.
  const dot = base.lastIndexOf(".");
  if (dot <= 0) return "text";
  return BY_EXT[base.slice(dot + 1).toLowerCase()] ?? "text";
}

/** Returns `<pre class="shiki"><code>…</code></pre>` with one `<span class="line"
 *  id="L<n>" data-line="<n>">` per line, so numbers and anchors are CSS and links. */
export async function highlight(code: string, lang: BundledLanguage | "text") {
  const h = await highlighter();
  const html = await h.codeToHtml(code, {
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
  return blockLines(html);
}

/**
 * Shiki separates its line spans with a literal newline:
 * `<span class="line">…</span>\n<span class="line">…</span>`.
 *
 * That is right for its default inline lines — the newline IS the line break.
 * We render `.line` as a block so a line number, a hover band and a `:target`
 * highlight can span the full width, and then the newline is a second break: a
 * stray text node inside a `<pre>`, which preserves whitespace, so every line of
 * code is followed by an empty one and the whole block comes out double-spaced.
 *
 * Removing the separators is the fix that keeps both — block lines, single
 * spacing. Only newlines BETWEEN line spans are touched; whitespace inside a
 * line is code and is left exactly as it is. Blank lines keep their height
 * because `.line::before` gives every line inline content.
 */
function blockLines(html: string) {
  return html.replace(/<\/span>\n<span class="line"/g, '</span><span class="line"');
}
