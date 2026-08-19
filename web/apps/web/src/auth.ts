import NextAuth, { type NextAuthConfig } from "next-auth";
import GitHub from "next-auth/providers/github";
import Google from "next-auth/providers/google";
import MicrosoftEntraID from "next-auth/providers/microsoft-entra-id";
import Credentials from "next-auth/providers/credentials";
import { signIn as apiSignIn } from "@/lib/api";

/** Email + shared password, for a deployment that has no OAuth provider yet.
 *  Registered only when both halves are configured, so it cannot exist by
 *  accident, and it is real authentication rather than a bypass: the address
 *  must be on the allowlist AND the password must match. */
function previewCredentials() {
  const allowed = (process.env.AUTH_ALLOWED_EMAILS ?? "")
    .split(",")
    .map((e) => e.trim().toLowerCase())
    .filter(Boolean);
  const password = process.env.AUTH_SHARED_PASSWORD ?? "";
  if (allowed.length === 0 || password.length === 0) return null;

  return Credentials({
    id: "credentials",
    name: "Email and password",
    credentials: { email: {}, password: {} },
    authorize(raw) {
      const email = String(raw?.email ?? "").trim().toLowerCase();
      const given = String(raw?.password ?? "");
      if (!allowed.includes(email)) return null;
      /* Length-independent compare is overkill for a shared preview password, but
         a plain === leaks length through timing and costs nothing to avoid. */
      if (given.length !== password.length) return null;
      let diff = 0;
      for (let i = 0; i < password.length; i++) diff |= given.charCodeAt(i) ^ password.charCodeAt(i);
      if (diff !== 0) return null;
      return { id: email, email, name: email.split("@")[0] };
    },
  });
}

/** A provider is only registered when its credentials are present. Registering one
 *  without them makes Auth.js fail at request time with an opaque error; leaving it
 *  out means the button can be hidden and the rest of sign-in still works. */
function providers() {
  const list: NextAuthConfig["providers"] = [];

  if (process.env.AUTH_GITHUB_ID && process.env.AUTH_GITHUB_SECRET) {
    list.push(GitHub({ clientId: process.env.AUTH_GITHUB_ID, clientSecret: process.env.AUTH_GITHUB_SECRET }));
  }
  if (process.env.AUTH_GOOGLE_ID && process.env.AUTH_GOOGLE_SECRET) {
    list.push(Google({ clientId: process.env.AUTH_GOOGLE_ID, clientSecret: process.env.AUTH_GOOGLE_SECRET }));
  }
  const preview = previewCredentials();
  if (preview) list.push(preview);

  if (process.env.AUTH_MICROSOFT_ENTRA_ID_ID && process.env.AUTH_MICROSOFT_ENTRA_ID_SECRET) {
    list.push(
      MicrosoftEntraID({
        clientId: process.env.AUTH_MICROSOFT_ENTRA_ID_ID,
        clientSecret: process.env.AUTH_MICROSOFT_ENTRA_ID_SECRET,
        issuer: process.env.AUTH_MICROSOFT_ENTRA_ID_ISSUER,
      }),
    );
  }
  return list;
}

/** Which providers are actually usable, for the UI to read. Server-side only. */
/** Whether email + password sign-in is available on this deployment. */
export const passwordSignIn = Boolean(
  process.env.AUTH_ALLOWED_EMAILS?.trim() && process.env.AUTH_SHARED_PASSWORD,
);

export const enabledProviders = {
  github: Boolean(process.env.AUTH_GITHUB_ID && process.env.AUTH_GITHUB_SECRET),
  google: Boolean(process.env.AUTH_GOOGLE_ID && process.env.AUTH_GOOGLE_SECRET),
  "microsoft-entra-id": Boolean(
    process.env.AUTH_MICROSOFT_ENTRA_ID_ID && process.env.AUTH_MICROSOFT_ENTRA_ID_SECRET,
  ),
};

export const { handlers, auth, signIn, signOut, unstable_update: updateSession } = NextAuth({
  providers: providers(),
  /* JWT sessions: no database needed to sign in. An adapter can be added later
     without changing any caller of auth(). */
  session: { strategy: "jwt" },
  pages: { signIn: "/login", newUser: "/signup", error: "/login" },
  callbacks: {
    /** The identity the rest of the app runs on comes from the api server, not
     *  from the provider: it records the person, decides their handle, and mints
     *  the token every later call presents. Auth.js only carries it.
     *
     *  `username` is deliberately allowed to be absent — that is what "has not
     *  picked a handle yet" looks like, and the app routes on it. */
    async jwt({ token, trigger, session: update }) {
      // A username claimed mid-session: the client asks for an update rather than
      // signing out and back in.
      if (trigger === "update" && update) {
        // `update` is whatever updateSession() was handed; it is not a Session,
        // and the api token must never become part of one.
        const patch = update as { apiToken?: string; user?: { username?: string } };
        if (patch.apiToken) token.apiToken = patch.apiToken;
        if (patch.user?.username) token.username = patch.user.username;
        return token;
      }

      // Once, on sign-in. `token.apiToken` already set means this is a later call
      // on the same session and the api server has nothing new to say.
      if (!token.apiToken && token.email) {
        const r = await apiSignIn(token.email, (token.name as string) ?? token.email);
        if (r.ok) {
          token.apiToken = r.value.token ?? undefined;
          token.username = r.value.user.username;
        } else {
          // Signing in must not fail because the directory is briefly down. The
          // session exists with no api token, and the pages that need one say so.
          console.error("sign-in: api server said", r.kind, r.message);
        }
      }
      return token;
    },
    /** The api token is deliberately NOT copied here. This object is what
     *  `/api/auth/session` serves to the browser, so anything on it is readable
     *  by client-side script — and the token is a bearer credential for the api
     *  server. It stays in the encrypted JWT; server code reads it with
     *  `apiToken()` from lib/api-token.ts. */
    async session({ session, token }) {
      if (session.user) {
        session.user.username = token.username as string | undefined;
      }
      return session;
    },
  },
});
