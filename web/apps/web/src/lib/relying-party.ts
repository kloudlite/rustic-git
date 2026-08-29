/** The WebAuthn relying party: the site, as the browser sees it. A credential is bound to the
 *  rpID and the origin the server checks must be byte-for-byte the one the browser used.
 *
 *  `AUTH_URL` is the deployment's canonical address (required in production, see `auth.ts`), so
 *  when it is set it is the answer and a request claiming a different host is refused — a
 *  forged `X-Forwarded-Host` reaching the pod directly must not pick the rpID. Only without it
 *  (dev: direct localhost, a tunnel) does the request's own host stand in, with the scheme fixed
 *  by the host rather than `X-Forwarded-Proto`, which Cloudflare's Flexible SSL sends as http. */
export function relyingPartyFor(authUrl: string | undefined, requestHost: string | undefined) {
  const seen = requestHost?.split(",")[0].trim();
  if (authUrl) {
    const u = new URL(authUrl);
    if (seen && seen.toLowerCase() !== u.host.toLowerCase()) {
      throw new Error(`passkeys are served for ${u.host}, not ${seen}`);
    }
    return { rpID: u.hostname, origin: u.origin, rpName: "kloudlite" };
  }
  const host = seen || "localhost:3000";
  const hostname = host.split(":")[0];
  const proto = hostname === "localhost" || hostname === "127.0.0.1" ? "http" : "https";
  return { rpID: hostname, origin: `${proto}://${host}`, rpName: "kloudlite" };
}
