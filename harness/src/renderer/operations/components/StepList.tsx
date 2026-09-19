import { For, Show, createSignal, createUniqueId } from "solid-js";
import { Badge } from "../../ui/Badge";
import { Icon } from "../../ui/Icon";
import { Empty, Gutter } from "../../ui/parts";
import { stepDetailRows, stepRows, type StepRow } from "../present";
import type { OperationView } from "../types";

/**
 * The internal calls of one operation, drawn as the workbench draws a tree: the state, the
 * capability and its target, what it waits for, how long it has been doing it, and how many
 * calls were in flight beside it. Expanded, it shows everything the durable record and the
 * redacted events hold for that step — including the uncertain ones, which never read as
 * success.
 */
export function StepList(props: { view: OperationView; now: number }) {
  const rows = () => stepRows(props.view, props.now);
  return (
    <Show when={rows().length} fallback={<Empty>No internal steps reported yet.</Empty>}>
      <For each={rows()}>{(row) => <StepItem view={props.view} row={row} now={props.now} />}</For>
    </Show>
  );
}

function StepItem(props: { view: OperationView; row: StepRow; now: number }) {
  const [open, setOpen] = createSignal(false);
  const detailId = `operation-step-${createUniqueId()}`;
  const step = () => props.view.steps.find((candidate) => candidate.stepId === props.row.stepId);
  return (
    <div data-component="operation-step" class="op-step">
      <button type="button" class="op-step-toggle flex min-h-6 w-full items-start gap-0.5 bg-transparent px-3 py-0.5 text-left whitespace-nowrap hover:bg-hover" aria-expanded={open()} aria-controls={detailId} onClick={() => setOpen(!open())}>
        <Gutter><Icon name={open() ? "chevronDown" : "chevronRight"} size={16} class="text-muted" /></Gutter>
        <span class="min-w-0 flex-1 px-1 leading-[18px] wrap-words">
          <span class="text-fg">{props.row.title}</span>
          <Show when={props.row.target}>
            <span class="text-subtle"> · {props.row.target}</span>
          </Show>
          <Show when={props.row.dependencyLabel}>
            <span class="text-subtle"> · {props.row.dependencyLabel}</span>
          </Show>
          <Show when={props.row.queueLabel}>
            <span class="text-warning"> · {props.row.queueLabel}</span>
          </Show>
        </span>
        <Show when={props.row.concurrencyLabel}>
          <span class="shrink-0 px-1 font-mono text-xs text-subtle">{props.row.concurrencyLabel}</span>
        </Show>
        <Show when={props.row.timeLabel}>
          <span class="shrink-0 px-1 font-mono text-xs tabular-nums text-subtle">{props.row.timeLabel}</span>
        </Show>
        <Show when={props.row.errorLabel}>
          <span class="shrink-0 px-1 font-mono text-xs text-danger">{props.row.errorLabel}</span>
        </Show>
        <Badge tone={props.row.tone}>{props.row.stateLabel}</Badge>
      </button>
      <Show when={open()}>
        <Show when={step()}>
          {(current) => (
            <div id={detailId} class="op-detail">
              <For each={stepDetailRows(props.view, current(), props.now)}>
                {(detail) => (
                  <div class="flex gap-3 px-6 py-0.5 text-sm">
                    <span class="w-[7.5rem] shrink-0 text-muted">{detail.label}</span>
                    <span class={detail.kind === "untrusted_text" ? "op-args min-w-0 flex-1 text-subtle" : "min-w-0 flex-1 wrap-words text-fg"}>
                      {detail.value}
                    </span>
                  </div>
                )}
              </For>
              <Show when={props.row.evidence.length}>
                <div class="flex gap-3 px-6 py-0.5 text-sm">
                  <span class="w-[7.5rem] shrink-0 text-muted">evidence</span>
                  <span class="min-w-0 flex-1 wrap-words font-mono text-xs text-subtle">{props.row.evidence.join(", ")}</span>
                </div>
              </Show>
              <Show when={props.row.reconcileLabel}>
                <div class="px-6 py-0.5 text-sm text-warning">
                  {props.row.reconcileLabel}: the conclusion comes from the durable record.
                </div>
              </Show>
            </div>
          )}
        </Show>
      </Show>
    </div>
  );
}
