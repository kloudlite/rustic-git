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
}): "/" | "/welcome" | null {
  const { hasSession, hasToken, username, from } = opts;
  if (!hasSession) return null;
  // The producers of `from=expired` send a caller here precisely because their
  // token was refused. Bouncing them onward is the loop.
  if (from === "expired") return null;
  // No handle yet: /welcome is the one page that renders without a token.
  if (!username) return "/welcome";
  return hasToken ? "/" : null;
}
