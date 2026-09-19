import { For, Show, createSignal, createUniqueId } from "solid-js";
import { Badge } from "../../ui/Badge";
import { Button } from "../../ui/Button";
import { Icon } from "../../ui/Icon";
import { Empty } from "../../ui/parts";
import { cancelCallbackPayload, type AdditionalInputCallbackPayload, type CancelCallbackPayload, type DecisionCallbackPayload } from "../bridge";
import {
  concurrencyLabel,
  errorRows,
  evidenceRows,
  operationRows,
  panelMeta,
  panelTitle,
  progressLabel,
  stateLabel,
  stateTone,
  stalledSteps,
  unknownRows,
} from "../present";
import type { OperationView, ResyncRequest } from "../types";
import type { OperationLoadError } from "../store";
import { DecisionPrompt } from "./DecisionPrompt";
import { StepList } from "./StepList";

/**
 * One operation, under the tool call that started it: collapsed to what was asked and how
 * it is going, expanded to every internal call, decision, unknown outcome, error and piece
 * of evidence. Everything it shows is a projection of the durable record and the durable
 * events; nothing here is estimated, and nothing here can approve, cancel or resume by
 * itself — those leave through the callbacks below, which O08 wires to the authenticated
 * control endpoints.
 */
export type OperationPanelProps = {
  view: OperationView;
  /** A ticking clock, so elapsed labels advance without re-reading anything. */
  now?: number;
  expanded?: boolean;
  onToggle?: (open: boolean) => void;
  /** A repair the view asked for: `snapshot` re-reads the record, `events_after` the cursor. */
  onResync?: (request: ResyncRequest) => void | Promise<void>;
  onDecision?: (payload: DecisionCallbackPayload) => void | Promise<void>;
  onAdditionalInput?: (payload: AdditionalInputCallbackPayload) => void | Promise<void>;
  onCancel?: (payload: CancelCallbackPayload) => void | Promise<void>;
  loadError?: OperationLoadError;
  onRetry?: () => void | Promise<void>;
};

export function OperationPanel(props: OperationPanelProps) {
  const [open, setOpen] = createSignal(!!props.expanded);
  const bodyId = `operation-${createUniqueId()}`;
  const [controlPending, setControlPending] = createSignal<"cancel" | "resync">();
  const [controlError, setControlError] = createSignal<string>();
  const isOpen = () => props.expanded === undefined ? open() : props.expanded;
  const now = () => props.now ?? Date.now();
  const toggle = () => {
    const next = !isOpen();
    if (props.expanded === undefined) setOpen(next);
    props.onToggle?.(next);
  };
  const runControl = async (kind: "cancel" | "resync", action: (() => void | Promise<void>) | undefined) => {
    if (!action || controlPending()) return;
    setControlPending(kind);
    setControlError(undefined);
    try {
      await action();
    } catch (cause) {
      setControlError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setControlPending(undefined);
    }
  };
  const cancel = () => {
    const payload = cancelCallbackPayload(props.view);
    if (payload) void runControl("cancel", () => props.onCancel?.(payload));
  };
  const stalled = () => stalledSteps(props.view, now(), 60_000);
  return (
    <section data-component="operation" class="op-panel border-b border-line-subtle">
      <header class="op-header flex min-h-6 items-center gap-2 px-3 py-0.5">
        <button
          type="button"
          class="flex min-w-0 flex-1 items-center gap-1 bg-transparent text-left"
          aria-expanded={isOpen()}
          aria-controls={bodyId}
          onClick={toggle}
        >
          <Icon name={isOpen() ? "chevronDown" : "chevronRight"} size={16} class="text-muted" />
          <span class="min-w-0 flex-1 truncate text-sm text-fg">{panelTitle(props.view)}</span>
        </button>
        <Show when={props.view.status === "repairing" || props.view.status === "disconnected"}>
          <span role="status" aria-live="polite" class="shrink-0 font-mono text-xs text-warning">
            {props.view.status === "disconnected" ? "reconnecting" : "catching up"}
          </span>
        </Show>
        <span class="shrink-0 font-mono text-xs tabular-nums text-subtle">{progressLabel(props.view)}</span>
        <Show when={props.view.steps.some((step) => step.state === "running")}>
          <Icon name="spinner" size={14} class="text-accent animate-spin" />
        </Show>
        <span role="status" aria-live="polite"><Badge tone={stateTone(props.view.state)}>{stateLabel(props.view.state)}</Badge></span>
        <Show when={cancelCallbackPayload({ ...props.view, resync: undefined })}>
          <Button variant="ghost" size="sm" disabled={controlPending() !== undefined} onClick={cancel} title="Ask the operation to stop">
            {controlPending() === "cancel" ? "Cancelling" : "Cancel"}
          </Button>
        </Show>
      </header>
      <Show when={isOpen()}>
        <div id={bodyId} class="op-body">
          <Show when={props.view.resync}>
            {(request) => (
              <div class="flex items-center gap-2 border-t border-line-subtle bg-warning-wash px-4 py-1.5 text-sm text-warning">
                <span class="min-w-0 flex-1">
                  {request().need === "snapshot"
                    ? "The log and this view disagree: reloading the operation record."
                    : `Events are missing after #${request().afterSequence}: asking for the cursor.`}
                </span>
                <Button variant="default" size="sm" disabled={controlPending() !== undefined} onClick={() => void runControl("resync", () => props.onResync?.(request()))}>
                  {controlPending() === "resync" ? "Reloading" : "Reload now"}
                </Button>
              </div>
            )}
          </Show>
          <Show when={controlError()}>{(message) => <p role="alert" class="m-0 border-t border-line-subtle px-4 py-1 text-xs text-danger">{message()}</p>}</Show>
          <Show when={props.loadError}>{(error) => <div role="alert" class="flex items-center gap-2 border-t border-line-subtle px-4 py-1 text-xs text-danger"><span class="min-w-0 flex-1">{error().message}</span><Button variant="default" size="sm" onClick={() => void runControl("resync", props.onRetry)}>Retry</Button></div>}</Show>
          <Show when={props.view.notice}>
            {(notice) => <p class="m-0 border-t border-line-subtle px-4 py-1 text-xs text-subtle">{notice().message}</p>}
          </Show>
          <div class="border-t border-line-subtle py-1">
            <For each={operationRows(props.view, now())}>
              {(row) => (
                <div class="op-row flex gap-3 px-4 py-0.5 text-sm">
                  <span class="w-[7.5rem] shrink-0 text-muted">{row.label}</span>
                  <span
                    class={
                      row.kind === "untrusted_text"
                        ? "op-args min-w-0 flex-1 text-subtle"
                        : row.kind === "error"
                          ? "min-w-0 flex-1 wrap-words text-danger"
                          : "min-w-0 flex-1 wrap-words text-fg"
                    }
                  >
                    {row.value}
                  </span>
                </div>
              )}
            </For>
            <p class="m-0 px-4 py-0.5 text-xs text-subtle">
              {panelMeta(props.view, now()).join(" · ")} · {concurrencyLabel(props.view)}
            </p>
          </div>
          <Show when={unknownRows(props.view).length}>
            <div class="border-t border-line-subtle py-1">
              <p class="m-0 px-4 py-0.5 text-xs font-bold uppercase text-warning">Unknown outcomes</p>
              <For each={unknownRows(props.view)}>
                {(row) => (
                  <div class="flex gap-3 px-4 py-0.5 text-sm">
                    <span class="w-[7.5rem] shrink-0 text-muted">{row.label}</span>
                    <span class="min-w-0 flex-1 wrap-words text-warning">{row.value}</span>
                  </div>
                )}
              </For>
            </div>
          </Show>
          <Show when={errorRows(props.view).length}>
            <div class="border-t border-line-subtle py-1">
              <p class="m-0 px-4 py-0.5 text-xs font-bold uppercase text-danger">Errors</p>
              <For each={errorRows(props.view)}>
                {(row) => (
                  <div class="flex gap-3 px-4 py-0.5 font-mono text-xs">
                    <span class="w-[7.5rem] shrink-0 truncate text-muted">{row.label}</span>
                    <span class={row.tone === "danger" ? "min-w-0 flex-1 wrap-words text-danger" : "min-w-0 flex-1 wrap-words text-warning"}>
                      {row.value}
                    </span>
                  </div>
                )}
              </For>
            </div>
          </Show>
          <div class="border-t border-line-subtle py-1">
            <p class="m-0 px-4 py-0.5 text-xs font-bold uppercase text-muted">Steps</p>
            <StepList view={props.view} now={now()} />
          </div>
          <div class="border-t border-line-subtle py-1">
            <p class="m-0 px-4 py-0.5 text-xs font-bold uppercase text-muted">Waiting on you</p>
            <DecisionPrompt view={props.view} now={now()} onDecision={props.onDecision} onAdditionalInput={props.onAdditionalInput} />
          </div>
          <Show when={evidenceRows(props.view).length}>
            <div class="border-t border-line-subtle py-1">
              <p class="m-0 px-4 py-0.5 text-xs font-bold uppercase text-muted">Evidence</p>
              <For each={evidenceRows(props.view)}>
                {(row) => <p data-evidence-id={row.label} title={row.label} class="m-0 px-4 py-0.5 font-mono text-xs wrap-words text-subtle">{row.label}</p>}
              </For>
            </div>
          </Show>
          <Show when={stalled().length}>
            <p class="m-0 border-t border-line-subtle px-4 py-1 text-xs text-subtle">
              {stalled().map((step) => step.stepId).join(", ")} have reported nothing for a while. Nothing is assumed about them.
            </p>
          </Show>
          <Show when={props.view.status === "waiting_snapshot"}>
            <Empty>The operation record has not loaded yet.</Empty>
          </Show>
        </div>
      </Show>
    </section>
  );
}
