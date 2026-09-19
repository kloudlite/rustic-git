/**
 * O09 — what the renderer may ask the trusted bridge to do, and what it may never do.
 *
 * The renderer is a view. It cannot mint a grant: `RecordedDecision` (actor, tenant,
 * session, payload digest, policy source, expiry, outcome) is written only by the
 * authenticated user UI or the trusted policy adapter, and only O08's bridge holds the
 * credential that proves who is answering. So these payloads carry an *intent* — the
 * decision being answered, the revision it was raised at, and the person's answer — and
 * nothing that could pass for a record. There is deliberately no actor, tenant, session,
 * policy source, record ID, or expiry in them.
 *
 * Wiring (O08, per `docs/superpowers/plans/2026-09-18-harness-operation-integration.md`):
 * the desktop main process attaches the login credential to authenticated operation
 * control requests; the renderer never sees it. `loadSnapshot` is the authenticated
 * `inspect`, `loadEvents` is `GET events?after=`, and `decide` / `answer` / `cancel` are
 * the decision and cancel controls. A model's text, an event's `summary`, and the
 * tool-call arguments are all untrusted input on this boundary.
 */
import type { OperationEvent, OperationSnapshot } from "../../../bench/src/operations/contracts.ts";
import { CANCEL_REQUEST_EDGES, isTerminalOperation, type DecisionPresentation, type OperationView } from "./types.ts";

export type DecisionOutcome = "granted" | "denied";

/**
 * The minimum a decision must expose to be answered. `DecisionPresentation` and the panel's
 * own `DecisionRow` both satisfy it, so neither has to be re-shaped into a fake record to
 * build a callback.
 */
export type AnswerableDecision = Pick<
  DecisionPresentation,
  "decisionId" | "operationId" | "stepId" | "decisionClass" | "revision" | "answerable"
>;

/** The person's answer to an authorization or preference. An intent, never a record. */
export type DecisionCallbackPayload = {
  operationId: string;
  stepId: string;
  decisionId: string;
  /** The revision the decision was raised at, so a stale answer is refused rather than applied. */
  expectedRevision: number;
  outcome: DecisionOutcome;
};

/** The person's answer to a question for facts. `additional_input` can never resolve an approval. */
export type AdditionalInputCallbackPayload = {
  operationId: string;
  stepId: string;
  decisionId: string;
  expectedRevision: number;
  inputs: { answer: string };
};

export type CancelCallbackPayload = {
  operationId: string;
  expectedRevision: number;
};

/**
 * An approval payload, or undefined when the decision cannot be answered: it has closed,
 * expired, or it is a question for facts (typing facts is not approving anything).
 */
export function decisionCallbackPayload(
  decision: AnswerableDecision,
  outcome: DecisionOutcome,
): DecisionCallbackPayload | undefined {
  if (!decision.answerable) return undefined;
  if (decision.decisionClass === "additional_input") return undefined;
  return {
    operationId: decision.operationId,
    stepId: decision.stepId,
    decisionId: decision.decisionId,
    expectedRevision: decision.revision,
    outcome,
  };
}

/** An additional-input payload, or undefined when the decision is not a question for facts. */
export function additionalInputCallbackPayload(
  decision: AnswerableDecision,
  answer: string,
): AdditionalInputCallbackPayload | undefined {
  if (!decision.answerable) return undefined;
  if (decision.decisionClass !== "additional_input") return undefined;
  const trimmed = answer.trim();
  if (!trimmed) return undefined;
  return {
    operationId: decision.operationId,
    stepId: decision.stepId,
    decisionId: decision.decisionId,
    expectedRevision: decision.revision,
    inputs: { answer: trimmed },
  };
}

/** A cancel request, or undefined when the durable state does not allow one. */
export function cancelCallbackPayload(view: OperationView): CancelCallbackPayload | undefined {
  if (view.resync || view.controlIntent?.cancelRequestedAt !== undefined) return undefined;
  const state = view.state;
  if (state === undefined || isTerminalOperation(state)) return undefined;
  if (!CANCEL_REQUEST_EDGES.some((edge) => edge.from === state)) return undefined;
  return { operationId: view.operationId, expectedRevision: view.revision };
}

/**
 * The hooks O08 wires. All of them are authenticated at the bench boundary; none of them
 * accepts identity, scope, a policy source, or an approval value from this side.
 */
export type OperationUiBridge = {
  /** Authenticated `inspect`: the durable record, including everything events do not carry. */
  loadSnapshot(operationId: string): Promise<OperationSnapshot>;
  /** Authenticated `GET events?after=N`: durable events after the cursor the view applied. */
  loadEvents(operationId: string, afterSequence: number): Promise<OperationEvent[]>;
  /** A change notification. It carries no sensitive detail; the fetch above does. */
  watch?(operationId: string, onChanged: (lastSequence: number) => void): () => void;
  /** Record the person's answer through the trusted path, then resume the operation. */
  decide?(payload: DecisionCallbackPayload): Promise<void>;
  /** Deliver additional input for a question that asks for facts. */
  answer?(payload: AdditionalInputCallbackPayload): Promise<void>;
  cancel?(payload: CancelCallbackPayload): Promise<void>;
  /**
   * Hand a fresh snapshot after a resync request. `need: "snapshot"` asks for the record;
   * `need: "events_after"` asks for the events after `afterSequence`.
   */
  onResyncRequested?(request: { operationId: string; afterSequence: number; need: "snapshot" | "events_after" }): void;
};

/** Renderer-side stream lifecycle. A future preload bridge can implement this without changing the store. */
export type OperationRendererBridge = OperationUiBridge & {
  onConnection?(onConnected: (connected: boolean) => void): () => void;
};
