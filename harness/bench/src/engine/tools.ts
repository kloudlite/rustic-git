import { dirname, basename, matchesGlob } from "node:path";
import { child, chain, type Envelope } from "./task.ts";
import { choice, noul, type Ask, type ChoiceAnswer, type NoulAnswer } from "./jev.ts";
import { pickBlocks } from "./pick.ts";
import { ladder } from "./chooser.ts";
import { remote, tryRemote, text } from "./remote.ts";

// The threshold chooser.ts uses for tool-choice confidence; defined here since chooser.ts imports tools.ts and re-exports it.
export const ACT = 0.9;
export const DECIDE = 0.5; // Jev's confidence below this pops the decision to the user
const MAX_CANDIDATES = 254; // Jev allows 255 options; one is "none"

export type RunCtx = {
  envelope?: Envelope; user?: User; seen?: Set<string>;
  detail?: string; // what the planner found and decided for this step; goes to the params writer only, never to a Jev pick
  ask?: Ask;
  generate?: (prompt: string, envelope: Envelope) => Promise<string>;
  approve?: (args: Record<string, string>) => Promise<boolean>;
  soft?: boolean; // the confirm exists only because the tool always asks: args the tool calls safe skip it
  onArgs?: (args: Record<string, string>) => void;
  trace?: (line: string) => void;
  think?: (question: string) => Promise<string>; // a task only: the one LLM call a step may make to settle a choice
  // ponytail: only run's spawned child honours this; every other tool runs to completion once started, so an
  // interrupted step that only e.g. wrote a file still finishes that write.
  signal?: AbortSignal;
};
export type Candidates = string | ((cwd: string, ctx?: RunCtx) => Promise<string[]>);
export type Param = { name: string; description: string; kind: "closed" | "free"; literal?: false; question?: string; unsure?: string; candidates?: Candidates; requiredUnless?: (args: Record<string, string>) => boolean };
// The user only talks to the main session; sessions reach the user through these tools.
export type User = { tell: (message: string, fixed?: boolean) => void; ask: (question: string) => Promise<string> };
export const TALK = ["tell_user", "ask_user"];
// brief is a short (~60 char) line for the planner, which only needs to know what a tool is for; Jev's own tool vote and
// the "no tool matches" message use the full description, since picking the right tool needs the whole of it.
export type Tool = { name: string; description: string; brief?: string; shows?: string; params: Param[]; outcomes: string[]; messages?: Record<string, string>; safe?: (args: Record<string, string>) => boolean; run: (cwd: string, args: Record<string, string>, ctx?: RunCtx) => Promise<string> };

export const RESPONSES = ["done", "failed", "blocked", "other"];

// Quoted into one shell line where a command is built from parts, since the pod's exec tool takes a command string.
const q = (s: string) => `'${s.replace(/'/g, "'\\''")}'`;

export let BG_WAIT_MS = 10000;
export const setBgWaitMs = (ms: number) => { BG_WAIT_MS = ms; };

// Background processes are tracked by the pod now, not here: candidates and the planner's "what's running" line both
// come from process_list.
export const procIds = async (cwd: string) => {
  const r = await tryRemote(cwd, "process_list", {});
  return typeof r === "string" ? [] : ((r as { processes: { id: string }[] }).processes ?? []).map((p) => p.id);
};

const PROMPT_FILES = 60; // project paths shown to the params writer, so it knows what it can read before it writes a value
// ponytail: cached per cwd for CACHE_MS, so 2-3 candidate calls in one tool call share a single git ls-files/package.json read; can read up to 2s stale after a write.
const CACHE_MS = 2000;
const filesCache = new Map<string, { at: number; v: string[] }>();
export const invalidateCache = (cwd: string) => { filesCache.delete(cwd); scriptCache.delete(cwd); digestCache.delete(cwd); };
async function cached(cache: Map<string, { at: number; v: string[] }>, cwd: string, compute: () => Promise<string[]>): Promise<string[]> {
  const hit = cache.get(cwd);
  if (hit && Date.now() - hit.at < CACHE_MS) return hit.v;
  const v = await compute();
  cache.set(cwd, { at: Date.now(), v });
  return v;
}
// File listing is the pod's own glob now (it already applies the project's ignore rules); the ignored-but-manifested-folder
// special case this used to walk by hand is the pod's `paths::confine`/glob concern, not the bench's.
const ignoredDirs = async (_cwd: string) => [] as string[];
const files = (cwd: string) => cached(filesCache, cwd, async () =>
  ((await remote(cwd, "glob", { pattern: "**/*" })) as { paths: string[] }).paths);
// Candidates read "name: body" so Jev sees what each script does ("dev: node index.js").
// A script is a named command a project file declares. Every such file in the project counts, not the root's alone: a script of an
// app in a folder is led by that folder ("api/dev: node index.js") and runs in it. Seen live: with the root's scripts only, "run the
// server" started the wrong project's start script.
type Script = { key: string; label: string; command: string };
// The package manager is the one whose lockfile sits beside the package.json; npm when there is none.
const LOCKS: [string, string][] = [["pnpm-lock.yaml", "pnpm"], ["yarn.lock", "yarn"], ["bun.lock", "bun"], ["bun.lockb", "bun"]];
// ponytail: TOML is read for `key = "text"` lines under a named table only, with no parser installed; a task written as an inline
// table or an array is not listed, and "other" with a shell command covers it. Install a TOML parser when one is missed.
const tomlTable = (raw: string, table: string): [string, string][] => {
  const body = raw.split(/^\[/m).find((t) => t.startsWith(`${table}]`)) ?? "";
  return [...body.matchAll(/^\s*"?([\w.:-]+)"?\s*=\s*(?:"([^"]*)"|'([^']*)')\s*(?:#.*)?$/gm)].map((m) => [m[1], m[2] ?? m[3]]);
};
// A pyproject task runner is named by the table its tasks sit in.
const PY_RUNNERS: [string, string][] = [["tool.poe.tasks", "poe"], ["tool.pdm.scripts", "pdm run"], ["tool.poetry.scripts", "poetry run"]];
const MANIFESTS: Record<string, (raw: string, dir: string, all: string[]) => [name: string, body: string, run: string][]> = {
  "package.json": (raw, dir, all) => {
    const pm = LOCKS.find(([f]) => all.includes(dir + f))?.[1] ?? "npm";
    return Object.entries(JSON.parse(raw).scripts ?? {}).map(([k, v]) => [k, String(v), k === "test" ? `${pm} test` : `${pm} run ${q(k)}`]);
  },
  "deno.json": (raw) => Object.entries(JSON.parse(raw).tasks ?? {}).map(([k, v]) => [k, String(v), `deno task ${q(k)}`]),
  "composer.json": (raw) => Object.entries(JSON.parse(raw).scripts ?? {}).map(([k, v]) => [k, String(v), `composer run-script ${q(k)}`]),
  "pyproject.toml": (raw) => PY_RUNNERS.flatMap(([table, run]) => tomlTable(raw, table).map(([k, v]): [string, string, string] => [k, v, `${run} ${q(k)}`])),
  // Cargo declares no scripts: its commands are fixed, and listed for a crate (a workspace root has no [package]).
  "Cargo.toml": (raw) => (/^\[package\]/m.test(raw) ? ["run", "test", "build"] : []).map((k) => [k, `cargo ${k}`, `cargo ${k}`]),
  "Makefile": (raw) => [...raw.matchAll(/^([A-Za-z0-9][\w.-]*):(?!=)/gm)].map((m) => [m[1], `make ${m[1]}`, `make ${q(m[1])}`]),
};
const scriptCache = new Map<string, { at: number; v: Script[] }>();
async function scriptList(cwd: string): Promise<Script[]> {
  const hit = scriptCache.get(cwd);
  if (hit && Date.now() - hit.at < CACHE_MS) return hit.v;
  const all = await files(cwd).catch(() => [] as string[]);
  const found = [...new Set([...Object.keys(MANIFESTS), ...all.filter((f) => basename(f) in MANIFESTS)])].sort((x, y) => x.split("/").length - y.split("/").length);
  // One batched read for every manifest file; a `files` entry the pod could not read (missing, unreadable) is skipped rather than thrown.
  const read = (await remote(cwd, "read", { paths: found }).catch(() => ({ files: [] }))) as { files?: { path: string; content?: string; error?: string }[] };
  const byPath = new Map((read.files ?? []).map((f) => [f.path, f]));
  const v = found.flatMap((f) => {
    const got = byPath.get(f);
    if (!got || got.error || got.content === undefined) return [];
    const dir = dirname(f) === "." ? "" : `${dirname(f)}/`;
    try { return MANIFESTS[basename(f)](got.content, dir, all).map(([name, body, run]) => ({ key: dir + name, label: `${dir}${name}: ${body}`, command: dir ? `cd ${q(dir)} && ${run}` : run })); } catch { return []; }
  });
  scriptCache.set(cwd, { at: Date.now(), v });
  return v;
}
const scripts = async (cwd: string) => (await scriptList(cwd)).map((x) => x.label);
// The value is a whole candidate, or the script's name as the user gave it. The longest name wins, so "test:unit" is not taken for "test".
export const scriptOf = async (cwd: string, given: string) => { const all = await scriptList(cwd); return all.find((x) => x.label === given)
  ?? all.filter((x) => given === x.key || given.startsWith(`${x.key}: `)).sort((x, y) => y.key.length - x.key.length)[0]; };
export const ALL = "all files";
// The pod's grep does the search now; "no matches" is a result, never an exception (mirrors the old git-grep exit-1 handling).
async function search(cwd: string, pattern: string, glob?: string): Promise<string> {
  const r = (await remote(cwd, "grep", { pattern, glob, mode: "content" }).catch((e) => `error: ${(e as Error).message}`)) as
    { matches?: { path: string; line: number; text: string }[] } | string;
  if (typeof r === "string") return r;
  return r.matches?.length ? r.matches.map((m) => `${m.path}:${m.line}:${m.text}`).join("\n") : "no matches";
}
// Task 6 gives recall its own bench tool, answered from the session log; the engine's own section files are gone with local fs.
const sectionLabels = async (_cwd: string): Promise<string[]> => [];

const RIVAL = 0.2; // a candidate at or above this share of Jev's vote is a rival worth showing the user
const READ_MAX = 16000; // chars; a file up to this size is returned whole, a larger one as an outline to read by line range
const SEARCH_BODY = 6000; // chars; 4000 turned a 144-line file into an outline and cost three more LLM rounds; a file found by concept is shown whole up to this size, as an outline above it
const CONCEPT_FILES = 5; // files read when "all files" is asked for a concept, not an exact text; 3 missed sample-node-app/index.js in the live probe when the repo's own router files competed

// ponytail: regex digest, JS/TS-biased; upgrade to a real symbol index if picks stay poor.
// Cheap scan for names defined/registered in a file, so Jev scores content, not just a bare path.
const DIGEST_RE = /\b(?:function|class)\s+(\w+)|\bconst\s+(\w+)\s*=|\bexport\s+(?:default\s+)?(\w+)|\b(?:app|router)\.(get|post|put|delete|patch)\(\s*['"`]([^'"`]+)['"`]/g;
const DIGEST_MAX_BYTES = 200 * 1024;
function fileDigest(text: string): string {
  const names = new Set<string>();
  let m: RegExpExecArray | null;
  DIGEST_RE.lastIndex = 0;
  while ((m = DIGEST_RE.exec(text)) && names.size < 12) {
    if (m[4] && m[5]) names.add(`${m[4]} ${m[5]}`);
    else names.add(m[1] ?? m[2] ?? m[3] ?? "");
  }
  return [...names].filter(Boolean).join(" ").slice(0, 160);
}
const digestCache = new Map<string, { at: number; v: Map<string, string> }>();
async function digestedPaths(cwd: string, paths: string[]): Promise<string[]> {
  const hit = digestCache.get(cwd);
  const cache = hit && Date.now() - hit.at < CACHE_MS ? hit.v : new Map<string, string>();
  if (!hit || Date.now() - hit.at >= CACHE_MS) digestCache.set(cwd, { at: Date.now(), v: cache });
  const missing = paths.filter((p) => !cache.has(p));
  if (missing.length) {
    // Batched read; no per-path size check (the pod owns confine/size) — approximate the old byte cap with total_lines*80, ponytail: a real char count needs the pod to report bytes.
    const read = (await remote(cwd, "read", { paths: missing }).catch(() => ({ files: [] }))) as { files?: { path: string; content?: string; total_lines?: number }[] };
    for (const f of read.files ?? []) cache.set(f.path, f.content !== undefined && (f.total_lines ?? 0) * 80 <= DIGEST_MAX_BYTES ? fileDigest(f.content) : "");
    for (const p of missing) if (!cache.has(p)) cache.set(p, "");
  }
  return paths.map((p) => { const d = cache.get(p) ?? ""; return d ? `${p}: ${d}` : p; });
}

// A file is never narrowed by relevance: what comes back is exact, and the reader asks for more by line range.
// ponytail: the outline is the digest regex per line, JS/TS-biased; a file it finds nothing in shows its head instead.
function fileView(path: string, body: string, range?: string, max = READ_MAX): string {
  const rows = body.split("\n"), num = (from: number, to: number) => rows.slice(from - 1, to).map((l, i) => `${from + i}| ${l}`).join("\n");
  const m = /(\d+)\D+(\d+)/.exec(range ?? "");
  if (m) return `${path} lines ${m[1]}-${Math.min(+m[2], rows.length)} of ${rows.length}\n${num(+m[1], +m[2])}`;
  if (text.length <= max) return `${path} (whole file, ${rows.length} lines)\n${num(1, rows.length)}`;
  const outline = rows.map((l, i) => (/^\S/.test(l) && new RegExp(DIGEST_RE.source).test(l) ? `${i + 1}| ${l}` : "")).filter(Boolean).join("\n").slice(0, max / 2);
  return `${path} (${rows.length} lines, too large to show whole: outline only; read it again with lines "from-to" for the code)\n${outline || num(1, 80)}`;
}
// The question names the param's role in the action: "which file is mentioned" splits the vote on a step that names two.
const pathParam: Param = { name: "path", description: "project-relative file path", kind: "closed", candidates: "glob",
  question: "Which file does this step change or create? Not a file it only takes code or text from." };

// Candidates for the talk tools' "key" param: the envelope's message keys plus "other" (free text).
const messageKeys = async (_cwd: string, ctx?: RunCtx) => [...Object.keys(ctx?.envelope?.messages ?? {}), "other"];
// The free text param is only required when the key is "other" (or unset, e.g. no envelope).
const onlyIfOther = (a: Record<string, string>) => !a.key || a.key === "other";

// Reads what the file is about to hold, before it lands, shared by write and edit: seen live, patch text ("@@", "+" lines)
// written as a new file broke every later run.
function cut(t: string) { const lines = t.split("\n"); return lines.length <= 20 ? t : `${lines.slice(0, 10).join("\n")}\n…\n${lines.slice(-10).join("\n")}`; }
async function sound(path: string, text: string, ctx?: RunCtx) {
  if (!ctx?.ask) return;
  const enrich = ctx.generate && ctx.envelope ? async () => (await ctx.generate!(`${path} is about to hold:\n${cut(text)}\n\nIn one sentence, state whether this is a well-formed file of its kind, or holds patch markers, leftover fragments or text that does not belong.`, child(ctx.envelope!, `is the new ${path} well-formed?`, {}, ["answered"]))) || undefined : undefined;
  const user = ctx.user ? async () => ({ type: "noul" as const, noul: (await ctx.user!.ask(`${path} may end up malformed:\n${cut(text)}\nWrite it anyway? (yes/no)`)).trim().toLowerCase().startsWith("y") ? 1 : 0 }) : undefined;
  const { answer } = await ladder(ctx.ask, { path, instruction: ctx.envelope?.instruction, file: cut(text) }, "sound",
    noul("Is this a well-formed file of its kind, with no patch markers, leftover fragments or text that does not belong?"), enrich, user).catch(() => ({ answer: undefined }));
  if (!answer) return; // no verdict is not a refusal: Jev being down must not stop every write
  ctx.trace?.(`${ind(ctx)}[picked] sound: ${answer.type === "noul" ? answer.noul.toFixed(2) : "?"}`);
  if (!(answer.type === "noul" && answer.noul >= DECIDE)) throw new Error(`the resulting ${path} would not be a well-formed file: read ${path} and send edit or write with a correct value`);
}

// A JS glob-style pattern (src/**/*.ts) against a project-relative path.
const globMatch = (p: string, pattern: string) => matchesGlob(p, pattern);

export const TOOLS: Tool[] = [
  { name: "tell_user", shows: "whether the message was delivered; the user does not reply", description: "Say a short status, answer or result in words to the user; no reply expected. Not for showing code or file contents: read does that",
    params: [
      { name: "key", description: "which fixed message to send, or \"other\" to write one", kind: "closed", candidates: messageKeys, question: "Which fixed message says what this step tells the user? Pick \"other\" when none does." },
      { name: "message", description: "the exact words for the user", kind: "free", requiredUnless: onlyIfOther },
    ],
    outcomes: ["told"],
    run: async (_cwd, a, ctx) => { if (!ctx?.user) return "error: no user channel"; const words = a.message ?? ctx.envelope?.messages[a.key] ?? ""; ctx.user.tell(words, !a.message); return `told: ${words}`; } }, // the words are the result: a fact about what the user was told is checked against them
  { name: "ask_user", shows: "the user's reply, in their own words", description: "Ask the user a question that only they can answer, and wait for the reply",
    params: [
      { name: "key", description: "which fixed question to send, or \"other\" to write one", kind: "closed", candidates: messageKeys, question: "Which fixed question is the one this step asks the user? Pick \"other\" when none is." },
      { name: "question", description: "the exact question for the user", kind: "free", requiredUnless: onlyIfOther },
    ],
    outcomes: ["answered", "no_answer"],
    run: async (_cwd, a, ctx) => (ctx?.user ? (await ctx.user.ask(a.question ?? ctx.envelope?.messages[a.key] ?? "")) || "no answer: the user did not reply. Do not ask again; go on with the best assumption" : "error: no user channel") },
  { name: "glob", shows: "every project file, one path per line relative to the project root", description: "List the project's files and folders: to see the layout or to find a file by its name (the entry file, a config file)", brief: "List project files and folders", outcomes: ["listed"],
    // Seen live: "list all files in the project directory (sample-node-app)" listed the whole project, since the tool could not take a folder.
    params: [{ name: "folder", description: "the folder to list", kind: "closed", unsure: "all folders", question: "Which one folder does this step ask to list? None when it asks for the whole project.",
      candidates: async (cwd) => [...new Set([...(await files(cwd)).flatMap((f) => f.split("/").slice(0, -1).map((_, i, a) => a.slice(0, i + 1).join("/"))), ...(await ignoredDirs(cwd))])] },
             { name: "pattern", description: "optional glob such as src/**/*.ts; left out, every file", kind: "free", requiredUnless: () => false }],
    messages: { listed: "Files:\n{tail}" },
    run: async (cwd, a) => (await files(cwd)).filter((f) => (!a.folder || a.folder === "all folders" || f.startsWith(`${a.folder}/`)) && (!a.pattern || globMatch(f, a.pattern))).join("\n") },
  // One tool for everything that runs: a package.json script is picked by Jev from a closed list and runs unasked; "other" is a shell command the LLM writes and the user always confirms.
  { name: "bash", shows: "the command's output, stdout then stderr, cut to the relevant parts when long; \"started in background as pN\" means it keeps running as process pN", description: "Run something: the tests, a package.json script (build, lint, dev, start), or any shell command: install dependencies, run a program, or start a server when the task asks for one, curl a URL, make, any git operation, which includes status, diff, and discarding, undoing or reverting uncommitted changes (git restore .). Also deletes, renames or moves a file or directory, and lists background processes. A long-running command keeps running in the background. Also the last resort: a single file or system action that no other tool covers (chmod, unzip) is one shell command here", brief: "Run a shell command, in the foreground or backgrounded",
    params: [
      { name: "command", description: "package.json script to run, or \"other\" for a shell command", kind: "closed", candidates: async (cwd) => [...(await scripts(cwd)), "other"], requiredUnless: (a) => !a.shell },
      { name: "shell", description: "the exact shell command, in the foreground: no &, nohup, pkill or kill. A long-running command is moved to the background and tracked automatically; a tracked process is stopped with kill_shell, never from here", kind: "free", requiredUnless: (a) => !a.command || a.command === "other" },
      { name: "background", description: "start a long-running server or watcher; answers its process id", kind: "closed", candidates: async () => ["true", "false"], requiredUnless: () => false },
    ],
    outcomes: ["succeeded", "failed", "started"],
    messages: { succeeded: "Succeeded:\n{tail}", failed: "Failed:\n{tail}", started: "Started in background:\n{tail}" },
    safe: (a) => !a.shell && !!a.command && a.command !== "other",
    run: async (cwd, a) => {
      let command = a.shell;
      if (!command && a.command !== "other") {
        // A given script name is checked against package.json: only a real script may skip the confirm; anything else is run as-is (a direct test call, or a name that just is a shell command).
        const script = await scriptOf(cwd, a.command);
        command = script ? script.command : a.command;
      }
      if (!command) throw new Error(`no package.json script "${a.command.split(":")[0]}"`);
      if (a.background === "true") {
        const r = await tryRemote(cwd, "exec", { cmd: command, detach: true });
        return typeof r === "string" ? r : `started in background as ${(r as { id: string }).id}`;
      }
      const r = await tryRemote(cwd, "exec", { cmd: command, timeout_ms: BG_WAIT_MS });
      const out = text(r);
      // A foreground call that timed out is not a failure to report as one: the model needs to know it can rerun in the background.
      return typeof r === "object" && r !== null && (r as { timed_out?: boolean }).timed_out
        ? `${out}\n(timed out after ${BG_WAIT_MS}ms; rerun with background: true if this was meant to keep running)`
        : out;
    } },
  // One tool for looking at code: a named file, or "all files" for a concept search. grep does exact text/regex search.
  { name: "read", shows: "numbered lines (N: text) of the file; a very large file as an outline of its definitions, to be read again by line range", description: "Read, find, get or show code or text: a whole file, a route, function, section or answer inside a file, or which files hold a concept the instruction names when the file is not known. For an exact text or regex, grep does that.", brief: "Read a file, or find which files hold a concept",
    params: [
      { name: "path", description: "project-relative file path, or \"all files\" to search every file", kind: "closed", unsure: ALL, candidates: async (cwd) => [...(await files(cwd)), ALL],
        // Asked as "file or all files" the vote splits evenly on a vague name ("the handoff": HANDOFF.md 0.54, all files 0.45); this wording gives 0.98.
        question: `Which file does the instruction most likely mean? Pick "${ALL}" only when no single file fits.` },
      { name: "lines", description: "optional line range \"from-to\" (\"120-200\") of a file already seen as an outline; left out, the whole file", kind: "free", literal: false, requiredUnless: () => false },
    ],
    outcomes: ["read", "found", "none", "not_found"],
    messages: { read: "{tail}", found: "Found:\n{tail}", none: "Nothing relevant found.", not_found: "No matches." },
    run: async (cwd, a, ctx) => {
      const all = a.path === ALL;
      if (all) {
        const query = ctx?.envelope?.instruction ?? "";
        // A concept ("the routes") is not a text pattern, and an LLM-written regex is a blind guess (it matched app. and missed router.).
        // Jev picks the files from the list by what the instruction means, then the lines inside them. No LLM call.
        if (!ctx?.ask) return "blocked: no exact text to search for; put it in backticks and use grep";
        const fileList = await files(cwd);
        const digested = await digestedPaths(cwd, fileList);
        const { blocks } = await pickBlocks(ctx.ask, digested.join("\n"), query, { lines: true, maxCalls: 100 });
        // Adjacent picked lines come back as one range: flatten to lines carrying each block's score, then sort by score and slice.
        // Recover the path from the known file list, not by splitting on ": " (a digest can itself contain ": ").
        const paths = blocks.flatMap((b) => b.text.split("\n").filter(Boolean).map((l) => ({ l, score: b.score ?? 0 })))
          .sort((x, y) => y.score - x.score)
          .map(({ l }) => fileList.find((f) => l === f || l.startsWith(f + ": ")))
          .filter((p): p is string => !!p)
          .slice(0, CONCEPT_FILES);
        // ponytail: a picked binary file is read as text; add a skip by extension when Jev ever picks one.
        const outs = await Promise.all(paths.map(async (p) => {
          const r = await tryRemote(cwd, "read", { path: p });
          if (typeof r === "string") return "";
          return fileView(p, (r as { content: string }).content, undefined, SEARCH_BODY);
        }));
        return outs.filter(Boolean).join("\n\n");
      }
      const r = await tryRemote(cwd, "read", { path: a.path });
      if (typeof r === "string") return r;
      return fileView(a.path, (r as { content: string }).content, a.lines);
    } },
  // Exact text or regex, across the project or narrowed to one file. read's "all files" branch instead finds files by what the instruction means, a concept, not a pattern.
  { name: "grep", shows: "the matching lines as path:line:text; nothing means no matches", description: "Find which files and lines mention an exact name, string, route or setting", brief: "Find files/lines matching an exact text or regex",
    params: [
      { name: "pattern", description: "an exact text or regex to search for", kind: "free" },
      { name: "path", description: "the one file to search, or \"all files\" to search every file", kind: "closed", candidates: async (cwd) => [...(await files(cwd)), ALL], unsure: ALL,
        question: `Which one file does this step search in? Pick "${ALL}" when it names none.` },
    ],
    outcomes: ["found", "not_found"],
    messages: { found: "Found:\n{tail}", not_found: "No matches." },
    run: async (cwd, a) => search(cwd, a.pattern, !a.path || a.path === ALL ? undefined : a.path) },
  // A patch for a file that already exists (an exact old_string that must occur once), the whole contents for a new file or a full replacement.
  { name: "edit", shows: "what changed, as a short -/+ rendering of the old and new text; the file is changed on disk", description: "Change part of an existing file: add, remove, rename, modify or fix code or text in it (a route, a function, a line, a config value, a bug). Not for a new file or a full replacement: write does that. Not for deleting, renaming or moving a whole file: bash does that", brief: "Change part of an existing file",
    params: [{ ...pathParam, question: "Which existing file does this step change?" },
             { name: "old_string", description: "the exact text to replace, copied from the file as it is now, with enough surrounding lines that it occurs exactly once", kind: "free", literal: false },
             { name: "new_string", description: "the text that takes its place; may be empty to remove old_string", kind: "free", literal: false }],
    outcomes: ["edited", "refused"],
    messages: { edited: "Edited the file.", refused: "Could not edit the file: {detail}" },
    run: async (cwd, a, ctx) => {
      const oldString = a.old_string ?? "", newString = a.new_string ?? "";
      if (!oldString) throw new Error("old_string must not be empty: use write to create or replace a file");
      if (oldString === newString) throw new Error("old_string and new_string are the same: nothing to change");
      // sound() checks the new whole-file text; the pod's own edit does the exactly-once match and reports the uniqueness error.
      const before = await tryRemote(cwd, "read", { path: a.path });
      if (typeof before === "string") throw new Error(`${a.path} does not exist: use write to create it`);
      const preview = ((before as { content: string }).content).replace(oldString, newString);
      await sound(a.path, preview, ctx);
      const r = await tryRemote(cwd, "edit", { files: [{ path: a.path, edits: [{ old: oldString, new: newString }] }] });
      if (typeof r === "string") throw new Error(r.replace(/^error: /, ""));
      invalidateCache(cwd);
      return `edited ${a.path}\n${cut(oldString).split("\n").map((l) => `-${l}`).join("\n")}\n${cut(newString).split("\n").map((l) => `+${l}`).join("\n")}`;
    } },
  // Whole-file only: a new file, or a full replacement of an existing one. To change part of an existing file, edit does that.
  { name: "write", shows: "what was written and where; the file is changed on disk", description: "Create a new file, or replace the whole contents of an existing one. To change part of an existing file use edit. Not for deleting, renaming or moving a whole file, or for discarding or undoing uncommitted changes: bash does that", brief: "Create a file, or replace its whole contents",
    params: [pathParam,
             { name: "content", description: "the complete contents of the file", kind: "free", literal: false }],
    outcomes: ["written", "refused"],
    messages: { written: "Wrote the file.", refused: "Could not write the file: {detail}" },
    run: async (cwd, a, ctx) => {
      const before = await tryRemote(cwd, "read", { path: a.path });
      const old = typeof before === "string" ? undefined : (before as { content: string }).content;
      // Overwriting an existing file is the one risky case: a fragment sent as the whole file wipes the rest.
      // Seen live: a 159-char "export function remove…" replaced all of lib/store.js, and every later edit failed.
      if (old !== undefined) {
        const enrich = ctx?.generate && ctx.envelope ? async () => {
          const said = await ctx.generate!(`The old file is:\n${cut(old)}\n\nThe new content sent is:\n${cut(a.content)}\n\nIn one sentence, state what the new content is relative to the old file: the complete file, or a fragment or patch of it?`,
            child(ctx.envelope!, `is the new content of ${a.path} complete?`, {}, ["answered"]));
          return said || undefined;
        } : undefined;
        const user = ctx?.user ? async () => ({ type: "noul" as const, noul: (await ctx.user!.ask(`The new content for ${a.path} may be a fragment, not the complete file. Is it the complete new file? (yes/no)`)).trim().toLowerCase().startsWith("y") ? 1 : 0 }) : undefined;
        const { answer } = await ladder(ctx?.ask ?? (async () => ({})), { path: a.path, old: cut(old), new: cut(a.content) }, "complete",
          noul("Is the new content the complete new file, not a fragment or a patch of it?"), enrich, user);
        if (!(answer.type === "noul" && answer.noul >= DECIDE)) throw new Error("content is not the complete file: use edit to change part of it, or send the complete file");
      }
      await sound(a.path, a.content, ctx);
      const r = await tryRemote(cwd, "write", { path: a.path, content: a.content });
      if (typeof r === "string") throw new Error(r.replace(/^error: /, ""));
      invalidateCache(cwd);
      // The written text is not shown back. Probed live: with it in the output the step judge went from "next" 0.69 to unsure (0.11 to 0.14) on
      // a good write, and every good write would have gone to the user. Junk is stopped before the write instead: plainValue.
      return `wrote ${a.path}`;
    } },
  // The only tool that calls the LLM from inside a step. A task wires ctx.think with a per-task budget; a direct main call has none.
  { name: "think", shows: "the decision that was taken, to be used by the steps that follow", description: "Take one decision the task needs before it can go on: a design choice, a schema, an approach that depends on what earlier steps found. Never for anything a file read, a search, a command or another tool can answer", brief: "Take one decision the task needs before going on",
    params: [{ name: "question", description: "the one decision to make, with the options if known", kind: "free" }],
    outcomes: ["decided", "blocked"],
    run: async (_cwd, a, ctx) => {
      if (!ctx?.think) throw new Error("blocked: think is not available here");
      ctx.trace?.(`think: ${a.question.slice(0, 200)}`);
      return ctx.think(a.question);
    } },
  { name: "recall", shows: "the full messages of that closed section, oldest first", description: "Get back the full messages of an earlier, closed topic (a section) by its label, when its summary is not enough", brief: "Get back the full messages of an earlier closed section",
    params: [{ name: "section", description: "closed section id and label", kind: "closed", candidates: sectionLabels }],
    outcomes: ["shown", "not_found"],
    messages: { shown: "{tail}", not_found: "No such section." },
    // Task 6 gives recall its own bench tool, answered from the session log.
    run: async () => "error: recall is answered from the session log" },
  // With no id (or an id nothing tracks), this lists every tracked process instead: the one place to check what a bash step backgrounded.
  { name: "bash_output", shows: "the latest output lines of that background process, or, with no id given, one line per tracked process: id, pid, state, command", description: "Show the log output a background process (a started server or watcher) has printed so far, or, with no id, list every tracked background process", brief: "Show a backgrounded command's output, or list them all",
    params: [{ name: "proc", description: "background process id", kind: "closed", candidates: procIds, requiredUnless: () => false }],
    outcomes: ["shown", "listed", "none", "not_found"],
    messages: { shown: "{tail}", listed: "Processes:\n{tail}", none: "No background processes.", not_found: "No such process." },
    run: async (cwd, a) => text(await tryRemote(cwd, "process_output", { id: a.proc || undefined, since: 0 })) },
  { name: "kill_shell", shows: "which background process was stopped; it no longer runs", description: "Stop a background process (a started server or watcher)", brief: "Stop a background process",
    params: [{ name: "proc", description: "background process id", kind: "closed", candidates: procIds, question: "Which background process does this step stop?" }],
    outcomes: ["stopped", "not_found"],
    messages: { stopped: "{tail}.", not_found: "No such process." },
    run: async (cwd, a) => text(await tryRemote(cwd, "process_kill", { id: a.proc })) },
];

const ind = (ctx?: RunCtx) => "  ".repeat(ctx?.envelope?.depth ?? 0);

// Fills a tool's still-missing params via a one-shot child LLM turn, when ctx.generate supports it.
// The file a tool is about to change, for the params writer.
async function fileFor(cwd: string, args: Record<string, string>): Promise<string> {
  if (!args.path) return "";
  const r = await tryRemote(cwd, "read", { path: args.path });
  const body = typeof r === "string" ? "" : (r as { content: string }).content;
  return body && body.length <= FILE_IN_PROMPT ? `Current contents of ${args.path}:\n${body}\n\n` : "";
}

// An LLM's value for a named field is not always the bare string asked for. Seen live: the whole params object, as a JSON string, inside the
// one param asked for, and that JSON was written into a file. Every such shape is put right here, for params, plans, messages and summaries alike.
export function plainValue(v: unknown, name: string): string | undefined {
  if (typeof v === "string" && v.trimStart().startsWith("{")) { try { const inner = JSON.parse(v); if (typeof inner?.[name] === "string") v = inner[name]; } catch { /* a real value that starts with a brace */ } }
  if (v && typeof v === "object" && typeof (v as Record<string, unknown>)[name] === "string") v = (v as Record<string, string>)[name]; // the object itself, not stringified
  if (typeof v === "number" || typeof v === "boolean") v = String(v);
  if (v !== undefined && v !== null && typeof v !== "string") throw new Error(`blocked: ${name} came back as ${Array.isArray(v) ? "a list" : "an object"}, not text`);
  // A value wholly inside one markdown fence is the fence's body: the fence would land in the file, the shell or the message.
  const fenced = typeof v === "string" && /^\s*```[^\n]*\n([\s\S]*?)\n?```\s*$/.exec(v);
  if (fenced && !fenced[1].includes("```")) v = fenced[1] + (name === "content" ? "\n" : "");
  return (v ?? undefined) as string | undefined;
}

const FILE_IN_PROMPT = 6000; // chars; a file this small goes to the params writer whole, so it needs no lookup calls and cannot misquote it

async function buildParams(cwd: string, tool: Tool, args: Record<string, string>, ctx?: RunCtx, note = ""): Promise<Record<string, string>> {
  const missing = tool.params.filter((p) => !(p.name in args) && (!p.requiredUnless || p.requiredUnless(args)));
  if (missing.length === 0) return args;
  const names = missing.map((p) => p.name);
  const parent = ctx?.envelope;
  // A step with an explicit command is that command, with no LLM call and no guessing from prose. Seen live, before this field existed:
  // "run the tests with `.venv/bin/pytest -q`" came back as "cd tests && .venv/bin/pytest -q", exit 127, twice.
  if (names.includes("shell") && !note && parent?.command) { ctx?.trace?.(`${ind(ctx)}[command] shell: ${parent.command}`); return { ...args, shell: parent.command, ...(names.includes("command") ? { command: "other" } : {}) }; }
  // With no params writer, a param that has a default takes it; anything else blocks.
  if (!ctx?.ask && !ctx?.generate && missing.every((p) => p.unsure)) return { ...args, ...Object.fromEntries(missing.map((p) => [p.name, p.unsure!])) };
  if (!ctx?.generate || !parent) throw new Error(`blocked: missing ${names.join(", ")}`);
  // A closed param whose list has no "other" is one of the list: left unsettled by Jev and the user, it blocks; the writer would only make one up.
  for (const p of missing) if (p.kind === "closed" && typeof p.candidates === "function" && p.name !== "path" && p.name !== "to") {
    const c = await p.candidates(cwd).catch(() => [] as string[]);
    if (!c.includes("other")) throw new Error(`blocked: could not tell which ${p.description} is meant (there ${c.length === 1 ? "is" : "are"}: ${c.slice(0, 6).join("; ") || "none"})`);
  }
  const params = Object.fromEntries(missing.map((p) => [p.name, p.description]));
  const prompt = `Tool: ${tool.name} (${tool.description})\nMissing params:\n${JSON.stringify(params)}\nKnown args: ${JSON.stringify(args)}\nInstruction: ${parent.instruction}\nContext:\n${JSON.stringify(parent.context)}${["bash", "write", "edit", "read"].includes(tool.name) ? `\n\nProject files:\n${(await files(cwd).catch(() => [])).slice(0, PROMPT_FILES).join("\n")}` : ""}\n\n${ctx.detail ? `Detail from the planner, who read the code (use it; look up only what it lacks):\n${ctx.detail}\n\n` : ""}${await fileFor(cwd, args)}${note}` +
    `Call submit_params with the missing params; write no text.`;
  const childEnvelope: Envelope = { ...child(parent, `build params ${names.join(", ")} for ${tool.name}: ${parent.instruction}`, params, ["params", "blocked"]), generate: names, generateTool: tool.name };
  const text = await ctx.generate(prompt, childEnvelope);
  // LLMs wrap the object in prose, fences or a "params:" prefix, and may quote code with braces first: take the first "{" from which the text up to the last "}" parses.
  const end = text.lastIndexOf("}") + 1;
  let built: Record<string, string> | undefined;
  for (let i = text.indexOf("{"); i !== -1 && i < end && !built; i = text.indexOf("{", i + 1)) {
    try { built = JSON.parse(text.slice(i, end)); } catch { /* not the object's start */ }
  }
  if (!built) throw new Error(`blocked: missing ${names.join(", ")}`);
  for (const n of names) { const v = plainValue((built as Record<string, unknown>)[n], n); if (v !== undefined) built[n] = v; }
  // A defaulted param the writer left out takes its default.
  const merged = { ...args, ...built };
  // A param may stop being required once the others are known (read's query, once path names a file).
  const stillMissing = missing.filter((p) => !(p.name in merged) && (!p.requiredUnless || p.requiredUnless(merged)));
  if (stillMissing.length > 0) throw new Error(`blocked: missing ${stillMissing.map((p) => p.name).join(", ")}`);
  const cut = Object.fromEntries(Object.entries(built).map(([k, v]) => [k, v.slice(0, 120)]));
  ctx.trace?.(`${ind(ctx)}[generated] params: ${JSON.stringify(cut)}`);
  return merged;
}

// Candidates are keyed c0..cN so file paths never have to be valid option ids.
async function fillClosed(cwd: string, tool: Tool, args: Record<string, string>, ctx?: RunCtx): Promise<Record<string, string>> {
  const missing = tool.params.filter((p) => p.kind === "closed" && !(p.name in args) && (!p.requiredUnless || p.requiredUnless(args)));
  if (missing.length === 0 || !ctx?.ask || !ctx.envelope) return {};
  const lists = new Map<string, string[]>();
  const picked: Record<string, string> = {};
  const questions: Record<string, ReturnType<typeof choice>> = {};
  for (const p of missing) {
    const spec = p.candidates!;
    let c: string[];
    let source: string;
    if (typeof spec === "string") {
      source = spec;
      const named = TOOLS.find((t) => t.name === spec);
      if (!named) continue;
      const childEnvelope = child(ctx.envelope, `candidates for ${p.name} of ${tool.name}`, { tool: tool.name, param: p.name }, named.outcomes);
      const out = await runTool(cwd, named, Object.fromEntries(named.params.filter((q) => q.unsure).map((q) => [q.name, q.unsure!])), { ...ctx, envelope: childEnvelope, approve: undefined }); // candidates are the whole list: no vote on the lister's own params
      c = out.split("\n").map((l) => l.trim()).filter(Boolean);
    } else {
      source = "function";
      c = await spec(cwd, ctx);
    }
    if (c.length === 0 || c.length > MAX_CANDIDATES) continue;
    ctx.trace?.(`${ind(ctx)}candidates ${p.name} <- ${source}: ${c.length} options`);
    // A candidate spelled out in the instruction needs no vote; the longest wins ("a/index.js" over "index.js").
    // Two files named ("move x from a.js to b.js") is a question of which, so it goes to the vote.
    // An option that leads with an id ("p1 npm run dev", "S2 label") is named by its id alone, and the id is the value.
    const said = (v: string) => { const id = idOf(v); return id !== v ? new RegExp(`\\b${id}\\b`).test(ctx.envelope!.instruction) : v.length > 3 && ctx.envelope!.instruction.includes(v); };
    const hits = c.filter(said).sort((x, y) => y.length - x.length);
    const named = hits.length > 0 && hits.every((h) => hits[0].includes(h)) ? idOf(hits[0]) : undefined;
    // A write may create its file, and a new file is never a candidate. Seen live: "convert a/todos.js to TypeScript in a/todos.ts" named only
    // the existing todos.js, so the TypeScript was written over it. One unlisted file path in the instruction is where the write goes; more
    // than one, or any unlisted path alongside a listed one for another tool, is a question of which and goes to the vote.
    // Seen live: "read from process.env.AUTH_TOKEN" was taken for a new file of that name and the diff was written into it.
    // ponytail: a path has a slash, a leading dot, or a short lowercase extension; "res.json" still slips through, the vote is the upgrade.
    const pathLike = (t: string) => t.includes("/") || t.startsWith(".") || /\.[a-z][a-z0-9]{0,4}$/.test(t);
    const unlisted = tool.name === "write" && p.name === "path" ? (ctx.envelope!.instruction.match(/[\w.\/-]*[\w-]+\.[a-zA-Z]\w*/g) ?? []).map((t) => t.replace(/^(\.\/)+/, "")).filter((t) => !c.some((v) => v === t || v.endsWith(`/${t}`))).filter(pathLike) : [];
    // An unlisted path is only a guess from the wording, so it never decides: it joins the candidates and Jev votes (a label saying it is new scored lower than the bare path).
    // Seen live: "./lib/helpers.js" was taken for a new file next to the listed "lib/helpers.js", and the diff was written into it.
    // Paths the instruction names are the whole question when there are several: the vote is put to those alone. Probed: "convert a/login.js to a/login.ts"
    // picked login.ts at 0.85 to 0.90 among 48 files (under ACT, so the user was asked) and at 0.98 between the two named.
    const namedAll = [...hits, ...new Set(unlisted)];
    // The narrowing needs a listed file among them: unlisted tokens alone may be no paths at all ("replace res.json with res.send"), and then the real files stay on the ballot.
    c = namedAll.length >= 2 && hits.length > 0 ? namedAll : [...c, ...new Set(unlisted)];
    if (named && unlisted.length === 0) { picked[p.name] = named; ctx.trace?.(`${ind(ctx)}[named] ${p.name} = ${named}`); continue; }
    lists.set(p.name, c);
    questions[p.name] = choice(p.question ?? `Which ${p.description} does the instruction refer to?`, {
      ...Object.fromEntries(c.map((v, i) => [`c${i}`, v])),
      // "other" and "all files" already mean none of these; a second way out splits the vote and drops a clear pick below ACT.
      ...(c.includes("other") || c.includes(ALL) ? {} : { none: "none of these" }),
    });
  }
  if (lists.size === 0) return picked;
  const instruction = ctx.envelope.instruction;
  let answers: Record<string, ChoiceAnswer>;
  try {
    answers = (await ctx.ask({ instruction, tool: tool.name, chain: chain(ctx.envelope) }, questions)) as Record<string, ChoiceAnswer>;
  } catch { return picked; }
  const built: Record<string, string> = picked;
  for (const [name, c] of lists) {
    const a = answers[name];
    if (!a) continue;
    const v = c[Number(a.choice.slice(1))] && idOf(c[Number(a.choice.slice(1))]);
    const conf = ` (${a.confidence.toFixed(2)})`;
    if (a.choice !== "none" && a.confidence >= ACT && v !== undefined) { built[name] = v; ctx.trace?.(`${ind(ctx)}[picked] ${name} = ${v}${conf}`); }
    else {
      ctx.trace?.(`${ind(ctx)}[picked] ${name}: none${conf}`);
      const p = missing.find((m) => m.name === name)!;
      // An existing target is never guessed by the params writer. A lookup falls to its search; a change with close rivals asks the user, who is
      // shown the rivals. With no rivals the target is likely a new file, and that path is the writer's to name.
      const rivals = Object.entries(a.probabilities ?? {}).filter(([k, pr]) => k !== "none" && pr >= RIVAL).sort((x, y) => y[1] - x[1]).slice(0, 3).map(([k]) => c[Number(k.slice(1))]).filter(Boolean);
      // A candidate of the same name in another folder is a rival whatever its share: the vote cannot tell `start` from `app/start` by the words alone.
      const leaf = (t: string) => t.split(": ")[0].split("/").pop();
      for (const o of c) if (rivals.length && rivals.length < 6 && !rivals.includes(o) && rivals.some((r) => r !== "other" && leaf(r) === leaf(o))) rivals.push(o);
      // Only a path can be something the list lacks (a new file). Any other closed param is one of the list or nothing: the writer never names it.
      // Seen live: one tracked process, Jev unsure, no rivals, and the writer made up "p2".
      const isPath = name === "path" || name === "to";
      if (!isPath && rivals.length < 2) { const byShare = c.map((o, i) => [o, a.probabilities?.[`c${i}`] ?? 0] as const).filter(([o]) => o !== "other").sort((x, y) => y[1] - x[1]).slice(0, 3).map(([o]) => o); rivals.splice(0, rivals.length, ...byShare); }
      if (p.unsure) { built[name] = p.unsure; ctx.trace?.(`${ind(ctx)}[unsure] ${name} = ${p.unsure}`); }
      else if (rivals.length > 1 || (!isPath && rivals.length === 1)) { // any closed param: a running process guessed wrong does as much harm as a file
        // Low confidence is worked on before the user hears of it: Jev votes again between the rivals alone (a short list reads far sharper than the
        // whole project). Failing that the params writer, which can look files up, writes what each rival is, and Jev votes once more with that.
        // The writer never chooses. Seen live: asked to choose, it took the root start script over the app's and started the wrong program twice.
        ctx.trace?.(`${ind(ctx)}rivals ${name}: ${rivals.join(", ")}`);
        const question = { [name]: choice(p.question ?? `Which ${p.description} does the instruction refer to?`, Object.fromEntries(rivals.map((r, i) => [`c${i}`, r]))) };
        const vote = async (context?: string) => {
          // One option is no choice: Jev is asked yes or no about it.
          if (rivals.length === 1) { const y = (await ctx.ask!({ instruction, tool: tool.name, chain: chain(ctx.envelope!), ...(context ? { context } : {}) }, { [name]: noul(`Is "${rivals[0]}" the ${p.description} the instruction refers to?`) }).catch(() => ({})) as Record<string, NoulAnswer>)[name]; return y && y.noul >= ACT ? rivals[0] : undefined; }
          const v = (await ctx.ask!({ instruction, tool: tool.name, chain: chain(ctx.envelope!), ...(context ? { context } : {}) }, question).catch(() => ({})) as Record<string, ChoiceAnswer>)[name];
          return v && v.confidence >= ACT ? rivals[Number(v.choice.slice(1))] : undefined;
        };
        let one = await vote();
        if (!one && ctx.generate) {
          const said = await ctx.generate(`Tool: ${tool.name}\nInstruction: ${instruction}\nContext:\n${JSON.stringify(ctx.envelope.context)}\n\n${p.question ?? `Which ${p.description}?`} It is one of:\n${rivals.join("\n")}\nDo not choose. Look the files up, then call submit_params with {"${name}": "<for each of those, one line on what it is and does, from what you read>"}; write no other text.`,
            { ...child(ctx.envelope, `describe the ${name} options for ${tool.name}: ${instruction}`, { [name]: p.description }, ["params", "blocked"]), generate: [name], generateTool: tool.name }).catch(() => "");
          let facts = said; try { facts = String(JSON.parse(said)[name] ?? said); } catch { /* not JSON: the text itself is the facts */ }
          if (facts) one = await vote(facts.slice(0, 2000));
        }
        if (one) { built[name] = idOf(one); ctx.trace?.(`${ind(ctx)}[picked] ${name} = ${built[name]} (between rivals)`); continue; }
        if (!ctx.user) continue;
        const reply = (await ctx.user.ask(`"${instruction.slice(0, 120)}": which ${isPath ? "file" : p.description}?\n${rivals.map((r, i) => `${i + 1}. ${r}`).join("\n")}\nAnswer with a number${isPath ? ", or name another path" : ""}.`)).trim();
        const told = rivals[Number(reply) - 1] ?? (isPath ? reply : undefined); // only a path may be one the list lacks
        if (told) built[name] = told;
      }
    }
  }
  return built;
}

const idOf = (v: string) => v.match(/^([a-zA-Z]\d+) /)?.[1] ?? v;

export async function runTool(cwd: string, tool: Tool, args: Record<string, string>, ctx?: RunCtx): Promise<string> {
  let out: string;
  try {
    for (const [k, v] of Object.entries(args)) ctx?.trace?.(`${ind(ctx)}[given] ${k} = ${v.slice(0, 120)}`);
    args = { ...args, ...(await fillClosed(cwd, tool, args, ctx)) };
    const generated = tool.params.some((p) => p.kind === "free" && !(p.name in args));
    let built = await buildParams(cwd, tool, args, ctx);
    // The range is optional, so no one is asked to fill it: a step that names one ("lines 32-53") is read from the instruction.
    // It is filled here, before the seen check, so a range of a file already seen as an outline is a new lookup.
    const range = tool.name === "read" && !built.lines && /\blines? +(\d+ *(?:-|to) *\d+)/i.exec(ctx?.envelope?.instruction ?? "")?.[1];
    if (range) built = { ...built, lines: range };
    // The same lookup twice in one task shows nothing new; a change in between makes it new again.
    if (ctx?.seen) {
      const key = `${tool.name} ${JSON.stringify(built)}`;
      if (tool.name !== "read") ctx.seen.clear();
      else if (ctx.seen.has(key)) return `blocked: ${key} was already read in this task and has not changed; use that result, or read something else`;
      else ctx.seen.add(key);
    }
    ctx?.onArgs?.(built);
    // A confirm raised only because the tool always asks is skipped for args the tool calls safe (a package.json script).
    if (ctx?.approve && !(ctx.soft && tool.safe?.(built))) {
      const ok = await ctx.approve(built);
      ctx.trace?.(`${ind(ctx)}approve ${tool.name} ${JSON.stringify(built).slice(0, 120)}: ${ok ? "yes" : "no"}`);
      if (!ok) return "denied: not approved";
    }
    ctx?.trace?.(`${ind(ctx)}ran ${tool.name} ${JSON.stringify(built).slice(0, 120)}`);
    try { out = await tool.run(cwd, built, ctx); } catch (e) {
      // Params the LLM wrote did not fit (an old_string that does not match the file): one rewrite with the error in view, no planner, no user.
      if (!generated || !ctx?.generate) throw e;
      ctx.trace?.(`${ind(ctx)}retry params: ${(e as Error).message.slice(0, 120)}`);
      built = await buildParams(cwd, tool, args, ctx, `Your last attempt failed: ${(e as Error).message}\nLast attempt: ${JSON.stringify(built).slice(0, 1500)}\n\n`);
      out = await tool.run(cwd, built, ctx);
    }
  } catch (e) { out = `error: ${(e as Error).message}`; }
  if (ctx?.trace) {
    const lines = out.split("\n");
    const shown = lines.slice(0, 3).map((l) => l.slice(0, 160));
    const extra = lines.length > 3 ? ` (+${lines.length - 3} lines)` : "";
    ctx.trace(`${ind(ctx)}out: ${shown.join(" / ")}${extra}`);
  }
  return out;
}
