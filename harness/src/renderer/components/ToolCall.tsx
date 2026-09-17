import { For, Show, createMemo, createSignal, onCleanup, type JSX } from "solid-js";
import { Icon } from "../ui/Icon";
import { highlight, languageOf } from "../syntax";
import type { Message } from "../model";
import { ResultCard, pickRenderer } from "./results";

type Action = Extract<Message, { role: "action" }>;

/**
 * A tool call, drawn for what it is rather than as one shape for all: a
 * command with its terminal output, a read with the file, a write with what
 * was written, an edit as a diff, a search with its hits, a platform call
 * with its answer laid out. Every one has the same head — a state dot, the
 * verb, the subject, how long it took, a chevron — and folds to that head.
 * A failed call shows only its error; nobody wants the whole response then.
 */
export function ToolCall(props: { a: Action }) {
  const a = () => props.a;
  const [open, setOpen] = createSignal(true);
  const html = () => /^\s*<!doctype html|^\s*<html/i.test(a().output ?? "");
  const failed = () => a().ok === false || html();
  // While it runs: a spinner in the dot's place and a clock counting up, so
  // a slow command is visibly alive rather than merely unfinished.
  const [tick, setTick] = createSignal(Date.now());
  const timer = setInterval(() => a().pending && setTick(Date.now()), 500);
  onCleanup(() => clearInterval(timer));
  const fmt = (ms: number) => (ms < 1000 ? `${Math.max(0, Math.round(ms))}ms` : `${(ms / 1000).toFixed(1)}s`);
  const took = () => (a().pending ? fmt(tick() - (a().ts ?? tick())) : a().ms !== undefined ? fmt(a().ms!) : "");
  const head = createMemo(() => describe(a()));

  return (
    <div class="flex flex-col font-mono text-sm leading-[18px]">
      <button class="group flex h-6 w-full items-center gap-2 text-left" onClick={() => setOpen((v) => !v)}>
        <span class="flex w-5 shrink-0 items-center justify-center">
          <Show when={!a().pending} fallback={<Icon name="spinner" size={14} class="animate-spin text-accent" />}>
            <span class={`size-1.5 rounded-full ${failed() ? "bg-danger" : "bg-success"}`} />
          </Show>
        </span>
        <span class="shrink-0 font-bold text-fg-strong">{head().verb}</span>
        <span class="min-w-0 flex-1 truncate text-fg">{head().subject}</span>
        <Show when={head().meta}><span class="shrink-0 text-xs text-subtle">{head().meta}</span></Show>
        <span class="shrink-0 text-xs tabular-nums" classList={{ "text-accent": a().pending, "text-subtle": !a().pending }}>{took()}</span>
        <Icon name={open() ? "chevronDown" : "chevronRight"} size={16} class="shrink-0 text-subtle opacity-0 group-hover:opacity-100" />
      </button>
      <Show when={open()}>
        <div class="ml-5 border-l border-line pl-3">
          <Show when={failed()} fallback={<Body a={a()} />}>
            <Fail text={a().output ?? ""} />
          </Show>
        </div>
      </Show>
    </div>
  );
}

/** The head line: verb, subject and a fact, per tool. */
function describe(a: Action): { verb: string; subject: string; meta?: string } {
  const g = a.args ?? {};
  const s = (k: string) => (typeof g[k] === "string" ? (g[k] as string) : "");
  switch (a.tool) {
    case "bash": return { verb: "$", subject: s("command") };
    case "read": return { verb: "read", subject: s("path"), meta: g.offset ? `from line ${g.offset}` : lines(a.output) };
    case "write": return { verb: "write", subject: s("path"), meta: lines(s("content")) };
    case "edit": return { verb: "edit", subject: s("path"), meta: `${(g.edits as unknown[] | undefined)?.length ?? 1} ${(g.edits as unknown[] | undefined)?.length === 1 ? "change" : "changes"}` };
    case "grep": return { verb: "grep", subject: `${s("pattern")}${s("path") ? ` in ${s("path")}` : ""}`, meta: count(a.output, "match") };
    case "find": return { verb: "find", subject: `${s("pattern")}${s("path") ? ` in ${s("path")}` : ""}`, meta: count(a.output, "file") };
    case "ls": return { verb: "ls", subject: s("path") || ".", meta: count(a.output, "entry") };
    case "process": return { verb: "process", subject: `${s("action")} ${s("name") || s("command") || s("id")}`.trim() };
    default:
      if (a.tool?.startsWith("kl_")) return { verb: "kloudlite", subject: `${a.tool.slice(3).replace(/_/g, " ")}${short(g)}` };
      return { verb: a.target ?? a.tool ?? "", subject: a.text };
  }
}
const lines = (t?: string) => (t ? `${t.split("\n").length} lines` : "");
const count = (t: string | undefined, noun: string) => {
  if (!t || t.startsWith("No ")) return "no " + noun + "s";
  const n = t.split("\n").filter(Boolean).length;
  return `${n} ${noun}${n === 1 ? "" : "s"}`;
};
const short = (g: Record<string, unknown>) => {
  const parts = Object.entries(g).filter(([, v]) => v !== undefined && v !== null && typeof v !== "object").map(([k, v]) => `${k}=${String(v)}`);
  return parts.length ? ` · ${parts.join(" ")}` : "";
};

/** The failure, and only the failure: the message the tool gave, in red. */
function Fail(props: { text: string }) {
  const msg = () => {
    const t = props.text.trim();
    // A platform answer is `status: body`; the body may be JSON with a message.
    const m = /^(\d{3}): ([\s\S]*)$/.exec(t);
    if (/^\s*<!doctype html|^\s*<html/i.test(t)) {
      const to = /url=([^"'&]+)/.exec(t)?.[1];
      return `not an api answer — got a web page${to ? ` (redirects to ${decodeURIComponent(to)})` : ""}`;
    }
    if (m) {
      try {
        const j = JSON.parse(m[2]) as { error?: string; message?: string };
        return `${m[1]} · ${j.message ?? j.error ?? m[2]}`;
      } catch {
        return `${m[1]} · ${m[2].replace(/<[^>]+>/g, " ").replace(/\s+/g, " ").trim().slice(0, 200) || "error"}`;
      }
    }
    return t.split("\n").slice(-6).join("\n");
  };
  return <pre class="m-0 py-1 whitespace-pre-wrap wrap-words font-[inherit] text-danger">{msg() || "failed"}</pre>;
}

function Body(props: { a: Action }) {
  const a = () => props.a;
  const g = () => a().args ?? {};
  const card = createMemo(() => !!pickRenderer(a().tool, a().output, a().args));
  return (
    <Show when={!a().pending} fallback={<Show when={a().output}><Out text={a().output!} /></Show>}>
      <Switch tool={a().tool}>
        {{
          bash: () => <Out text={a().output ?? ""} empty="(no output)" />,
          read: () => <Code path={String(g().path ?? "")} text={a().output ?? ""} from={Number(g().offset ?? 1)} />,
          write: () => <Code path={String(g().path ?? "")} text={String(g().content ?? "")} from={1} />,
          edit: () => <Edits path={String(g().path ?? "")} edits={(g().edits as { oldText: string; newText: string }[]) ?? []} result={a().output} />,
          grep: () => <Hits text={a().output ?? ""} />,
          find: () => <Hits text={a().output ?? ""} />,
          ls: () => <Hits text={a().output ?? ""} />,
          // A card when one fits, the block it always had when none does: an api route this build
          // does not know about must still be readable (spec §8).
          process: () => <Show when={card()} fallback={<Out text={a().output ?? ""} />}><ResultCard tool={a().tool} output={a().output} args={a().args} /></Show>,
          other: () => (
            <Show when={card()} fallback={a().tool?.startsWith("kl_") ? <Answer text={a().output ?? ""} /> : <Out text={a().output ?? ""} empty="(no output)" />}>
              <ResultCard tool={a().tool} output={a().output} args={a().args} />
            </Show>
          ),
        }}
      </Switch>
    </Show>
  );
}

function Switch(props: { tool?: string; children: Record<string, () => JSX.Element> }) {
  const pick = () => props.children[props.tool ?? ""] ?? props.children.other;
  return <>{pick()()}</>;
}

/** Terminal output: the first lines, the rest a click away. */
function Out(props: { text: string; empty?: string; head?: number }) {
  const HEAD = props.head ?? 8;
  const [all, setAll] = createSignal(false);
  const CHARS = 1200;
  const ls = () => props.text.replace(/\s+$/, "").split("\n");
  const folded = () => {
    const head = ls().slice(0, HEAD).join("\n");
    return head.length > CHARS ? head.slice(0, CHARS) + "…" : head;
  };
  const more = () => Math.max(0, ls().length - HEAD) || (ls().slice(0, HEAD).join("\n").length > CHARS ? 1 : 0);
  return (
    <Show when={props.text.trim()} fallback={<div class="py-1 text-subtle">{props.empty ?? ""}</div>}>
      <pre class="m-0 py-1 whitespace-pre-wrap wrap-words font-[inherit] text-muted">{all() ? ls().join("\n") : folded()}</pre>
      <Show when={more()}>
        <button class="pb-1 text-xs text-subtle hover:text-fg" onClick={() => setAll((v) => !v)}>{all() ? "collapse" : ls().length > HEAD ? `… ${ls().length - HEAD} more lines` : "… show all"}</button>
      </Show>
    </Show>
  );
}

/** A file, or the part of it read: numbered, highlighted by its extension. */
function Code(props: { path: string; text: string; from: number }) {
  const HEAD = 12;
  const [all, setAll] = createSignal(false);
  const lang = () => languageOf(props.path);
  const ls = () => props.text.replace(/\s+$/, "").split("\n");
  const shown = () => (all() ? ls() : ls().slice(0, HEAD));
  return (
    <Show when={props.text.trim()} fallback={<div class="py-1 text-subtle">(empty)</div>}>
      <div class="my-1 overflow-x-auto rounded-[2px] bg-codeblock py-1">
        <For each={shown()}>
          {(l, i) => (
            <div class="flex">
              <span class="w-10 shrink-0 pr-3 text-right text-line-number select-none">{props.from + i()}</span>
              <span class="whitespace-pre" innerHTML={highlight(l, lang())} />
            </div>
          )}
        </For>
      </div>
      <Show when={ls().length > HEAD}>
        <button class="pb-1 text-xs text-subtle hover:text-fg" onClick={() => setAll((v) => !v)}>{all() ? "collapse" : `… ${ls().length - HEAD} more lines`}</button>
      </Show>
    </Show>
  );
}

/** An edit as the diff it is: what left in red, what came in green. */
function Edits(props: { path: string; edits: { oldText: string; newText: string }[]; result?: string }) {
  const lang = () => languageOf(props.path);
  return (
    <div class="my-1 flex flex-col gap-2">
      <For each={props.edits}>
        {(e) => (
          <div class="overflow-x-auto rounded-[2px] bg-codeblock py-1">
            <For each={e.oldText.replace(/\s+$/, "").split("\n")}>
              {(l) => <div class="flex bg-danger-wash"><span class="w-6 shrink-0 text-center text-deleted select-none">−</span><span class="whitespace-pre" innerHTML={highlight(l, lang())} /></div>}
            </For>
            <For each={e.newText.replace(/\s+$/, "").split("\n")}>
              {(l) => <div class="flex bg-success-wash"><span class="w-6 shrink-0 text-center text-created select-none">+</span><span class="whitespace-pre" innerHTML={highlight(l, lang())} /></div>}
            </For>
          </div>
        )}
      </For>
      <Show when={props.result && !/^Successfully/.test(props.result)}><div class="text-xs text-subtle">{props.result}</div></Show>
    </div>
  );
}

/** Search or listing results: one per line, the path part quiet. */
function Hits(props: { text: string }) {
  const HEAD = 12;
  const [all, setAll] = createSignal(false);
  const ls = () => props.text.replace(/\s+$/, "").split("\n").filter(Boolean);
  return (
    <Show when={ls().length} fallback={<div class="py-1 text-subtle">nothing</div>}>
      <div class="py-1">
        <For each={all() ? ls() : ls().slice(0, HEAD)}>
          {(l) => {
            const m = /^([^:\s]+):(\d+):(.*)$/.exec(l);
            return m ? (
              <div class="truncate"><span class="text-muted">{m[1]}</span><span class="text-subtle">:{m[2]}:</span><span class="text-fg">{m[3]}</span></div>
            ) : (
              <div class="truncate text-muted">{l}</div>
            );
          }}
        </For>
      </div>
      <Show when={ls().length > HEAD}>
        <button class="pb-1 text-xs text-subtle hover:text-fg" onClick={() => setAll((v) => !v)}>{all() ? "collapse" : `… ${ls().length - HEAD} more`}</button>
      </Show>
    </Show>
  );
}

/**
 * A platform answer. One record — an object, or a list of one — reads as
 * label/value rows, nested objects indented under their key; several read
 * as a table of the columns that matter. Anything else is text.
 */
function Answer(props: { text: string }) {
  const data = createMemo<unknown>(() => {
    try {
      return JSON.parse(props.text);
    } catch {
      return undefined;
    }
  });
  const isRow = (v: unknown): v is Record<string, unknown> => !!v && typeof v === "object" && !Array.isArray(v);
  const one = () => (isRow(data()) ? (data() as Record<string, unknown>) : Array.isArray(data()) && (data() as unknown[]).length === 1 && isRow((data() as unknown[])[0]) ? ((data() as unknown[])[0] as Record<string, unknown>) : undefined);
  const many = () => (Array.isArray(data()) && (data() as unknown[]).length > 1 && isRow((data() as unknown[])[0]) ? (data() as Record<string, unknown>[]) : undefined);
  return (
    <Show when={data() !== undefined} fallback={<Out text={props.text} />}>
      <Show when={one()}>{(o) => <Record value={o()} />}</Show>
      <Show when={many()}>{(rows) => <Table rows={rows()} />}</Show>
      <Show when={!one() && !many()}>
        <Show when={Array.isArray(data()) && !(data() as unknown[]).length} fallback={<Out text={Array.isArray(data()) ? (data() as unknown[]).map(String).join("\n") : String(data())} />}>
          <div class="py-1 text-subtle">none</div>
        </Show>
      </Show>
    </Show>
  );
}

const cell = (v: unknown) => (v === null || v === undefined || v === "" ? "—" : typeof v === "object" ? JSON.stringify(v) : String(v));

/**
 * One record, laid out to be read rather than dumped: a title line (name,
 * id, state), the plain fields in two columns, and every nested object as a
 * titled block below — a `{ready, reason, message}` status folds to one line.
 */
function Record(props: { value: Record<string, unknown> }) {
  const v = () => props.value;
  const str = (k: string) => (typeof v()[k] === "string" ? (v()[k] as string) : undefined);
  const HEAD = ["name", "id", "state"];
  const isObj = (x: unknown): x is Record<string, unknown> => !!x && typeof x === "object" && !Array.isArray(x);
  const isStatus = (x: unknown) => isObj(x) && "ready" in x && ("reason" in x || "message" in x);
  const plain = () => Object.entries(v()).filter(([k, x]) => !HEAD.includes(k) && x !== undefined && (!isObj(x) || isStatus(x)) && !(Array.isArray(x) && x.some(isObj)));
  const blocks = () => Object.entries(v()).filter(([k, x]) => !HEAD.includes(k) && ((isObj(x) && !isStatus(x) && Object.keys(x).length) || (Array.isArray(x) && x.some(isObj))));
  const STATE: Record<string, string> = { ready: "bg-success", running: "bg-success", stopped: "bg-subtle", failed: "bg-danger", error: "bg-danger", creating: "bg-warning", starting: "bg-warning", stopping: "bg-warning" };
  return (
    <div class="my-1 flex flex-col gap-2">
      <Show when={str("name") || str("id") || str("state")}>
        <div class="flex items-center gap-2">
          <Show when={str("state")}>{(st) => <span class={`size-1.5 rounded-full ${STATE[st()] ?? "bg-subtle"}`} title={st()} />}</Show>
          <Show when={str("name")}>{(n) => <span class="font-bold text-fg-strong">{n()}</span>}</Show>
          <Show when={str("id")}>{(id) => <span class="text-subtle">{id()}</span>}</Show>
          <Show when={str("state")}>{(st) => <span class="text-xs text-muted">{st()}</span>}</Show>
        </div>
      </Show>
      <div class="grid grid-cols-[repeat(auto-fit,minmax(260px,1fr))] gap-x-8 gap-y-0.5">
        <For each={plain()}>
          {([k, x]) => (
            <div class="flex min-w-0 items-baseline gap-3">
              <span class="w-28 shrink-0 truncate text-subtle">{k}</span>
              <Value v={x} />
            </div>
          )}
        </For>
      </div>
      <For each={blocks()}>
        {([k, x]) => (
          <div>
            <div class="mb-0.5 text-xs text-subtle">{k}</div>
            <Show when={Array.isArray(x)} fallback={<div class="ml-3 border-l border-line pl-3"><Record value={x as Record<string, unknown>} /></div>}>
              <div class="ml-3 border-l border-line pl-3"><Table rows={(x as Record<string, unknown>[]).filter(isObj)} /></div>
            </Show>
          </div>
        )}
      </For>
    </div>
  );
}

/** A value on one line: a status as dot + reason + message, a list joined, an empty as a dash. */
function Value(props: { v: unknown }) {
  const v = () => props.v;
  const isObj = (x: unknown): x is Record<string, unknown> => !!x && typeof x === "object" && !Array.isArray(x);
  return (
    <Show when={isObj(v())} fallback={
      <span class="min-w-0 truncate" classList={{ "text-fg": v() !== null && v() !== "" && !(Array.isArray(v()) && !(v() as unknown[]).length), "text-subtle": v() === null || v() === "" || (Array.isArray(v()) && !(v() as unknown[]).length) }} title={Array.isArray(v()) ? (v() as unknown[]).map(cell).join(", ") : cell(v())}>
        {Array.isArray(v()) ? ((v() as unknown[]).length ? (v() as unknown[]).map(cell).join(", ") : "—") : cell(v())}
      </span>
    }>
      {(() => {
        const s = v() as { ready?: boolean; reason?: string; message?: string };
        return (
          <span class="flex min-w-0 items-center gap-1.5">
            <span class={`size-1.5 shrink-0 rounded-full ${s.ready ? "bg-success" : "bg-warning"}`} />
            <span class="text-fg">{s.reason ?? (s.ready ? "ready" : "not ready")}</span>
            <Show when={s.message}><span class="min-w-0 truncate text-muted" title={s.message}>· {s.message}</span></Show>
          </span>
        );
      })()}
    </Show>
  );
}

const COLS = ["id", "name", "state", "region", "owner", "team", "branch"];
function Table(props: { rows: Record<string, unknown>[] }) {
  const c = () => cols(props.rows, COLS);
  return (
    <div class="my-1 overflow-x-auto">
      <table class="border-collapse text-xs">
        <thead>
          <tr><For each={c()}>{(k) => <th class="border-b border-line px-2 py-1 text-left font-bold text-fg-strong">{k}</th>}</For></tr>
        </thead>
        <tbody>
          <For each={props.rows}>
            {(r) => (
              <tr><For each={c()}>{(k) => <td class="max-w-[280px] truncate border-b border-line-subtle px-2 py-1 text-fg" title={cell(r[k])}>{cell(r[k])}</td>}</For></tr>
            )}
          </For>
        </tbody>
      </table>
    </div>
  );
}
const cols = (rows: Record<string, unknown>[], prefer: string[]) => {
  const keys = new Set<string>();
  rows.forEach((r) => Object.keys(r).forEach((k) => typeof r[k] !== "object" && keys.add(k)));
  return [...prefer.filter((k) => keys.has(k)), ...[...keys].filter((k) => !prefer.includes(k))].slice(0, 7);
};
