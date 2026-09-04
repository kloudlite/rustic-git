import NextAuth, { type NextAuthConfig } from "next-auth";
import GitHub from "next-auth/providers/github";
import Google from "next-auth/providers/google";
import Credentials from "next-auth/providers/credentials";
import { signIn as apiSignIn } from "@/lib/api";
import { verifyAssertion } from "@/lib/assertion";
import { Lockout } from "@/lib/lockout";
import { log } from "@/lib/log";
import { count } from "@/lib/metrics";

const logger = log("web::auth");

/** Every refusal counted under the reason it was refused for — a burst of `bad_password` is
 *  somebody guessing, a burst of `api_unavailable` is our own outage, and a single counter
 *  would make them the same alert. The address is never a label: that is unbounded, and it is
 *  personal data sitting in a metrics store forever. */
function refused(reason: string) {
  count("auth_failures_total", { reason });
  logger.warn("auth.signin.failed", { reason });
}

/** Five wrong preview passwords a minute per account (S-23); see `Lockout` for the ceiling. */
const lockout = new Lockout(5, 60_000);

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
      if (!allowed.includes(email)) {
        refused("not_allowed");
        return null;
      }
      // One secret covers every allow-listed account, so guesses are counted per account and
      // the answer while locked is the same "no" a wrong password gets.
      if (lockout.locked(email)) {
        refused("locked");
        return null;
      }
      /* Length-independent compare is overkill for a shared preview password, but
         a plain === leaks length through timing and costs nothing to avoid. */
      let diff = given.length === password.length ? 0 : 1;
      for (let i = 0; i < password.length; i++) diff |= given.charCodeAt(i) ^ password.charCodeAt(i);
      if (diff !== 0) {
        lockout.fail(email);
        refused("bad_password");
        return null;
      }
      lockout.clear(email);
      return { id: email, email, name: email.split("@")[0] };
    },
  });
}

/** Passkeys, as a credentials provider.
 *
 *  Auth.js publishes every credentials provider at `/api/auth/callback/<id>`, so
 *  whatever this accepts is something anyone can POST. It therefore accepts one
 *  thing: an HMAC signed with AUTH_SECRET, produced only after the server has
 *  verified a WebAuthn signature. A bare email here would be an open door.
 *
 *  The stock Passkey provider is not used because it requires an Adapter, and an
 *  Adapter means a database connection in the browser-facing process. */
function passkeyProvider() {
  return assertionProvider("passkey", "Passkey");
}

/** A magic link, the same way: clicking the link proves possession of the inbox, the server
 *  redeems the token with the api and only then mints the assertion. The email is verified
 *  by the click — there is nothing else to verify. */
function emailLinkProvider() {
  return assertionProvider("email-link", "Email link");
}

function assertionProvider(id: string, name: string) {
  return Credentials({
    id,
    name,
    credentials: { assertion: {} },
    authorize(raw) {
      const assertion = String(raw?.assertion ?? "");
      if (!assertion) {
        refused("no_assertion");
        return null;
      }
      const email = verifyAssertion(assertion);
      if (!email) {
        refused("bad_assertion");
        return null;
      }
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
  // Always available: a passkey needs no configuration, only a browser that has
  // one. Whether anyone HAS one is answered by the browser, not by env vars.
  list.push(passkeyProvider());
  list.push(emailLinkProvider());

  return list;
}

/** Which providers are actually usable, for the UI to read. Server-side only. */
/** Whether email + password sign-in is available on this deployment. */
export const passwordSignIn = Boolean(
  process.env.AUTH_ALLOWED_EMAILS?.trim() && process.env.AUTH_SHARED_PASSWORD,
);

/** Whether a sign-in link can actually be emailed. Without it the email step has nowhere to
 *  go and says so, rather than minting links nobody receives. */
export const emailLinkSignIn = Boolean(process.env.RESEND_API_KEY && process.env.RESEND_FROM);

export const enabledProviders = {
  github: Boolean(process.env.AUTH_GITHUB_ID && process.env.AUTH_GITHUB_SECRET),
  google: Boolean(process.env.AUTH_GOOGLE_ID && process.env.AUTH_GOOGLE_SECRET),
};

/** One decision about the session cookie, made here and read back by
 *  `lib/api-token.ts`. Auth.js would pick the same defaults from AUTH_URL, but
 *  two places deriving the same answer is how they come to differ. */
// AUTH_URL must be set (deploy/kloudlite-git-web.yaml does): behind a TLS proxy the
// request itself looks like http, so an unset AUTH_URL silently drops `Secure` —
// the one failure mode here that is invisible everywhere it does not matter and
// catastrophic in the one place it does. In production that is a refusal, not a
// default: an unset value means a misconfigured rollout, and failing to boot is
// how that gets noticed instead of shipping non-Secure session cookies for a week.
const authUrl = process.env.AUTH_URL ?? "";
// `next build` runs this module to prerender, with NODE_ENV=production and no
// deployment env — so the check is scoped to serving, which is when it is true.
if (process.env.NODE_ENV === "production" && process.env.NEXT_PHASE !== "phase-production-build" && !authUrl) {
  throw new Error("AUTH_URL is required in production (without it the session cookie loses `Secure`)");
}
export const secureCookies = authUrl.startsWith("https");
export const sessionCookie = secureCookies ? "__Secure-authjs.session-token" : "authjs.session-token";

export const { handlers, auth, signIn, signOut, unstable_update: updateSession } = NextAuth({
  providers: providers(),
  /* JWT sessions: no database needed to sign in. An adapter can be added later
     without changing any caller of auth(). */
  // The session cookie lives exactly as long as the api token it carries (12 h, the api's
  // `TTL_SECS`). Nothing extends either: server components cannot set cookies, so a token
  // re-minted during a render was thrown away and minted again on the next one — every render of
  // a day-old session was a `POST /v1/users`. Letting both expire together sends the person back
  // through sign-in once, which is usually silent.
  session: { strategy: "jwt", maxAge: 12 * 60 * 60 },
  useSecureCookies: secureCookies,
  cookies: { sessionToken: { name: sessionCookie } },
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

      // Minted once, at sign-in. The session's `maxAge` above matches the token's life, so it is
      // never refreshed here: a refresh from a server-component render cannot be written back to
      // the cookie, and one that could be would re-run on every render anyway.
      //
      // A token the api refuses before then (rotated secret, revoked user) cannot be detected
      // here — this callback never calls the api with it. The refusal surfaces where the call is
      // made: `lib/api.ts` answers `unauthorized`, the caller redirects to /login?from=expired,
      // and that page offers sign-out. Signing in again lands here and mints a fresh token.
      if (!token.apiToken && token.email) {
        const r = await apiSignIn(token.email, (token.name as string) ?? token.email);
        if (r.ok) {
          token.apiToken = r.value.token ?? undefined;
          token.username = r.value.user.username;
          // Read from the api token's own payload: the api decided it at sign-in, and the web must
          // never decide it. This gates what is SHOWN; every admin action is re-authorized by /v1
          // against the same claim, so a tampered cookie reveals a page and grants nothing.
          token.superadmin = readSuperadmin(r.value.token ?? "");
        } else {
          // Signing in must not fail because the directory is briefly down. The
          // session exists with no api token, and the pages that need one say so.
          count("auth_failures_total", { reason: "api_unavailable" });
          logger.error("auth.signin.failed", { reason: "api_unavailable", kind: r.kind, detail: r.message });
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
        session.user.superadmin = token.superadmin === true;
      }
      return session;
    },
  },
});

/** The api token is a JWS whose payload is readable without the key — the api already decided the
 *  claim at sign-in, this just reads it back to decide what the web RENDERS. */
function readSuperadmin(jws: string): boolean {
  try {
    const payload = jws.split(".")[1];
    return JSON.parse(Buffer.from(payload, "base64url").toString()).superadmin === true;
  } catch {
    return false;
  }
}
