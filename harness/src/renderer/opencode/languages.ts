/**
 * harness: the languages a workspace actually shows. shiki's `bundledLanguages` is every grammar it
 * ships — importing the map lazily code-splits ~600 chunks into the build (22 MB), and a bench
 * renders Rust, Go, TypeScript and a shell, not ABAP. Anything outside this list highlights as
 * plain text, which is what shiki already does for an unknown language.
 *
 * One place, so the restriction is a single edit to re-vendor around.
 */
import type { LanguageRegistration } from "shiki";
import bash from "@shikijs/langs/bash";
import css from "@shikijs/langs/css";
import diff from "@shikijs/langs/diff";
import dockerfile from "@shikijs/langs/docker";
import go from "@shikijs/langs/go";
import html from "@shikijs/langs/html";
import javascript from "@shikijs/langs/javascript";
import json from "@shikijs/langs/json";
import jsx from "@shikijs/langs/jsx";
import markdown from "@shikijs/langs/markdown";
import python from "@shikijs/langs/python";
import rust from "@shikijs/langs/rust";
import shellscript from "@shikijs/langs/shellscript";
import sql from "@shikijs/langs/sql";
import toml from "@shikijs/langs/toml";
import tsx from "@shikijs/langs/tsx";
import typescript from "@shikijs/langs/typescript";
import yaml from "@shikijs/langs/yaml";

/** Every grammar we ship, by the name a fence can carry (their own aliases included). */
export const LANGUAGES: Record<string, LanguageRegistration[]> = {
  bash, sh: bash, shell: bash, zsh: bash, shellscript,
  css,
  diff, patch: diff,
  dockerfile, docker: dockerfile,
  go, golang: go,
  html,
  javascript, js: javascript,
  json,
  jsx,
  markdown, md: markdown,
  python, py: python,
  rust, rs: rust,
  sql,
  toml,
  tsx,
  typescript, ts: typescript,
  yaml, yml: yaml,
};

/** `true` when we ship a grammar for this fence; anything else highlights as plain text. */
export const known = (language: string): boolean => language in LANGUAGES;

/**
 * harness: what a file extension is CALLED, for an attachment's chip. shiki's own
 * `bundledLanguagesInfo` carries this for every grammar it ships and drags the whole bundle in with
 * it; these are the names for the languages we ship, and anything else falls back to the extension
 * in capitals exactly as upstream does.
 */
export const LANGUAGE_NAMES = new Map<string, string>(
  Object.entries({
    bash: "Bash", sh: "Bash", shell: "Bash", zsh: "Bash",
    css: "CSS", diff: "Diff", patch: "Diff", dockerfile: "Dockerfile",
    go: "Go", html: "HTML", js: "JavaScript", javascript: "JavaScript", jsx: "JSX",
    json: "JSON", md: "Markdown", markdown: "Markdown", py: "Python", python: "Python",
    rs: "Rust", rust: "Rust", sql: "SQL", toml: "TOML", ts: "TypeScript", typescript: "TypeScript",
    tsx: "TSX", yaml: "YAML", yml: "YAML",
  }),
);
