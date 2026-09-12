import "server-only";
import { createHmac, timingSafeEqual } from "node:crypto";

/**
 * The one shape a session `update` may carry, and the proof that the server minted it.
 *
 * Auth.js's `update` trigger is reachable from the browser: `POST /api/auth/session` hands
 * whatever JSON it was given straight to the `jwt` callback. A marker field is therefore no
 * marker at all — a client can send `_server: true` — so the api token in a patch is accepted
 * only against `_sig`, an HMAC over the token keyed by AUTH_SECRET, which only this process
 * can produce. `user.username` stays unsigned: it is re-derived from the api's own answer on
 * the next sign-in and grants nothing on its own.
 */
export type SessionPatch = {
  apiToken?: string;
  user?: { username?: string };
  _server: true;
  _sig?: string;
};

function sign(apiToken: string): string {
  const secret = process.env.AUTH_SECRET;
  if (!secret) throw new Error("AUTH_SECRET is required to sign a session patch");
  return createHmac("sha256", secret).update(apiToken).digest("base64url");
}

export function serverPatch(p: { apiToken?: string; user?: { username?: string } }): SessionPatch {
  return { ...p, _server: true, ...(p.apiToken ? { _sig: sign(p.apiToken) } : {}) };
}

/** True only for a patch this process minted, carrying the token it was minted for. */
export function isServerPatch(p: Partial<SessionPatch>): boolean {
  if (p._server !== true || !p.apiToken || !p._sig) return false;
  const a = Buffer.from(p._sig);
  const b = Buffer.from(sign(p.apiToken));
  return a.length === b.length && timingSafeEqual(a, b);
}
