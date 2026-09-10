import { call } from "./client";
import type { SignIn } from "./client";

/**
 * Sign-in and the handle claim.
 */
/** Records the person and returns their token. Called once, at sign-in. */
export function signIn(email: string, name: string) {
  return call<SignIn>("/v1/users", {
    method: "POST",
    asUser: email,
    body: JSON.stringify({ email, name }),
  });
}

/** Mint a magic sign-in link for `email`. Peer-authenticated: nobody is signed in yet. The
 *  token comes back once and goes into the email; the api keeps only its hash. `clientIp` is
 *  the browser's address as the ingress reported it — the api's per-address bucket on this
 *  route would otherwise see every request as coming from this pod. */
export function requestSignInLink(email: string, clientIp?: string) {
  return call<{ token: string; email: string }>("/v1/signin/email", {
    method: "POST",
    asUser: email,
    headers: clientIp ? { "x-real-ip": clientIp } : undefined,
    body: JSON.stringify({ email }),
  });
}

/** Spend a link. 404 for spent, expired or invented alike. */
export function redeemSignInLink(token: string) {
  return call<{ email: string }>(`/v1/signin/email/${encodeURIComponent(token)}`, {
    method: "POST",
    asUser: "-",
  });
}

/** Claims a handle. Returns a NEW token: the old one asserts they have none. */
export function claimUsername(token: string, username: string) {
  return call<SignIn>("/v1/users/username", {
    method: "POST",
    token,
    body: JSON.stringify({ username }),
  });
}
