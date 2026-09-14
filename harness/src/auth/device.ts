/** A desktop login: the same CLI credential kl-connect holds, kept apart from it. */
export type Credential = { api: string; token: string; expiresAt: string; username: string };

/**
 * The device-code login, exactly as `bins/kl-connect/src/login.rs` runs it: ask for a code, let
 * the person approve it in their own browser, poll for the token. Nothing held before approval
 * is a credential. 410 is the api's one terminal answer (expired, denied or already collected);
 * anything else unexpected is retried until the code's own expiry, as the CLI does.
 */
export class LoginFailed extends Error {
  constructor(message: string) {
    super(message);
    this.name = "LoginFailed";
  }
}

const CODE = /^[A-Z0-9]{4}-[A-Z0-9]{4}$/;

/** One claim of a JWT payload, unverified: display name and revocation id only. */
export function claim(token: string, key: string): string | undefined {
  try {
    const v = JSON.parse(Buffer.from(token.split(".")[1] ?? "", "base64url").toString("utf8")) as Record<string, unknown>;
    return typeof v[key] === "string" ? (v[key] as string) : undefined;
  } catch {
    return undefined;
  }
}

export const authorizeUrl = (api: string, code: string) => `${api}/cli/authorize?code=${code}`;

/** The one URL the app will hand to the OS browser. */
export function isAuthorizeUrl(api: string, url: string): boolean {
  const prefix = authorizeUrl(api, "");
  return url.startsWith(prefix) && CODE.test(url.slice(prefix.length));
}

const sleep = (ms: number, signal: AbortSignal) =>
  new Promise<void>((resolve, reject) => {
    if (signal.aborted) return reject(signal.reason);
    const t = setTimeout(resolve, ms);
    signal.addEventListener("abort", () => (clearTimeout(t), reject(signal.reason)), { once: true });
  });

export async function startLogin(api: string, device: string, opts: { signal: AbortSignal; pollMs?: number }) {
  const r = await fetch(`${api}/v1/cli/code`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ device }),
    signal: opts.signal,
  });
  if (!r.ok) throw new LoginFailed(`Kloudlite would not start a login (${r.status})`);
  const dc = (await r.json()) as { code: string; poll: string; expiresIn: number };
  if (!CODE.test(dc.code)) throw new LoginFailed("Kloudlite answered with a malformed login code");
  const deadline = Date.now() + dc.expiresIn * 1000;
  const every = opts.pollMs ?? 2000;

  const done = (async (): Promise<Credential> => {
    for (;;) {
      if (Date.now() > deadline) throw new LoginFailed("timed out waiting for approval");
      let status = 0;
      let body: { token?: string; expiresAt?: string } = {};
      try {
        const p = await fetch(`${api}/v1/cli/token?poll=${encodeURIComponent(dc.poll)}`, { signal: opts.signal });
        status = p.status;
        if (status === 200) body = (await p.json()) as typeof body;
        else await p.body?.cancel();
      } catch (e) {
        if (opts.signal.aborted) throw e;
        // a dropped connection mid-login is retried: the code stays valid until the api expires it
      }
      if (status === 200 && body.token) {
        const username = claim(body.token, "username") ?? claim(body.token, "name") ?? claim(body.token, "sub") ?? "";
        return { api, token: body.token, expiresAt: body.expiresAt ?? "", username };
      }
      if (status === 410) throw new LoginFailed("that login expired or was denied");
      await sleep(every, opts.signal);
    }
  })();
  done.catch(() => undefined); // observed by the caller; never an unhandled rejection meanwhile
  return { code: dc.code, url: authorizeUrl(api, dc.code), done };
}
