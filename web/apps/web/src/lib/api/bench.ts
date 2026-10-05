import { call } from "./client";

/** `POST /v1/bench/session`: once the bench is up, a token and the gateway to open its terminal
 *  through; while it is still waking, `state` names the phase instead. */
export type BenchSession = { id: string; token: string; gateway: string; expires_at: string } | { state: string };

export function benchSession(token: string) {
  return call<BenchSession>("/v1/bench/session", { method: "POST", token });
}
