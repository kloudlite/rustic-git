import "server-only";
import { cache } from "react";
import { redirect, notFound } from "next/navigation";
import { auth } from "@/auth";
import { apiToken } from "@/lib/api-token";

export type Session = {
  user: {
    name: string;
    email: string;
    /** The namespace this person owns. Absent until they pick a handle — pages
     *  that build a URL from it must send them to /welcome instead of guessing. */
    username?: string;
    /** Kept for the pages still reading it; the same value as `username`. */
    owner: string;
    /** The `superadmin` JWT claim — gates whether `/superadmin` exists for this person. Not an
     *  access decision: /v1 re-checks the claim on every call the superadmin area makes. */
    superadmin: boolean;
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
        superadmin: user.superadmin ?? false,
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
export async function requireSession(next?: string): Promise<NonNullable<Session>> {
  const session = await getSession();
  if (!session) redirect(loginFor(next));
  if (!session.user.username) redirect("/welcome");
  return session;
}

/** Identity AND an api token, or the sign-in page with the way back to `next` — a deep link
 *  opened signed-out used to land on `/` after sign-in, with the link lost. Still no access
 *  decision: the token is only what the page hands to the api, which answers that. */
export async function requireToken(next: string): Promise<{ session: NonNullable<Session>; token: string }> {
  const session = await requireSession(next);
  const token = await apiToken();
  if (!token) redirect(loginFor(next));
  return { session, token };
}

/** Identity, a token, and the superadmin claim — or 404 for anyone without it.
 *
 *  Still not an access decision: /v1 re-checks the claim on every call this area makes. This only
 *  decides whether the page exists for this person, and 404 rather than 403 because whether a
 *  superadmin area is here is not a non-admin's to learn. */
export async function requireSuperadmin(next: string) {
  const { session, token } = await requireToken(next);
  if (!session.user.superadmin) notFound();
  return { session, token };
}

function loginFor(next?: string) {
  return next ? `/login?next=${encodeURIComponent(next)}` : "/login";
}
