import { For } from "solid-js";
import { Card, Pill } from "./parts";

/** Everything this session can do, grouped as the catalogue groups it, with each tool's effect. */
export function CapabilitiesList(props: { data: { group: string; tools: { name: string; effect: string; summary: string }[] }[] }) {
  return (
    <Card>
      <For each={props.data}>
        {(g) => (
          <div class="flex flex-col gap-0.5">
            <div class="text-xs text-subtle">{g.group}</div>
            <For each={g.tools}>
              {(t) => (
                <div class="flex min-w-0 items-baseline gap-2">
                  <span class="shrink-0 text-fg">{t.name}</span>
                  <span class={t.effect === "destroy" ? "text-danger" : t.effect === "write" ? "text-warning" : ""}>
                    {t.effect ? <Pill tone={t.effect === "read" ? "plain" : "warn"}>{t.effect}</Pill> : null}
                  </span>
                  <span class="min-w-0 flex-1 truncate text-muted" title={t.summary}>{t.summary}</span>
                </div>
              )}
            </For>
          </div>
        )}
      </For>
    </Card>
  );
}
