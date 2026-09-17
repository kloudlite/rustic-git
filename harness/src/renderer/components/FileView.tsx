import { For, Match, Show, Switch, createMemo, createResource, createSignal } from "solid-js";
import { Badge } from "../ui/Badge";
import { Button } from "../ui/Button";
import { DIFFS, FILES } from "../model";
import * as live from "../live";
import { highlight, languageOf } from "../syntax";

type Row = { kind: "add" | "del" | "ctx" | "hunk"; old?: number; neu?: number; text: string };

/**
 * One file, opened from the tree or from a change, in place inside the thread.
 * A diff is shown when the file differs from the branch and its content
 * otherwise, because that is what a person came to look at in each case.
 *
 * The diff is numbered on both sides, the way every reviewer expects: the line
 * it was and the line it became, then the marker, then the code. Colour is a
 * wash plus a rule on the edge rather than a full-strength highlight, so a long
 * run of changes stays readable.
 */
export function FileView(props: { path: string; status?: string; scope?: string; onClose: () => void }) {
  const lang = createMemo(() => languageOf(props.path));
  /**
   * The workspace's own file and its diff, read from its tool server (spec §2: the desktop renders
   * a workspace from `/fs/*`). The fixtures are the fallback for a view with no scope — a tab that
   * is not a workspace, or the preview window.
   */
  const [fetched] = createResource(
    () => (props.scope ? { scope: props.scope, path: props.path } : undefined),
    (k) => live.fsFile(k.scope, k.path),
  );
  const [fetchedDiff] = createResource(
    () => (props.scope && props.status && props.status !== "?" ? { scope: props.scope, path: props.path } : undefined),
    (k) => live.fsDiff(k.scope, k.path),
  );
  const rows = createMemo<Row[] | undefined>(() => {
    const raw = fetchedDiff()?.diff ?? DIFFS[props.path];
    return raw ? parse(raw) : undefined;
  });
  const body = () => fetched()?.text ?? FILES[props.path];
  /** A file that is not text is named and measured, never rendered: "binary file, N bytes". */
  const binary = () => (fetched()?.binary ? `binary file, ${fetched()!.bytes ?? 0} bytes` : undefined);
  const parts = () => props.path.split("/");
  const name = () => parts()[parts().length - 1];
  const dir = () => parts().slice(0, -1).join("/");
  const added = () => rows()?.filter((r) => r.kind === "add").length ?? 0;
  const removed = () => rows()?.filter((r) => r.kind === "del").length ?? 0;

  return (
    <div class="flex min-h-0 min-w-0 flex-col overflow-hidden">
      <header class="flex items-center gap-3 border-b border-line-subtle px-4 py-2">
        <Button variant="ghost" size="sm" icon="chevronLeft" onClick={props.onClose}>Back</Button>
        <span class="min-w-0 flex-1 truncate font-mono text-sm">
          <Show when={dir()}>{(d) => <span class="text-subtle">{d()}/</span>}</Show>
          <span class="text-fg">{name()}</span>
        </span>
        <Show when={props.status}>
          {(st) => (
            <Badge tone={st() === "A" ? "success" : st() === "D" ? "danger" : "warning"}>
              {st() === "A" ? "added" : st() === "D" ? "deleted" : "modified"}
            </Badge>
          )}
        </Show>
        <Show when={rows()}>
          <span class="font-mono text-xs tabular-nums">
            <span class="text-created">+{added()}</span> <span class="text-deleted">−{removed()}</span>
          </span>
        </Show>
      </header>

      <div class="min-h-0 min-w-0 flex-1 overflow-auto font-mono text-xs leading-5 select-text">
        <Show
          when={rows() ?? body()}
          fallback={
            <p class="px-4 py-4 font-ui text-sm text-subtle">
              {binary() ?? (fetched.loading ? "reading…" : "This file could not be read.")}
            </p>
          }
        >
          <Show when={rows()} fallback={<Plain text={body()!} lang={lang()} />}>
            {(rs) => <Diff rows={rs()} lang={lang()} />}
          </Show>
        </Show>
      </div>
    </div>
  );
}

/**
 * Unified text → rows, numbered on both sides. The entries carry the whole file,
 * so numbering starts at one; a hunk header is still honoured if one appears.
 */
function parse(text: string): Row[] {
  const out: Row[] = [];
  let old = 1;
  let neu = 1;
  for (const line of text.split("\n")) {
    const hunk = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@(.*)$/.exec(line);
    if (hunk) {
      old = Number(hunk[1]);
      neu = Number(hunk[2]);
      out.push({ kind: "hunk", text: hunk[3].trim() });
    } else if (line.startsWith("+")) {
      out.push({ kind: "add", neu: neu++, text: line.slice(1) });
    } else if (line.startsWith("-")) {
      out.push({ kind: "del", old: old++, text: line.slice(1) });
    } else {
      out.push({ kind: "ctx", old: old++, neu: neu++, text: line.replace(/^ /, "") });
    }
  }
  return out;
}

const GUTTER = "w-11 shrink-0 px-2 text-right tabular-nums text-line-number select-none";

/**
 * The file as it is now, with what left it folded away: added lines carry a
 * green number, a run of deleted lines is one marker — `— 3 deleted —` — that
 * opens on click to show them. Reading a diff is reading the new file; the
 * old text is there when it is wanted, not in the way when it is not.
 */
function Diff(props: { rows: Row[]; lang?: string }) {
  const groups = createMemo(() => {
    const out: (Row | { kind: "gone"; rows: Row[] })[] = [];
    for (const r of props.rows) {
      const last = out[out.length - 1];
      if (r.kind === "del") {
        if (last && last.kind === "gone") last.rows.push(r);
        else out.push({ kind: "gone", rows: [r] });
      } else out.push(r);
    }
    return out;
  });

  return (
    <For each={groups()}>
      {(g) => (
        <Switch>
          <Match when={g.kind === "hunk"}>
            <div class="flex items-center gap-3 border-y border-line bg-codeblock px-3 py-1 text-subtle">
              <span class="font-ui text-2xs uppercase">hunk</span>
              <span class="truncate">{(g as Row).text}</span>
            </div>
          </Match>
          <Match when={g.kind === "gone"}>
            <Gone rows={(g as { rows: Row[] }).rows} lang={props.lang} />
          </Match>
          <Match when={g.kind === "add" || g.kind === "ctx"}>
            <div class="flex" classList={{ "bg-success-wash/40": g.kind === "add" }}>
              <span class={`${GUTTER} ${g.kind === "add" ? "text-created" : ""}`}>{(g as Row).neu}</span>
              <span class="flex-1 pr-4 pl-3 whitespace-pre" innerHTML={highlight((g as Row).text, props.lang)} />
            </div>
          </Match>
        </Switch>
      )}
    </For>
  );
}

/** A run of deleted lines, folded to one marker until it is opened. */
function Gone(props: { rows: Row[]; lang?: string }) {
  const [open, setOpen] = createSignal(false);
  return (
    <>
      <button
        class="flex w-full items-center gap-2 py-0.5 text-left text-deleted select-none hover:bg-danger-wash/40"
        onClick={() => setOpen((v) => !v)}
        title={open() ? "Hide the deleted lines" : "Show the deleted lines"}
      >
        <span class={`${GUTTER} text-transparent`}>0</span>
        <span class="h-px w-6 bg-deleted/60" />
        <span class="text-sm">{props.rows.length} deleted</span>
        <span class="h-px w-6 bg-deleted/60" />
      </button>
      <Show when={open()}>
        <For each={props.rows}>
          {(r) => (
            <div class="flex bg-danger-wash/40">
              <span class={`${GUTTER} text-deleted`}>{r.old}</span>
              <span class="flex-1 pr-4 pl-3 whitespace-pre opacity-70" innerHTML={highlight(r.text, props.lang)} />
            </div>
          )}
        </For>
      </Show>
    </>
  );
}

/** A file that has not changed: one gutter, one rule, and the code. */
function Plain(props: { text: string; lang?: string }) {
  return (
    <For each={props.text.split("\n")}>
      {(l, i) => (
        <div class="group flex hover:bg-hover">
          <span class={`${GUTTER} group-hover:text-line-number-active`}>{i() + 1}</span>
          <span class="w-px shrink-0 bg-line-subtle" />
          <span class="flex-1 pr-4 pl-3 whitespace-pre" innerHTML={highlight(l, props.lang)} />
        </div>
      )}
    </For>
  );
}
