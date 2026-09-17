import type { Message } from "../../model";

type Action = Extract<Message, { role: "action" }>;

/**
 * Our tools, mapped onto opencode's part catalogue (the render contract, §0–§1). The pane renders
 * exactly as `packages/session-ui` does; what we have to supply is which of ITS parts each of OUR
 * pi events is. That mapping is data, not scattered conditionals, so it can be read and tested in
 * one place.
 *
 * `opencode-ref` is the source of record: `message-part.tsx:1489` keys tool bodies by name, with
 * `apply_patch → patch` and `bash → shell`, and anything unregistered falling back to GenericTool.
 */
export type PartKind = "tool" | "text" | "reasoning" | "compaction";
/** The tool bodies opencode registers; everything else renders as `generic`. */
export type ToolKind =
  | "read" | "list" | "glob" | "grep" | "webfetch" | "websearch" | "task"
  | "shell" | "edit" | "write" | "patch" | "todowrite" | "question" | "skill" | "generic";

/** Ours → theirs. A name they register keeps its body; everything else is a generic row. */
const TOOLS: Record<string, ToolKind> = {
  read: "read",
  ls: "list",
  find: "glob",
  grep: "grep",
  bash: "shell",
  process: "shell",
  write: "write",
  edit: "edit",
  ask: "task",
  question: "question",
  skill: "skill",
  plan: "todowrite",
  kl_repo_clone: "shell",
  kl_container_build: "shell",
  kl_container_push: "shell",
  kl_images: "shell",
};

export const toolKind = (tool: string | undefined): ToolKind => (tool ? (TOOLS[tool] ?? "generic") : "generic");

/**
 * The four tools opencode folds into one "Exploring / Explored" group
 * (`message-part.tsx:607 CONTEXT_GROUP_TOOLS`). Consecutive parts of these become one group.
 */
export const CONTEXT_TOOLS = new Set<ToolKind>(["read", "glob", "grep", "list"]);
export const isContext = (tool: string | undefined) => CONTEXT_TOOLS.has(toolKind(tool));

/** `contextToolSummary` (:899): reads, searches (glob + grep), lists. */
export function contextCounts(rows: Action[]): { read: number; search: number; list: number } {
  const counts = { read: 0, search: 0, list: 0 };
  for (const r of rows) {
    const k = toolKind(r.tool);
    if (k === "read") counts.read++;
    else if (k === "glob" || k === "grep") counts.search++;
    else if (k === "list") counts.list++;
  }
  return counts;
}

/** `{{count}} read/reads`, `search/searches`, `list/lists` (`ui/src/i18n/en.ts:105-110`), in order. */
export function contextSummary(counts: { read: number; search: number; list: number }): string[] {
  const n = (v: number, one: string, many: string) => (v ? `${v} ${v === 1 ? one : many}` : "");
  return [n(counts.read, "read", "reads"), n(counts.search, "search", "searches"), n(counts.list, "list", "lists")].filter(Boolean);
}

/** The last path segment, as `getFilename`/`getDirectory` do for the subtitle. */
const tail = (p: string) => String(p).replace(/\/+$/, "").split("/").filter(Boolean).pop() ?? String(p);

/**
 * One row's trigger: title, subtitle, and at most three `k=v` args — `basic-tool.tsx:196` for the
 * grammar, `:304` for which keys become the label and which become args.
 */
export type Trigger = { title: string; subtitle?: string; args: string[]; icon: string };

const LABEL_KEYS = ["description", "query", "url", "filePath", "path", "pattern", "name"];
/** `args(input)` (:309): every other primitive, `key=value`, at most three. */
export function genericArgs(input: Record<string, unknown> = {}): string[] {
  return Object.entries(input)
    .filter(([k, v]) => !LABEL_KEYS.includes(k) && (typeof v === "string" || typeof v === "number" || typeof v === "boolean"))
    .map(([k, v]) => `${k}=${v}`)
    .slice(0, 3);
}
/** `label(input)` (:304): the first of the label keys that is a non-empty string. */
export const genericLabel = (input: Record<string, unknown> = {}) =>
  LABEL_KEYS.map((k) => input[k]).find((v) => typeof v === "string" && v.length > 0) as string | undefined;

const ICONS: Record<ToolKind, string> = {
  read: "file", list: "folder", glob: "search", grep: "search", webfetch: "globe", websearch: "globe",
  task: "robot", shell: "terminal", edit: "pencil", write: "file", patch: "pencil",
  todowrite: "list", question: "help", skill: "book", generic: "tool",
};

/** What the row says, per tool — the contract's own per-tool title/subtitle/args table. */
export function trigger(tool: string | undefined, input: Record<string, any> = {}): Trigger {
  const kind = toolKind(tool);
  const icon = ICONS[kind];
  switch (kind) {
    case "read":
      return { title: "Read", subtitle: input.path ? tail(input.path) : undefined, args: genericArgs({ offset: input.offset, limit: input.limit }), icon };
    case "list":
      return { title: "List", subtitle: input.path ? tail(input.path) : undefined, args: [], icon };
    case "glob":
      return { title: "Glob", subtitle: input.path ? tail(input.path) : undefined, args: input.pattern ? [`pattern=${input.pattern}`] : [], icon };
    case "grep":
      return { title: "Grep", subtitle: input.path ? tail(input.path) : undefined, args: [input.pattern ? `pattern=${input.pattern}` : "", input.glob ? `include=${input.glob}` : ""].filter(Boolean), icon };
    case "shell":
      return { title: "Shell", subtitle: String(input.command ?? input.action ?? ""), args: [], icon };
    case "edit":
      return { title: "Edit", subtitle: input.path ? tail(input.path) : undefined, args: [], icon };
    case "write":
      return { title: "Write", subtitle: input.path ? tail(input.path) : undefined, args: [], icon };
    case "task":
      return { title: input.to === "agent" ? "Agent" : "Ask", subtitle: String(input.name ?? input.to ?? ""), args: [], icon };
    case "question":
      return { title: "Question", subtitle: String(input.header ?? ""), args: [], icon };
    case "skill":
      return { title: "Skill", subtitle: String(input.name ?? ""), args: [], icon };
    case "todowrite":
      return { title: "Plan", subtitle: input.done ? `done: ${input.done}` : input.doing ? `doing: ${input.doing}` : `${(input.set as unknown[] | undefined)?.length ?? 0} steps`, args: [], icon };
    default:
      // `Called \`{{tool}}\`` (`ui/src/i18n/en.ts:170`), with the generic label and args.
      return { title: `Called \`${tool ?? ""}\``, subtitle: genericLabel(input), args: genericArgs(input), icon };
  }
}

/**
 * Default-open policy (`part-default-open.ts:19`): a shell row opens when shells are opened, an
 * edit/write/patch when edits are, and a pure-deletion diff stays collapsed. Everything else is
 * closed. We pass the two switches rather than reading a settings store.
 */
export function defaultOpen(tool: string | undefined, shell = false, edit = false, deletionOnly = false): boolean {
  const k = toolKind(tool);
  if (k === "shell") return shell;
  if (k === "edit" || k === "write" || k === "patch") return edit && !deletionOnly;
  return false;
}
