import "server-only";
import { cache } from "react";
import { headers } from "next/headers";
import { getToken } from "next-auth/jwt";
import { secureCookies, sessionCookie } from "@/auth";

/**
 * The api server's token for the signed-in person.
 *
 * Read from the encrypted session JWT rather than from `auth()`: the session
 * object is what `/api/auth/session` returns to the browser, so a bearer
 * credential placed on it would be readable by any client-side script. The JWT
 * is encrypted with AUTH_SECRET and only the server can open it.
 */
/** For a server action: the token, or the one answer every action gives without it. A
 *  `string` is a token; anything else is the state to return as-is. */
export async function tokenOr(): Promise<string | { error: string }> {
  return (await apiToken()) ?? { error: "Your session has expired. Sign in again." };
}

export const apiToken = cache(async function apiToken(): Promise<string | undefined> {
  const token = await getToken({
    req: new Request("http://n", { headers: await headers() }),
    secret: process.env.AUTH_SECRET,
    cookieName: sessionCookie,
    // The salt is the cookie name: that is the key-derivation input Auth.js used.
    salt: sessionCookie,
    secureCookie: secureCookies,
  });
  return token?.apiToken;
});
