import "server-only";
import { cookies, headers } from "next/headers";

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
 *  credential made for one rpID cannot be used on another, and the origin the
 *  server checks must be byte-for-byte the one the browser used.
 *
 *  Derived from the request's host rather than AUTH_URL, so it is correct however
 *  the app is reached — deployed, direct localhost, or tunnelled to a public host
 *  in a dev environment — without a per-environment env var to keep in sync. The
 *  scheme is fixed by the host (localhost is http, everything else https) instead
 *  of trusting X-Forwarded-Proto, which behind Cloudflare's Flexible SSL arrives
 *  as http even though the browser is on https. */
export async function relyingParty() {
  const h = await headers();
  const raw =
    h.get("x-forwarded-host") ??
    h.get("host") ??
    new URL(process.env.AUTH_URL ?? "http://localhost:3000").host;
  const host = raw.split(",")[0].trim();
  const hostname = host.split(":")[0];
  const proto = hostname === "localhost" || hostname === "127.0.0.1" ? "http" : "https";
  return { rpID: hostname, origin: `${proto}://${host}`, rpName: "kloudlite" };
}

const CHALLENGE_COOKIE = "webauthn-challenge";

/** The challenge is what makes a signature fresh rather than replayable, so it is
 *  kept where the browser cannot read or set it: an httpOnly cookie, five minutes,
 *  cleared the moment it is used. */
export async function rememberChallenge(challenge: string) {
  const jar = await cookies();
  const { origin } = await relyingParty();
  jar.set(CHALLENGE_COOKIE, challenge, {
    httpOnly: true,
    sameSite: "strict",
    secure: origin.startsWith("https"),
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
