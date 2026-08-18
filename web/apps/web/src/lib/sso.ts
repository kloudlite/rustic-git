import "server-only";

/**
 * Which email domains are handed to an identity provider rather than a password.
 *
 * A domain is enterprise-configured or it is not — there is no user-visible choice
 * here, which is why the sign-in form asks for the email first and decides afterwards.
 * Asking someone to pick "SSO or password" makes them guess at their own org's config.
 */
const SSO_DOMAINS: Record<string, { provider: string; name: string }> = {
  "kloudlite.io": { provider: "okta", name: "kloudlite" },
  "example-corp.com": { provider: "entra", name: "Example Corp" },
};

/** Free-mail domains can never be enterprise SSO, whatever anyone configures. */
const CONSUMER = new Set([
  "gmail.com", "googlemail.com", "outlook.com", "hotmail.com", "live.com",
  "yahoo.com", "icloud.com", "me.com", "proton.me", "protonmail.com",
]);

export type EmailRoute =
  | { kind: "sso"; provider: string; org: string }
  | { kind: "password" };

export function routeForEmail(email: string): EmailRoute {
  const domain = email.trim().toLowerCase().split("@")[1] ?? "";
  if (!domain || CONSUMER.has(domain)) return { kind: "password" };
  const hit = SSO_DOMAINS[domain];
  return hit ? { kind: "sso", provider: hit.provider, org: hit.name } : { kind: "password" };
}
