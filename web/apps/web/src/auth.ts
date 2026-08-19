import NextAuth, { type NextAuthConfig } from "next-auth";
import GitHub from "next-auth/providers/github";
import Google from "next-auth/providers/google";
import MicrosoftEntraID from "next-auth/providers/microsoft-entra-id";
import Credentials from "next-auth/providers/credentials";

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

export const { handlers, auth, signIn, signOut } = NextAuth({
  providers: providers(),
  /* JWT sessions: no database needed to sign in. An adapter can be added later
     without changing any caller of auth(). */
  session: { strategy: "jwt" },
  pages: { signIn: "/login", newUser: "/signup", error: "/login" },
  callbacks: {
    /** `owner` is an identity claim — the namespace this user is known by — and
     *  nothing more. It is not a grant. Whether this user may read a given repo is
     *  decided by the backend (src/auth.rs `authorize`), which checks the repo's
     *  own ownership and visibility; a claim on a token cannot answer that, because
     *  the token is issued before any repo is named. Never branch access on it. */
    async jwt({ token, profile }) {
      /* `profile` exists only for OAuth. Credentials sign-in has none, so derive
         from the email — the same rule the OAuth path falls back to. */
      if (profile || !token.owner) {
        token.owner =
          (profile as { login?: string } | undefined)?.login ??
          token.email?.split("@")[0] ??
          token.sub ??
          "user";
      }
      return token;
    },
    async session({ session, token }) {
      if (session.user) {
        session.user.owner = (token.owner as string) ?? "";
      }
      return session;
    },
  },
});
