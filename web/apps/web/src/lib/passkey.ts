import "server-only";
import { cookies } from "next/headers";
import { createHmac, timingSafeEqual } from "node:crypto";

/**
 * WebAuthn, verified here rather than by an Auth.js adapter.
 *
 * The stock Passkey provider needs an Adapter, which means a database connection
 * in the web app — the one thing this app deliberately does not have. So the
 * ceremony runs in server actions, the credential is stored through the api
 * server like every other record, and the verified result is handed to Auth.js as
 * a signed assertion (below) rather than as a bare email.
 */

/** The relying party is the site, and WebAuthn binds credentials to it: a
 *  credential made for one rpID cannot be used on another. Derived from AUTH_URL
 *  so it always matches where the browser actually is. */
export function relyingParty() {
  const url = new URL(process.env.AUTH_URL ?? "http://localhost:3000");
  return { rpID: url.hostname, origin: url.origin, rpName: "kloudlite" };
}

const CHALLENGE_COOKIE = "webauthn-challenge";

/** The challenge is what makes a signature fresh rather than replayable, so it is
 *  kept where the browser cannot read or set it: an httpOnly cookie, five minutes,
 *  cleared the moment it is used. */
export async function rememberChallenge(challenge: string) {
  const jar = await cookies();
  jar.set(CHALLENGE_COOKIE, challenge, {
    httpOnly: true,
    sameSite: "strict",
    secure: relyingParty().origin.startsWith("https"),
    path: "/",
    maxAge: 300,
  });
}

export async function takeChallenge(): Promise<string | undefined> {
  const jar = await cookies();
  const v = jar.get(CHALLENGE_COOKIE)?.value;
  // Single use: a challenge that has been spent must not verify a second response.
  if (v) jar.delete(CHALLENGE_COOKIE);
  return v;
}

/**
 * A one-minute, single-purpose proof that the server verified a passkey.
 *
 * Auth.js exposes every credentials provider at a public callback URL, so a
 * provider that accepted `{ email }` would let anyone POST their way into any
 * account. The provider therefore accepts only this: an HMAC over the email and
 * an expiry, keyed by AUTH_SECRET, which the browser cannot produce.
 */
function assertionKey() {
  const secret = process.env.AUTH_SECRET;
  if (!secret) throw new Error("AUTH_SECRET is required to sign a passkey assertion");
  return secret;
}

export function signAssertion(email: string): string {
  const exp = Date.now() + 60_000;
  const body = `${email.toLowerCase()}.${exp}`;
  const mac = createHmac("sha256", assertionKey()).update(body).digest("base64url");
  return `${body}.${mac}`;
}

/** The email, if this really was signed here and has not expired. */
export function verifyAssertion(assertion: string): string | null {
  const parts = assertion.split(".");
  if (parts.length !== 3) return null;
  const [email, exp, mac] = parts;
  const expected = createHmac("sha256", assertionKey()).update(`${email}.${exp}`).digest("base64url");
  const a = Buffer.from(mac);
  const b = Buffer.from(expected);
  if (a.length !== b.length || !timingSafeEqual(a, b)) return null;
  if (!Number.isFinite(Number(exp)) || Number(exp) < Date.now()) return null;
  return email;
}
