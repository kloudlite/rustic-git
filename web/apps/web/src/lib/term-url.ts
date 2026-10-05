/** The iframe `src` for a bench's browser terminal. `gateway` is the `wss://…/tunnel/{id}`
 *  address the api handed back with the session token — the gateway terminates TLS itself and
 *  mounts `/term/...` at its ORIGIN, not under `/tunnel/...` (`bins/gateway/src/term.rs`), so this
 *  builds from the origin alone and drops the rest of the path.
 *  Pure and server-free so it is importable from both the server action and a plain bun test. */
export function termUrl(gateway: string, bench: string, token: string): string {
  const u = new URL(gateway);
  u.protocol = u.protocol === "wss:" ? "https:" : "http:";
  return `${u.origin}/term/${encodeURIComponent(bench)}/?token=${encodeURIComponent(token)}`;
}
