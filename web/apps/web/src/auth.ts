import NextAuth, { type NextAuthConfig } from "next-auth";
import GitHub from "next-auth/providers/github";
import Google from "next-auth/providers/google";
import MicrosoftEntraID from "next-auth/providers/microsoft-entra-id";

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
    /** `owner` is the account namespace the browse API authorises against, so it has
     *  to survive on the token — the session is the only place pages read it from. */
    async jwt({ token, profile }) {
      if (profile) {
        token.owner =
          (profile as { login?: string }).login ??
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
