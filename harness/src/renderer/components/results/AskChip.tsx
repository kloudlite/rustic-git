import { For, Show } from "solid-js";
import { exchangesOf } from "../../live";
import { exchangeText } from "../../rows";
import { Card, Dot, Pill } from "./parts";

/**
 * An ask is not a document: the answer to `kl_workspace_ask` is one sentence, and what the person
 * wants to see is what became of it. The bench publishes every exchange as it moves
 * (queued → running → done), so this reads the live rows rather than the tool's own answer.
 */
export function AskChip(props: { workspace: string }) {
  const rows = () => exchangesOf(props.workspace);
  return (
    <Card>
      <Show when={rows().length} fallback={<span class="text-subtle">queued in {props.workspace}'s session</span>}>
        <For each={rows().filter((e) => e.dir === "out")}>
          {(e) => {
            const reply = () => rows().find((r) => r.ref === e.id);
            return (
              <div class="flex flex-col gap-0.5">
                <div class="flex min-w-0 items-baseline gap-2">
                  <Dot state={e.state} />
                  <Pill>{e.workspace}</Pill>
                  <span class="shrink-0 text-muted">{e.state}</span>
                  <span class="min-w-0 flex-1 truncate text-fg" title={e.text}>{exchangeText(e.text)}</span>
                </div>
                <Show when={reply()}>{(r) => <div class="ml-5 border-l border-line pl-3 text-muted">{r().text}</div>}</Show>
              </div>
            );
          }}
        </For>
      </Show>
    </Card>
  );
}
