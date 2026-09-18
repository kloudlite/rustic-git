export type ExchangeState =
  | "sent"
  | "queued"
  | "running"
  | "working"
  | "pending"
  | "done"
  | "failed"
  | "blocked"
  | "expired"
  | "cancelled"
  | "note";

export const isActiveExchangeState = (state: string): boolean => state === "queued" || state === "running";

export const isTerminalExchangeState = (state: string): boolean =>
  state === "done" || state === "failed" || state === "blocked" || state === "expired" || state === "cancelled";

export type RendererExchangeState = "pending" | "working" | "done";

export const rendererExchangeState = (state: string): RendererExchangeState => {
  if (isTerminalExchangeState(state)) return "done";
  if (state === "running" || state === "working") return "working";
  return "pending";
};
