import { For, Show, createEffect, createMemo, createSignal, createUniqueId } from "solid-js";
import { Button } from "../../ui/Button";
import { Empty } from "../../ui/parts";
import { Confirm } from "../../ui/Confirm";
import { additionalInputCallbackPayload, decisionCallbackPayload, type AdditionalInputCallbackPayload, type DecisionCallbackPayload } from "../bridge";
import { decisionDetailRows, decisionRows, type DecisionRow } from "../present";
import type { OperationView } from "../types";

/**
 * What a person is being asked, with the content they are approving.
 *
 * The rules this component exists to keep: an approval shows its concrete target, content
 * and window before any answer is offered; an approval with no preview ("approving blind")
 * is not offered at all; a question for facts is a text box, never a grant; and every answer
 * leaves through a callback that carries an intent, so the trusted bridge — which knows who
 * is answering — is the only thing that records a decision.
 */
export function DecisionPrompt(props: {
  view: OperationView;
  now: number;
  onDecision?: (payload: DecisionCallbackPayload) => void | Promise<void>;
  onAdditionalInput?: (payload: AdditionalInputCallbackPayload) => void | Promise<void>;
}) {
  const rows = () => decisionRows(props.view, props.now);
  // `decisionRows()` rebuilds fresh row objects on every view or clock change, and `<For>` keys
  // by reference — keying on the object would remount every card (and its typed answer, open
  // confirmation and pending guard) on every tick. Key on the stable `decisionId` string instead,
  // and look the current row up inside the child so it stays mounted while the id is present.
  const ids = createMemo(() => rows().map((row) => row.decisionId), undefined, { equals: (a, b) => a.length === b.length && a.every((id, i) => id === b[i]) });
  const rowById = (decisionId: string) => rows().find((row) => row.decisionId === decisionId)!;
  return (
    <Show when={rows().length} fallback={<Empty>Nothing is waiting on you.</Empty>}>
      <For each={ids()}>{(decisionId) => <Decision view={props.view} row={rowById(decisionId)} onDecision={props.onDecision} onAdditionalInput={props.onAdditionalInput} />}</For>
    </Show>
  );
}

function Decision(props: {
  view: OperationView;
  row: DecisionRow;
  onDecision?: (payload: DecisionCallbackPayload) => void | Promise<void>;
  onAdditionalInput?: (payload: AdditionalInputCallbackPayload) => void | Promise<void>;
}) {
  const [answer, setAnswer] = createSignal("");
  const [confirming, setConfirming] = createSignal(false);
  const [pending, setPending] = createSignal(false);
  const [error, setError] = createSignal<string>();
  const inputId = `decision-${createUniqueId()}`;
  const approval = () => props.row.decisionClass !== "additional_input";
  /** A write or a destroy gets the dialog, whatever the request claimed it was. */
  const needsCare = () => props.row.effect === "write" || props.row.effect === "destroy";
  createEffect(() => {
    if (confirming() && !props.row.answerable) setConfirming(false);
  });
  const run = async (action: (() => void | Promise<void>) | undefined) => {
    if (!action || pending()) return;
    setPending(true);
    setError(undefined);
    try {
      await action();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setPending(false);
    }
  };
  const submit = (outcome: "granted" | "denied") => {
    const payload = decisionCallbackPayload(props.row, outcome);
    if (payload) return run(() => props.onDecision?.(payload));
  };
  const sendFacts = () => {
    const payload = additionalInputCallbackPayload(props.row, answer());
    if (payload) return run(() => props.onAdditionalInput?.(payload));
  };

  return (
    <div class="relative border-t border-line-subtle first:border-t-0" data-component="operation-decision">
      <div class="flex items-start gap-2 px-4 py-2">
        <div class="min-w-0 flex-1">
          <div class="flex items-center gap-2 text-xs uppercase text-subtle">
            <span>{props.row.decisionLabel}</span>
            <span class="tabular-nums">{props.row.expiryLabel}</span>
            <Show when={!props.row.answerable}>
              <span class="text-warning">not answerable</span>
            </Show>
          </div>
          <p class="m-0 mt-1 text-sm leading-[20px] wrap-words text-fg">{props.row.question}</p>
          <Show when={props.row.preview}>
            <pre class="op-args m-0 mt-1 max-h-40 overflow-auto whitespace-pre-wrap font-mono text-xs text-muted">{props.row.preview}</pre>
          </Show>
          <div class="mt-1 flex flex-wrap gap-x-3 gap-y-0.5 font-mono text-xs text-subtle">
            <span>{props.row.subject}</span>
            <Show when={props.row.digest}>{(value) => <span>{value()}</span>}</Show>
          </div>
          <Show when={props.row.unavailable}>
            <p class="m-0 mt-1 text-xs text-warning">{props.row.unavailable}</p>
          </Show>
        </div>
        <Show when={props.row.answerable}>
          <div class="op-actions flex shrink-0 items-center gap-1">
            <Show
              when={approval()}
              fallback={
                <form class="op-actions flex items-center gap-1" onSubmit={(event) => { event.preventDefault(); void sendFacts(); }}>
                  <label for={inputId} class="sr-only">Answer {props.row.question}</label>
                  <input
                    id={inputId}
                    class="h-6 w-[14rem] rounded-[2px] border border-input-line bg-input px-1.5 text-sm text-fg"
                    placeholder="Answer"
                    value={answer()}
                    onInput={(e) => setAnswer(e.currentTarget.value)}
                  />
                  <Button type="submit" variant="primary" size="sm" disabled={!answer().trim() || pending()}>
                    {pending() ? "Sending" : "Send"}
                  </Button>
                </form>
              }
            >
              <Button size="sm" disabled={pending()} onClick={() => void submit("denied")}>
                Deny
              </Button>
              <Button
                variant="primary"
                size="sm"
                disabled={pending()}
                onClick={() => (needsCare() ? setConfirming(true) : void submit("granted"))}
              >
                Approve
              </Button>
            </Show>
          </div>
        </Show>
      </div>
      <Show when={error()}>{(message) => <p role="alert" class="m-0 px-4 pb-2 text-xs text-danger">{message()}</p>}</Show>
      <Confirm
        open={confirming()}
        title={props.row.question}
        danger="Approve and apply"
        body={
          <div class="flex flex-col gap-1">
            <For each={decisionDetailRows(props.row)}>
              {(detail) => (
                <div class="flex gap-3">
                  <span class="w-[7.5rem] shrink-0 text-muted">{detail.label}</span>
                  <span class="min-w-0 flex-1 wrap-words">{detail.value}</span>
                </div>
              )}
            </For>
          </div>
        }
        onNo={() => setConfirming(false)}
        disabled={!props.row.answerable || pending()}
        onYes={() => {
          if (!props.row.answerable || pending()) return;
          setConfirming(false);
          void submit("granted");
        }}
      />
    </div>
  );
}
