import "server-only";
import { cache } from "react";
import { redirect } from "next/navigation";
import { auth } from "@/auth";

export type Session = {
  user: {
    name: string;
    email: string;
    /** The namespace this person owns. Absent until they pick a handle — pages
     *  that build a URL from it must send them to /welcome instead of guessing. */
    username?: string;
    /** Kept for the pages still reading it; the same value as `username`. */
    owner: string;
  };
} | null;

/** The single place pages ask *who is signed in*. Authentication only.
 *
 *  This answers identity, never permission. Nothing here may be used to decide
 *  whether a user can see a repo, an environment or a registry: those depend on
 *  the resource, not the caller, and are the backend's to answer. A page that
 *  needs an access decision must ask for the resource and handle the refusal. */
// `cache()`: the shell, the layout and the page each ask, and each ask decrypted the session
// cookie again. One decrypt per request.
export const getSession = cache(async function getSession(): Promise<Session> {
  const session = await auth();
  const user = session?.user;

  if (user?.email) {
    return {
      user: {
        name: user.name ?? user.email,
        email: user.email,
        username: user.username,
        // Falls back to the email local-part only so pages render before a handle
        // is chosen; it is not a namespace and nothing may be created under it.
        owner: user.username ?? user.email.split("@")[0],
      },
    };
  }

  return null;
});

/** Identity or the sign-in page, for a route with nothing else to ask the api about.
 *
 *  Still authentication only — see above. The pages that use it show a placeholder
 *  either way; the guard is only there so a signed-out visitor sees the product's
 *  front door instead of the inside of a namespace they are not in.
 *  A page that fetches anything uses guardRepo/guardImage instead: they carry the
 *  token and let the api decide access. */
export async function requireSession(): Promise<NonNullable<Session>> {
  const session = await getSession();
  if (!session) redirect("/login");
  if (!session.user.username) redirect("/welcome");
  return session;
}
