import { For, Show, createEffect, createMemo, createSignal, onCleanup, type JSX } from "solid-js";
import { Icon } from "../ui/Icon";
import { highlight, languageOf } from "../syntax";
import type { Message } from "../model";
import { ResultCard, pickRenderer } from "./results";
import { grepBlock, plainBlock, readBlock, type CodeBlock } from "./results/code";
import { diagnostics, toolError, toolLine } from "./results/toolline";
import { FileDiff } from "./results/FileDiff";
import { editFile, patchFiles } from "./results/diff";
import { defaultOpen } from "./results/opencode-map";
import { Spinner } from "./Motion";
import { OperationPanel, operationIdFromAction, operationStore, type OperationStore } from "../operations/index.ts";

type Action = Extract<Message, { role: "action" }>;

/**
 * A tool call, drawn for what it is rather than as one shape for all: a
 * command with its terminal output, a read with the file, a write with what
 * was written, an edit as a diff, a search with its hits, a platform call
 * with its answer laid out. Every one has the same head — a state dot, the
 * verb, the subject, how long it took, a chevron — and folds to that head.
 * A failed call shows only its error; nobody wants the whole response then.
 */
export function ToolCall(props: { a: Action; operations?: OperationStore; sessionId?: string; workspaceId?: string }) {
  const a = () => props.a;
  // Folded to its one line, opened on click. A transcript of open blocks is a wall; what a person
  // wants at a glance is WHAT ran and whether it worked — a failure opens itself, because that is
  // the one they were about to click anyway.
  const [open, setOpen] = createSignal(false);
  createEffect(() => props.a.ok === false && setOpen(true));
  // What opens itself, per opencode's own policy: a shell and an edit are the thing you came to
  // see; a patch that only deletes is not (`part-default-open.ts:19`).
  createEffect(() => defaultOpen(props.a.tool, true, true, deletionOnly()) && setOpen(true));
  const html = () => /^\s*<!doctype html|^\s*<html/i.test(a().output ?? "");
  /**
   * A refusal is not a failure: the TUI strikes the line through and keeps the row quiet
   * (`routes/session/index.tsx:1866` — `QuestionRejectedError`, "rejected permission",
   * "specified a rule", "user dismissed").
   */
  const denied = () => /QuestionRejectedError|rejected permission|specified a rule|user dismissed|refused/i.test(a().output ?? "");
  const failed = () => (a().ok === false || html()) && !denied();
  // While it runs: a spinner in the dot's place and a clock counting up, so
  // a slow command is visibly alive rather than merely unfinished.
  const [tick, setTick] = createSignal(Date.now());
  const timer = setInterval(() => a().pending && setTick(Date.now()), 500);
  onCleanup(() => clearInterval(timer));
  const fmt = (ms: number) => (ms < 1000 ? `${Math.max(0, Math.round(ms))}ms` : `${(ms / 1000).toFixed(1)}s`);
  const took = () => (a().pending ? fmt(tick() - (a().ts ?? tick())) : a().ms !== undefined ? fmt(a().ms!) : "");
  const deletionOnly = () => {
    const files = patchFiles(String((props.a.args ?? {}).patch ?? (props.a.args ?? {}).diff ?? ""));
    return files.length > 0 && files.every((f) => f.type === "delete");
  };
  const line = createMemo(() => toolLine(a().tool, a().args ?? {}, a().output, { pending: a().pending, ok: a().ok !== false, secs: a().pending ? Math.round((tick() - (a().ts ?? tick())) / 1000) : undefined }));
  const operation = createMemo(() => {
    const id = operationIdFromAction(a());
    return id ? (props.operations ?? operationStore())?.open(id, { sessionId: props.sessionId, workspaceId: props.workspaceId }) : undefined;
  });

  // opencode's contract (§16b): a read or a search is ONE line, but an edit and a command are rail
  // BLOCKS — their result is the thing you came to see, so it is not behind a click.
  const railed = () => a().tool === "edit" || a().tool === "bash" || a().tool === "write";
  return (
    /* The TUI's own geometry: a row is indented three cells, a block row is a rail with one cell of
       padding inside it, and a block is always separated from what came before
       (`routes/session/index.tsx:1914` `InlineToolRow`, `:1994` `BlockTool`). */
    <div class="flex flex-col font-mono" classList={{ "my-1 border-l border-line bg-raised py-1 pl-2": railed(), "pl-3": !railed() }}>
      {/* One muted line: an icon two cells wide, then what ran. The card is what a click opens. */}
      <button class="group flex w-full items-baseline text-left" onClick={() => setOpen((v) => !v)}>
        <span
          class="w-[2ch] shrink-0"
          classList={{ "text-danger": failed(), "text-accent": a().pending, "text-muted": !failed() && !a().pending }}
        >
          {/* While it runs the TUI shows the braille spinner in the icon's place (`Spinner`, 80 ms);
              a finished row is a dot, and a refused one keeps the dot and strikes the line. */}
          <Show when={a().pending} fallback="⏺"><Spinner /></Show>
        </span>
        <span
          class="min-w-0 flex-1 truncate"
          /* Complete rows are muted, a live one is plain text, a failure is red: `fg()`, `:1875`. */
          classList={{ "text-danger": failed(), "text-fg": a().pending, "text-muted": !failed() && !a().pending, "line-through": denied() }}
        >
          <Show when={a().pending} fallback={<>
            <Show when={line().verb}><span classList={{ "text-fg": !failed() }}>{line().verb} </span></Show>
            {line().arg}
            <Show when={line().count}>{(c) => <span class="text-subtle"> ({c()})</span>}</Show>
          </>}>
            {/* `~ {pending}` is how the TUI says "not yet" (`:1955`). */}
            ~ {[line().verb, line().arg].filter(Boolean).join(" ")}
          </Show>
        </span>
        <span class="shrink-0 tabular-nums text-subtle">{took()}</span>
        <Icon name={open() ? "chevronDown" : "chevronRight"} size={14} class="shrink-0 text-subtle opacity-40 group-hover:opacity-100" />
      </button>
      {/* An agent says how much it did and where to watch it, the way opencode's subagent row does. */}
      <Show when={a().tool === "ask" && (a().args ?? {}).to === "agent"}>
        <div class="pl-4 text-subtle">
          ↳ {a().pending ? "working" : "reported"}
          <Show when={a().ms}>{(ms) => <> · {(ms() / 1000).toFixed(1)}s</>}</Show>
          <span class="pl-3">ctrl+x ↓ view agents</span>
        </div>
      </Show>
      <Show when={open()}>
        <div class="springy flex min-w-0">
          <span class="w-4 shrink-0 text-subtle">⎿</span>
          <div class="min-w-0 flex-1" classList={{ "border-l border-line pl-3": !railed() }}>
          <Show when={failed()} fallback={<Body a={a()} />}>
            <Fail tool={a().tool} text={a().output ?? ""} />
          </Show>
          </div>
        </div>
      </Show>
      <Show when={operation()}>{(entry) => <OperationPanel view={entry().view()} onResync={entry().resync} onDecision={entry().controls.decide} onAdditionalInput={entry().controls.answer} onCancel={entry().controls.cancel} loadError={entry().error()} onRetry={entry().reload} />}</Show>
    </div>
  );
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
function Fail(props: { tool?: string; text: string }) {
  const card = () => toolError(props.tool, props.text);
  const [copied, setCopied] = createSignal(false);
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
  return (
    <div data-component="tool-error-card" class="my-1 flex flex-col gap-0.5 border-l-2 border-danger pl-2">
      {/* What failed and in one word how (`tool-error-card.tsx:66`); the detail is underneath. */}
      <div class="flex items-baseline gap-2">
        <span class="shrink-0 text-danger">⊘ {card().title}</span>
        <span class="min-w-0 flex-1 truncate text-muted">{card().subtitle}</span>
        <button
          class="shrink-0 text-subtle hover:text-fg"
          onClick={() => { void navigator.clipboard.writeText(props.text); setCopied(true); setTimeout(() => setCopied(false), 2000); }}
        >
          {copied() ? "Copied" : "Copy error"}
        </button>
      </div>
      <pre class="m-0 whitespace-pre-wrap wrap-words font-[inherit] text-danger">{msg() || "failed"}</pre>
    </div>
  );
}

/** The errors a tool reported, at most three — a wall of them is how the edit stops being read. */
function Diagnostics(props: { text?: string }) {
  const rows = createMemo(() => diagnostics(props.text));
  return (
    <For each={rows()}>
      {(d) => (
        <div class="flex min-w-0 items-baseline gap-2 text-danger">
          <span class="shrink-0">Error</span>
          <span class="shrink-0 tabular-nums">[{d.line}:{d.char}]</span>
          <span class="min-w-0 truncate" title={d.message}>{d.message}</span>
        </div>
      )}
    </For>
  );
}

/** The plan, as the to-do list it is: `[✓]` done, `[•]` doing, `[ ]` still to do. */
function Todos(props: { set: { text: string; state?: string }[]; doing?: string; done?: string }) {
  const mark = (t: { text: string; state?: string }) =>
    t.state === "done" || t.text === props.done ? "✓" : t.state === "doing" || t.text === props.doing ? "•" : " ";
  return (
    <div class="py-1">
      <For each={props.set}>
        {(t) => (
          <div class="flex min-w-0 gap-2" classList={{ "text-warning": mark(t) === "•", "text-muted": mark(t) !== "•" }}>
            <span class="shrink-0 select-none">[{mark(t)}]</span>
            <span class="min-w-0 wrap-words" classList={{ "line-through": mark(t) === "✓" }}>{t.text}</span>
          </div>
        )}
      </For>
    </div>
  );
}

function Body(props: { a: Action }) {
  const a = () => props.a;
  const g = () => a().args ?? {};
  const card = createMemo(() => !!pickRenderer(a().tool, a().output, a().args));
  return (
    <Show when={!a().pending} fallback={<Show when={a().output}><Out text={a().output!} /></Show>}>
      <Switch tool={a().tool}>
        {{
          // The command and what it printed, one verbatim block (`message-part.tsx:2091`): a
          // person reading a shell row wants to copy both, and the `$` is how they tell them apart.
          bash: () => <Shell cmd={String(g().command ?? g().cmd ?? "")} out={plainBlock(a().output ?? "").lines.map((l) => l.text).join("\n")} />,
          read: () => <Code path={String(g().path ?? "")} text={a().output ?? ""} from={Number(g().offset ?? 1)} block={readBlock(a().output ?? "")} />,
          write: () => <Code path={String(g().path ?? "")} text={String(g().content ?? "")} from={1} />,
          plan: () => <Todos set={((g().set as { text: string; state?: string }[] | string[]) ?? []).map((t) => (typeof t === "string" ? { text: t } : t))} doing={g().doing as string} done={g().done as string} />,
          edit: () => <Edits path={String(g().path ?? "")} edits={(g().edits as { oldText: string; newText: string }[]) ?? []} result={a().output} />,
          // A patch is many files at once; each is its own accordion, and a delete stays shut.
          patch: () => <Patch text={String(g().patch ?? g().diff ?? a().output ?? "")} />,
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
      <pre class="m-0 py-1 whitespace-pre-wrap wrap-words font-[inherit] text-muted [tab-size:4]">{all() ? ls().join("\n") : folded()}</pre>
      <Show when={more()}>
        <button class="pb-1 text-subtle hover:text-fg" onClick={() => setAll((v) => !v)}>{all() ? "collapse" : ls().length > HEAD ? `… +${ls().length - HEAD} lines (click to expand)` : "… show all"}</button>
      </Show>
    </Show>
  );
}

/**
 * A file, or the part of it read. The tool server's own line numbers are the gutter — ONE gutter,
 * with the real numbers, so an offset page starts where it actually starts. The trailer it prints
 * is the block's footer, not a line of code, and the header names the file.
 */
function Code(props: { path: string; text: string; from: number; block?: CodeBlock }) {
  const HEAD = 12;
  const [all, setAll] = createSignal(false);
  const lang = () => languageOf(props.path);
  const block = () => props.block ?? { lines: props.text.replace(/\s+$/, "").split("\n").map((text, i) => ({ n: props.from + i, text })) };
  const shown = () => (all() ? block().lines : block().lines.slice(0, HEAD));
  const gutter = () => Math.max(2, String(block().lines[block().lines.length - 1]?.n ?? "").length);
  return (
    <Show when={props.text.trim()} fallback={<div class="py-1 text-subtle">(empty)</div>}>
      <div class="my-1 flex items-baseline gap-2 text-subtle">
        <span class="min-w-0 truncate text-muted">{props.path}</span>
        <span class="flex-1" />
        <span class="tabular-nums">{block().lines.length} lines</span>
      </div>
      <div class="overflow-x-auto rounded-[2px] bg-codeblock py-1 [tab-size:4]">
        <For each={shown()}>
          {(l) => (
            <div class="flex">
              <span class="shrink-0 pr-3 text-right text-line-number select-none" style={{ width: `${gutter() + 1}ch`, "padding-left": "8px" }}>{l.n ?? ""}</span>
              <span class="whitespace-pre" innerHTML={highlight(l.text, lang())} />
            </div>
          )}
        </For>
      </div>
      <div class="flex items-baseline gap-3 pb-1">
        <Show when={block().lines.length > HEAD}>
          <button class="text-subtle hover:text-fg" onClick={() => setAll((v) => !v)}>{all() ? "collapse" : `… +${block().lines.length - HEAD} lines (click to expand)`}</button>
        </Show>
        <Show when={block().footer}>{(f) => <span class="text-subtle">{f()}</span>}</Show>
      </div>
    </Show>
  );
}

/** An edit as the diff it is, under the file's own sticky header; one hunk per replacement. */
function Edits(props: { path: string; edits: { oldText: string; newText: string }[]; result?: string }) {
  return (
    <div class="my-1 flex flex-col gap-2">
      <FileDiff file={editFile(props.path, props.edits)} />
      <Diagnostics text={props.result} />
      <Show when={props.result && !/^Successfully/.test(props.result)}><div class="text-subtle">{props.result}</div></Show>
    </div>
  );
}

/** A unified patch: one accordion per file, with `Created` / `Deleted` / `Moved` where it applies. */
function Patch(props: { text: string }) {
  const files = createMemo(() => patchFiles(props.text));
  return (
    <Show when={files().length} fallback={<Out text={props.text} />}>
      <Show when={files().length > 1}>
        <div class="py-1 text-subtle">{files().length} files</div>
      </Show>
      <For each={files()}>{(f) => <FileDiff file={f} />}</For>
    </Show>
  );
}

/** The shell body: `$ command`, a blank line, then what it printed — and a copy of the lot. */
function Shell(props: { cmd: string; out: string }) {
  const text = () => `$ ${props.cmd}${props.out.trim() ? `\n\n${props.out}` : ""}`;
  const [done, setDone] = createSignal(false);
  return (
    <div data-slot="bash-scroll" class="group/shell relative min-w-0">
      <button
        data-slot="bash-copy"
        class="absolute top-1 right-1 rounded-[2px] bg-fg/10 px-1 text-subtle opacity-0 group-hover/shell:opacity-100 hover:text-fg"
        onClick={() => { void navigator.clipboard.writeText(text()); setDone(true); setTimeout(() => setDone(false), 2000); }}
      >
        {done() ? "copied" : "copy"}
      </button>
      <Out text={text()} empty="(no output)" head={9} />
    </div>
  );
}

/** Search results: the path and the line in a gutter, the match as code beside it. */
function Hits(props: { text: string }) {
  const HEAD = 12;
  const [all, setAll] = createSignal(false);
  const rows = () => grepBlock(props.text);
  const plain = () => props.text.replace(/\s+$/, "").split("\n").filter(Boolean);
  return (
    <Show when={plain().length} fallback={<div class="py-1 text-subtle">nothing</div>}>
      <div class="py-1 [tab-size:4]">
        <Show
          when={rows().length}
          fallback={<For each={all() ? plain() : plain().slice(0, HEAD)}>{(l) => <div class="truncate text-muted">{l}</div>}</For>}
        >
          <For each={all() ? rows() : rows().slice(0, HEAD)}>
            {(r) => (
              <div class="flex gap-2">
                <span class="shrink-0 truncate text-subtle" style={{ "max-width": "28ch" }} title={r.path}>{r.path}</span>
                <span class="shrink-0 text-right text-line-number tabular-nums select-none" style={{ width: "5ch" }}>{r.n}</span>
                <span class="min-w-0 flex-1 truncate whitespace-pre text-fg">{r.text}</span>
              </div>
            )}
          </For>
        </Show>
      </div>
      <Show when={(rows().length || plain().length) > HEAD}>
        <button class="pb-1 text-subtle hover:text-fg" onClick={() => setAll((v) => !v)}>{all() ? "collapse" : `… +${(rows().length || plain().length) - HEAD} lines (click to expand)`}</button>
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
          <Show when={str("state")}>{(st) => <span class="text-muted">{st()}</span>}</Show>
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
            <div class="mb-0.5 text-subtle">{k}</div>
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
      <table class="border-collapse">
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
