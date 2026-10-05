/** The iframe `src` for a bench's browser terminal. `gateway` is the `wss://…` address the api
 *  handed back with the session token; the gateway terminates TLS itself, so the same host answers
 *  `https://` for the `/term/...` page the ponytail comment in `bins/gateway/src/term.rs` proxies.
 *  Pure and server-free so it is importable from both the server action and a plain bun test. */
export function termUrl(gateway: string, bench: string, token: string): string {
  const https = gateway.replace(/^wss:\/\//, "https://");
  return `${https}/term/${encodeURIComponent(bench)}/?token=${encodeURIComponent(token)}`;
}
