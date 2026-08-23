import { createHmac, timingSafeEqual } from "node:crypto";

/**
 * A one-minute, single-purpose proof that the server verified a passkey.
 *
 * Auth.js exposes every credentials provider at a public callback URL, so a
 * provider that accepted `{ email }` would let anyone POST their way into any
 * account. The provider therefore accepts only this: an HMAC over the email and
 * an expiry, keyed by AUTH_SECRET, which the browser cannot produce.
 *
 * Kept free of `server-only` and `next/headers` so it can be unit-tested; the
 * WebAuthn ceremony that calls it stays in `lib/passkey.ts`.
 */
function assertionKey() {
  const secret = process.env.AUTH_SECRET;
  if (!secret) throw new Error("AUTH_SECRET is required to sign a passkey assertion");
  return secret;
}

export function signAssertion(email: string, now = Date.now()): string {
  const exp = now + 60_000;
  const body = `${email.toLowerCase()}.${exp}`;
  const mac = createHmac("sha256", assertionKey()).update(body).digest("base64url");
  return `${body}.${mac}`;
}

/** The email, if this really was signed here and has not expired. */
export function verifyAssertion(assertion: string, now = Date.now()): string | null {
  // Cut from the end: an email can contain any number of dots, but the expiry is
  // digits and the mac is base64url, so the last two dots are the separators.
  const macAt = assertion.lastIndexOf(".");
  if (macAt <= 0) return null;
  const expAt = assertion.lastIndexOf(".", macAt - 1);
  if (expAt <= 0) return null;
  const email = assertion.slice(0, expAt);
  const exp = assertion.slice(expAt + 1, macAt);
  const mac = assertion.slice(macAt + 1);
  const expected = createHmac("sha256", assertionKey()).update(`${email}.${exp}`).digest("base64url");
  const a = Buffer.from(mac);
  const b = Buffer.from(expected);
  if (a.length !== b.length || !timingSafeEqual(a, b)) return null;
  if (!/^\d+$/.test(exp) || Number(exp) < now) return null;
  return email;
}
