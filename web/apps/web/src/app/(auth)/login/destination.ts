/** A `?next=` worth honouring, or `undefined`.
 *
 *  Only a same-origin RELATIVE path. `//evil.com` is a protocol-relative URL that
 *  browsers follow off-site, and it starts with `/` — so "starts with a slash" alone
 *  is the open redirect, not the guard against it. Everything that reaches a
 *  `redirectTo` goes through here. */
export function safeNext(next?: string): string | undefined {
  if (!next || !next.startsWith("/") || next.startsWith("//")) return undefined;
  // A backslash is a slash to some parsers; `/\evil.com` has escaped the origin in
  // browsers that normalise it before the redirect.
  if (next.startsWith("/\\")) return undefined;
  return next;
}

/** Where a request to /login actually belongs.
 *
 *  Pure, and tested, because every arm below has been a redirect loop: /login
 *  bounced any signed-in caller to /, while the pages behind / bounce a caller
 *  with no usable api token back to /login. "Signed in" is not the condition —
 *  "signed in AND holding a token the api will accept" is.
 *
 *  `null` means: stay on /login and show the form. */
export function loginDestination(opts: {
  hasSession: boolean;
  hasToken: boolean;
  username?: string;
  from?: string;
  /** Where the caller was headed when they were sent here. */
  next?: string;
}): string | null {
  const { hasSession, hasToken, username, from } = opts;
  const next = safeNext(opts.next);
  if (!hasSession) return null;
  // The producers of `from=expired` send a caller here precisely because their
  // token was refused. Bouncing them onward is the loop.
  if (from === "expired") return null;
  // No handle yet: /welcome is the one page that renders without a token. `next` waits —
  // it is reached from /welcome's own onward redirect, not skipped over it.
  if (!username) return "/welcome";
  return hasToken ? (next ?? "/") : null;
}
