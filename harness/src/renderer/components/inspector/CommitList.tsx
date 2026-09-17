import { For, Show, createSignal } from "solid-js";
import { Icon } from "../../ui/Icon";
import { Row, Gutter } from "../../ui/parts";
import { rowTone, statusBadge } from "../../rows";
import type { FsCommit } from "../../live";

/**
 * What this branch has COMMITTED, under what is still uncommitted — the second half of the CHANGES
 * tab. A person who has just committed saw an empty panel and no sign of the work (owner,
 * 2026-09-18); the commits are where it went.
 *
 * A commit opens to the files it touched, with the same letters and tones the uncommitted list
 * uses, because `--name-status` and `git status` speak the same alphabet (`A`/`M`/`D`/`R`).
 */
export function CommitList(props: { commits: readonly FsCommit[]; onOpen: (path: string, status?: string) => void }) {
  const [open, setOpen] = createSignal(new Set<string>());
  const toggle = (hash: string) =>
    setOpen((was) => {
      const next = new Set(was);
      if (!next.delete(hash)) next.add(hash);
      return next;
    });
  /** Its own time, as the commit recorded it — never when this panel happened to read it. */
  const when = (at: string) => {
    const t = new Date(at);
    return Number.isNaN(+t) ? "" : t.toTimeString().slice(0, 5);
  };

  return (
    <For each={props.commits}>
      {(c) => (
        <>
          <Row class="h-5.5 pl-2" onClick={() => toggle(c.hash)} title={`${c.short} · ${c.author}`}>
            <Gutter>
              <Icon name={open().has(c.hash) ? "chevronDown" : "chevronRight"} size={16} class="text-muted" />
            </Gutter>
            <span class="min-w-0 flex-1 truncate px-1">{c.subject}</span>
            <span class="shrink-0 pr-1 font-mono text-2xs text-subtle">{c.short}</span>
            <span class="shrink-0 pr-2 text-2xs text-subtle">{when(c.at)}</span>
          </Row>
          <Show when={open().has(c.hash)}>
            <For each={c.files}>
              {(f) => (
                <Row class="h-5.5 pl-8" onClick={() => props.onOpen(f.path, f.status)} title={f.from ? `${f.from} → ${f.path}` : f.path}>
                  <span class={`min-w-0 flex-1 truncate px-1 ${rowTone(f.status)}`}>{f.path}</span>
                  <Show when={statusBadge(f.status)}>
                    {(b) => <span class={`shrink-0 pr-2 font-mono text-2xs ${rowTone(f.status)}`}>{b()}</span>}
                  </Show>
                </Row>
              )}
            </For>
          </Show>
        </>
      )}
    </For>
  );
}
