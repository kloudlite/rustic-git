/**
 * What a repo is written in, worked out from the tree.
 *
 * Nothing serves this, and computing it properly means reading every file. The
 * honest approximation is what a file is CALLED and how big it is: extensions are
 * how editors decide too, and blob sizes come back with the tree for free — so a
 * breakdown costs no extra request per file.
 *
 * It is an approximation, and the places it is wrong are worth knowing: a
 * generated file counts like a written one, and vendored code counts like yours.
 */

export type Language = { name: string; color: string };

/** Colours follow GitHub's linguist, so the bar reads the way people expect. */
const LANGUAGES: Record<string, Language> = {
  rs: { name: "Rust", color: "#dea584" },
  ts: { name: "TypeScript", color: "#3178c6" },
  tsx: { name: "TypeScript", color: "#3178c6" },
  js: { name: "JavaScript", color: "#f1e05a" },
  jsx: { name: "JavaScript", color: "#f1e05a" },
  mjs: { name: "JavaScript", color: "#f1e05a" },
  cjs: { name: "JavaScript", color: "#f1e05a" },
  py: { name: "Python", color: "#3572A5" },
  go: { name: "Go", color: "#00ADD8" },
  java: { name: "Java", color: "#b07219" },
  kt: { name: "Kotlin", color: "#A97BFF" },
  swift: { name: "Swift", color: "#F05138" },
  rb: { name: "Ruby", color: "#701516" },
  php: { name: "PHP", color: "#4F5D95" },
  c: { name: "C", color: "#555555" },
  h: { name: "C", color: "#555555" },
  cpp: { name: "C++", color: "#f34b7d" },
  cc: { name: "C++", color: "#f34b7d" },
  hpp: { name: "C++", color: "#f34b7d" },
  cs: { name: "C#", color: "#178600" },
  scala: { name: "Scala", color: "#c22d40" },
  ex: { name: "Elixir", color: "#6e4a7e" },
  exs: { name: "Elixir", color: "#6e4a7e" },
  erl: { name: "Erlang", color: "#B83998" },
  hs: { name: "Haskell", color: "#5e5086" },
  lua: { name: "Lua", color: "#000080" },
  zig: { name: "Zig", color: "#ec915c" },
  dart: { name: "Dart", color: "#00B4AB" },
  sh: { name: "Shell", color: "#89e051" },
  bash: { name: "Shell", color: "#89e051" },
  fish: { name: "Shell", color: "#89e051" },
  zsh: { name: "Shell", color: "#89e051" },
  css: { name: "CSS", color: "#663399" },
  scss: { name: "SCSS", color: "#c6538c" },
  html: { name: "HTML", color: "#e34c26" },
  vue: { name: "Vue", color: "#41b883" },
  svelte: { name: "Svelte", color: "#ff3e00" },
  sql: { name: "SQL", color: "#e38c00" },
  md: { name: "Markdown", color: "#083fa1" },
  mdx: { name: "MDX", color: "#fcb32c" },
  json: { name: "JSON", color: "#292929" },
  yaml: { name: "YAML", color: "#cb171e" },
  yml: { name: "YAML", color: "#cb171e" },
  toml: { name: "TOML", color: "#9c4221" },
  tf: { name: "HCL", color: "#844FBA" },
  hcl: { name: "HCL", color: "#844FBA" },
  proto: { name: "Protocol Buffers", color: "#e8b71a" },
  nix: { name: "Nix", color: "#7e7eff" },
};

/** Files that describe the build rather than the program. Counting a lockfile as
 *  JSON makes a lockfile-heavy repo look like a JSON repo, which is a lie about
 *  what it is. */
const IGNORED = new Set([
  "package-lock.json", "bun.lock", "bun.lockb", "yarn.lock", "pnpm-lock.yaml",
  "Cargo.lock", "poetry.lock", "composer.lock", "go.sum",
]);

const IGNORED_DIRS = new Set([
  "node_modules", "vendor", "dist", "build", "target", ".git", "third_party",
]);

export function isIgnoredDir(name: string) {
  return IGNORED_DIRS.has(name);
}

export function languageOf(filename: string): Language | undefined {
  if (IGNORED.has(filename)) return undefined;
  if (filename === "Dockerfile") return { name: "Dockerfile", color: "#384d54" };
  if (filename === "Makefile") return { name: "Makefile", color: "#427819" };
  // A dotfile with no other extension (`.gitignore`) is configuration, not code.
  const dot = filename.lastIndexOf(".");
  if (dot <= 0) return undefined;
  return LANGUAGES[filename.slice(dot + 1).toLowerCase()];
}

export type LanguageShare = Language & { pct: number };

/** Shares by byte count, largest first. Anything under a whole percent is folded
 *  into "Other" rather than drawn as a sliver nobody can see or click. */
export function breakdown(files: { name: string; size: number | null }[]): LanguageShare[] {
  const bytes = new Map<string, { color: string; total: number }>();
  for (const f of files) {
    const lang = languageOf(f.name);
    if (!lang || !f.size) continue;
    const at = bytes.get(lang.name) ?? { color: lang.color, total: 0 };
    at.total += f.size;
    bytes.set(lang.name, at);
  }
  const total = [...bytes.values()].reduce((n, v) => n + v.total, 0);
  if (total === 0) return [];

  const all = [...bytes.entries()]
    .map(([name, v]) => ({ name, color: v.color, pct: (v.total / total) * 100 }))
    .sort((a, b) => b.pct - a.pct);

  const shown = all.filter((l) => l.pct >= 1);
  const rest = all.filter((l) => l.pct < 1).reduce((n, l) => n + l.pct, 0);
  if (rest > 0) shown.push({ name: "Other", color: "#9ca3af", pct: rest });
  return shown.map((l) => ({ ...l, pct: Math.round(l.pct * 10) / 10 }));
}
