import hljs from "highlight.js/lib/core";
import rust from "highlight.js/lib/languages/rust";
import typescript from "highlight.js/lib/languages/typescript";
import json from "highlight.js/lib/languages/json";
import ini from "highlight.js/lib/languages/ini";
import yaml from "highlight.js/lib/languages/yaml";
import bash from "highlight.js/lib/languages/bash";
import css from "highlight.js/lib/languages/css";
import markdown from "highlight.js/lib/languages/markdown";

/**
 * Syntax highlighting, one line at a time.
 *
 * Only the languages this app actually opens are registered, so the bundle
 * carries a handful of grammars rather than all two hundred. Colour comes from
 * the theme's own tokens (see `hljs-*` in app.css), never from a highlight.js
 * stylesheet, so code follows the app between light and dark like everything
 * else.
 *
 * Lines are highlighted independently. That is wrong for a construct that spans
 * lines — a block comment, a multi-line string — and right for everything else,
 * and it keeps a diff simple: each row is a row, whichever side of the change it
 * came from.
 */
hljs.registerLanguage("rust", rust);
hljs.registerLanguage("typescript", typescript);
hljs.registerLanguage("json", json);
hljs.registerLanguage("ini", ini);
hljs.registerLanguage("yaml", yaml);
hljs.registerLanguage("bash", bash);
hljs.registerLanguage("css", css);
hljs.registerLanguage("markdown", markdown);

const BY_EXTENSION: Record<string, string> = {
  rs: "rust",
  ts: "typescript",
  tsx: "typescript",
  js: "typescript",
  jsx: "typescript",
  json: "json",
  toml: "ini",
  yaml: "yaml",
  yml: "yaml",
  sh: "bash",
  bash: "bash",
  css: "css",
  md: "markdown",
};

const BY_NAME: Record<string, string> = {
  Dockerfile: "bash",
  Makefile: "bash",
};

/** The language a path is written in, or undefined when we should not guess. */
export function languageOf(path: string): string | undefined {
  const name = path.split("/").pop() ?? path;
  if (BY_NAME[name]) return BY_NAME[name];
  const ext = name.includes(".") ? name.split(".").pop()! : "";
  return BY_EXTENSION[ext];
}

const escape = (s: string) =>
  s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");

/** One line as HTML. Falls back to the escaped text when the language is unknown. */
export function highlight(line: string, language?: string): string {
  if (!line) return "&nbsp;";
  if (!language) return escape(line);
  try {
    return hljs.highlight(line, { language, ignoreIllegals: true }).value;
  } catch {
    return escape(line);
  }
}
