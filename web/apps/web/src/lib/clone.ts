/**
 * Where to clone from.
 *
 * Read on the SERVER and passed down as props, never through `NEXT_PUBLIC_`:
 * those are inlined at build time, so a value set in the deployment would never
 * reach the browser and the menu would keep printing whatever the image was built
 * with. The addresses a person copies must be the ones this deployment actually
 * answers on — a stale host is how a Clone menu ends up handing out URLs that
 * resolve to somebody else's site.
 *
 * The ssh form matters as much as the host. `git@host:owner/repo.git` is scp
 * syntax and cannot carry a port, so a deployment whose ssh listener is not on 22
 * must print `ssh://git@host:port/owner/repo.git` instead — the short form would
 * silently try port 22 and hang.
 */
import { getPublicCentralSettings, type PublicCentralSettings } from "@/lib/api";

export type CloneUrls = { https: string; ssh: string; cli: string };

/** `GET /v1/settings/central` is now the truth for these three (Task 4 Step 5 of the live-settings
 *  plan): an admin-set override in the settings document beats whatever this pod's own env was
 *  templated with, exactly like every other live setting's `stored ?? env ?? default` order.
 *  Cached in-process for a few seconds — a plain closure over a timestamp, not a new dependency —
 *  so a page render doesn't cost the api tier a round trip on every request; `getPublicCentralSettings`
 *  itself already has a 5s fetch timeout and falls back to blank fields on any error, which this
 *  reads the same way the route's own doc comment does: blank means "fall back to env".
 */
const CACHE_TTL_MS = 5_000;
let cached: { value: PublicCentralSettings; at: number } | null = null;

async function centralSettings(): Promise<PublicCentralSettings> {
  if (cached && Date.now() - cached.at < CACHE_TTL_MS) return cached.value;
  const r = await getPublicCentralSettings();
  const value = r.ok ? r.value : { cloneHost: "", sshHost: "", sshPort: 22, registryHost: "" };
  cached = { value, at: Date.now() };
  return value;
}

/** A host this deployment answers on, or a throw at first use — the same stance `auth.ts`
 *  takes on AUTH_URL: a production fallback would be a fallback to somebody else's site, and
 *  only a dev server gets `localhost` for free. `fromCentral` wins when the admin has set it;
 *  empty means "never set", not "clear it to nothing". */
function host(fromCentral: string, envName: string): string {
  if (fromCentral) return fromCentral.replace(/\/$/, "");
  const v = process.env[envName];
  if (v) return v.replace(/\/$/, "");
  if (process.env.NODE_ENV === "production") throw new Error(`${envName} is not set`);
  return "localhost";
}

export async function registryHost() {
  const c = await centralSettings();
  return host(c.registryHost, "KLOUDLITE_GIT_REGISTRY_HOST");
}

export async function cloneUrls(owner: string, repo: string): Promise<CloneUrls> {
  const c = await centralSettings();
  const httpHost = host(c.cloneHost, "KLOUDLITE_GIT_CLONE_HOST");
  const sshHost = host(c.sshHost, "KLOUDLITE_GIT_SSH_HOST");
  const sshPort = c.sshPort || Number(process.env.KLOUDLITE_GIT_SSH_PORT ?? 22);
  const path = `${owner}/${repo}.git`;
  return {
    https: `https://${httpHost}/${path}`,
    ssh: sshPort === 22 ? `git@${sshHost}:${path}` : `ssh://git@${sshHost}:${sshPort}/${path}`,
    cli: `kl clone ${owner}/${repo}`,
  };
}
