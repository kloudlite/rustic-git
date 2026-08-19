import "server-only";

/**
 * Which email domains are handed to an identity provider rather than a password.
 *
 * A domain is enterprise-configured or it is not — there is no user-visible choice
 * here, which is why the sign-in form asks for the email first and decides afterwards.
 * Asking someone to pick "SSO or password" makes them guess at their own org's config.
 *
 * The map is read from AUTH_SSO_DOMAINS, as `domain=provider:Org Name` pairs:
 *
 *   AUTH_SSO_DOMAINS="acme.com=entra:Acme, dunder.com=okta:Dunder Mifflin"
 *
 * Deliberately not a hardcoded list. A domain listed here but with no provider
 * behind it sends its users to a dead end — the SSO screen has nothing to
 * continue to — and they cannot fall back to a password, because the form has
 * already decided for them. Unset means nobody is routed to SSO, which is the
 * correct behaviour for a deployment that has not configured one.
 */
function ssoDomains(): Record<string, { provider: string; name: string }> {
  const raw = process.env.AUTH_SSO_DOMAINS?.trim();
  if (!raw) return {};
  const out: Record<string, { provider: string; name: string }> = {};
  for (const entry of raw.split(",")) {
    const [domain, rest] = entry.split("=").map((p) => p?.trim());
    if (!domain || !rest) continue;
    const [provider, ...nameParts] = rest.split(":");
    if (!provider) continue;
    out[domain.toLowerCase()] = {
      provider: provider.trim(),
      name: nameParts.join(":").trim() || domain,
    };
  }
  return out;
}

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
  const hit = ssoDomains()[domain];
  return hit ? { kind: "sso", provider: hit.provider, org: hit.name } : { kind: "password" };
}
