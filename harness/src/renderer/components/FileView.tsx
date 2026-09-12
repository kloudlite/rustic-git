import { For, Show, createMemo } from "solid-js";
import { Badge } from "../ui/Badge";
import { Button } from "../ui/Button";
import { DIFFS, FILES } from "../model";
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
export function FileView(props: { path: string; status?: string; onClose: () => void }) {
  const lang = createMemo(() => languageOf(props.path));
  const rows = createMemo<Row[] | undefined>(() => (DIFFS[props.path] ? parse(DIFFS[props.path]) : undefined));
  const body = () => FILES[props.path];
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
              Nothing to show yet. Contents come from the workspace's own tool server, which this build does not reach.
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

const GUTTER = "w-11 shrink-0 px-2 text-right tabular-nums select-none";

/**
 * Consecutive changed lines are one block, not a stack of separately tinted
 * rows: a replacement reads as one edit, so its removed and added halves share a
 * single rule down the edge.
 */
function runs(rows: Row[]): Row[][] {
  const out: Row[][] = [];
  for (const r of rows) {
    const last = out[out.length - 1];
    const changed = r.kind === "add" || r.kind === "del";
    const lastChanged = last && (last[0].kind === "add" || last[0].kind === "del");
    if (changed && lastChanged) last.push(r);
    else out.push([r]);
  }
  return out;
}

function Diff(props: { rows: Row[]; lang?: string }) {
  return (
    <For each={runs(props.rows)}>
      {(run) => (
        <Show
          when={run[0].kind !== "hunk"}
          fallback={
            <div class="flex items-center gap-3 border-y border-line-subtle bg-panel px-3 py-1 text-subtle">
              <span class="font-ui text-2xs tracking-[0.06em] uppercase">hunk</span>
              <span class="truncate">{run[0].text}</span>
            </div>
          }
        >
          <div
            class="border-l-2"
            classList={{
              "border-transparent": run[0].kind === "ctx",
              "border-created": run.every((r) => r.kind === "add"),
              "border-deleted": run.every((r) => r.kind === "del"),
              "border-warning": run.some((r) => r.kind === "add") && run.some((r) => r.kind === "del"),
            }}
          >
            <For each={run}>
              {(r) => (
                <div
                  class="flex"
                  classList={{ "bg-success-wash": r.kind === "add", "bg-danger-wash": r.kind === "del" }}
                >
                  <span class={`${GUTTER} ${r.kind === "add" ? "text-transparent" : "text-subtle"}`}>{r.old ?? ""}</span>
                  <span class={`${GUTTER} ${r.kind === "del" ? "text-transparent" : "text-subtle"}`}>{r.neu ?? ""}</span>
                  <span
                    class="w-4 shrink-0 text-center select-none"
                    classList={{
                      "text-created": r.kind === "add",
                      "text-deleted": r.kind === "del",
                      "text-transparent": r.kind === "ctx",
                    }}
                  >
                    {r.kind === "add" ? "+" : r.kind === "del" ? "−" : " "}
                  </span>
                  <span class="flex-1 pr-4 pl-2 whitespace-pre" innerHTML={highlight(r.text, props.lang)} />
                </div>
              )}
            </For>
          </div>
        </Show>
      )}
    </For>
  );
}

/** A file that has not changed: one gutter, one rule, and the code. */
function Plain(props: { text: string; lang?: string }) {
  return (
    <For each={props.text.split("\n")}>
      {(l, i) => (
        <div class="group flex hover:bg-hover">
          <span class={`${GUTTER} text-subtle group-hover:text-muted`}>{i() + 1}</span>
          <span class="w-px shrink-0 bg-line-subtle" />
          <span class="flex-1 pr-4 pl-3 whitespace-pre" innerHTML={highlight(l, props.lang)} />
        </div>
      )}
    </For>
  );
}
