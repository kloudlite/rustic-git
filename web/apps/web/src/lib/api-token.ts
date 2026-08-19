import "server-only";
import { headers } from "next/headers";
import { getToken } from "next-auth/jwt";

/**
 * The api server's token for the signed-in person.
 *
 * Read from the encrypted session JWT rather than from `auth()`: the session
 * object is what `/api/auth/session` returns to the browser, so a bearer
 * credential placed on it would be readable by any client-side script. The JWT
 * is encrypted with AUTH_SECRET and only the server can open it.
 */
export async function apiToken(): Promise<string | undefined> {
  const token = await getToken({
    req: new Request("http://n", { headers: await headers() }),
    secret: process.env.AUTH_SECRET,
    salt: process.env.AUTH_URL?.startsWith("https")
      ? "__Secure-authjs.session-token"
      : "authjs.session-token",
    secureCookie: process.env.AUTH_URL?.startsWith("https"),
  });
  return token?.apiToken;
}
