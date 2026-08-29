import "server-only";
import { cookies, headers } from "next/headers";
import { relyingPartyFor } from "@/lib/relying-party";

/**
 * WebAuthn, verified here rather than by an Auth.js adapter.
 *
 * The stock Passkey provider needs an Adapter, which means a database connection
 * in the web app — the one thing this app deliberately does not have. So the
 * ceremony runs in server actions, the credential is stored through the api
 * server like every other record, and the verified result is handed to Auth.js as
 * a signed assertion (below) rather than as a bare email.
 */

/** See `relyingPartyFor`: AUTH_URL decides, the request host may only agree with it. */
export async function relyingParty() {
  const h = await headers();
  return relyingPartyFor(process.env.AUTH_URL, h.get("x-forwarded-host") ?? h.get("host") ?? undefined);
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
