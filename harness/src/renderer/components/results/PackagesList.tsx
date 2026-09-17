import { For, Show } from "solid-js";
import { Card, Pill } from "./parts";

/** This machine's packages. A pin (`attr@version`) reads as one chip, like the list it came from. */
export function PackagesList(props: { data: string[] }) {
  return (
    <Card>
      <Show when={props.data.length} fallback={<span class="text-subtle">none</span>}>
        <div class="flex flex-wrap gap-1"><For each={props.data}>{(p) => <Pill>{p}</Pill>}</For></div>
      </Show>
    </Card>
  );
}
