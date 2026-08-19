import "server-only";

/**
 * A development-only way past sign-in, so the app can be worked on before OAuth
 * credentials exist.
 *
 * Two independent conditions must hold, and one of them cannot be set by
 * configuration: the build must not be a production build. An environment
 * variable alone is not enough, because environment variables are exactly the
 * thing that gets copied to the wrong place.
 */
const enabled = process.env.NODE_ENV !== "production" && process.env.AUTH_DEV_BYPASS === "1";

/* A production build that still carries the flag is a misconfiguration worth
   failing loudly on, rather than silently ignoring and wondering later. */
if (process.env.NODE_ENV === "production" && process.env.AUTH_DEV_BYPASS === "1") {
  throw new Error(
    "AUTH_DEV_BYPASS is set in a production build. Remove it — it would sign every visitor in.",
  );
}

export const DEV_BYPASS = enabled;

/** Cookie that turns the bypass off for one browser, so the signed-out UI stays
 *  reachable without editing the environment and restarting. */
export const DEV_SIGNED_OUT_COOKIE = "kl_dev_signed_out";

export function devUser() {
  const email = process.env.AUTH_DEV_EMAIL || "dev@kloudlite.io";
  return {
    name: process.env.AUTH_DEV_NAME || "Dev User",
    email,
    /* Derived from the identity, exactly as it is for a real sign-in. Deliberately
       not configurable: an env var that lets you pick your own namespace is a way
       to assume an identity you do not have, which is the one thing a dev bypass
       must not quietly teach the codebase to allow. */
    owner: email.split("@")[0],
  };
}
