import { call } from "./client";
import { repoPath } from "./pulls";

/**
 * Credentials: tokens, CLI tokens and device codes, SSH and signing keys, the platform key,
 * passkeys.
 */
/** A credential's metadata. The secret is never here — a token is readable exactly
 *  once, in the reply to the call that created it. */
export type ApiCredential = {
  _id: string;
  kind: "token" | "sshkey" | "signingkey";
  owner: string;
  createdBy: string;
  name: string;
  /** An ssh key's public line, kept so the fleet can build `authorized_keys`. Empty for keys
   *  added before it was kept — those still clone over ssh but cannot reach a workspace. */
  material?: string;
};

export type IssuedToken = ApiCredential & { token: string };

export function listTokens(token: string, owner: string) {
  return call<ApiCredential[]>(`/v1/tokens?owner=${encodeURIComponent(owner)}`, { method: "GET", token });
}

export function createToken(token: string, owner: string, name: string) {
  return call<IssuedToken>("/v1/tokens", { method: "POST", token, body: JSON.stringify({ owner, name }) });
}

export function revokeToken(token: string, id: string) {
  return call<void>(`/v1/tokens/${encodeURIComponent(id)}`, { method: "DELETE", token });
}

/** One CLI login: the device that asked for it, and when it stops working on its own. */
export type ApiCliToken = { id: string; name: string; createdAt: string; expiresAt: string };

/** Defaults to the caller's own handle — a CLI login is personal, so no owner is passed. */
export function listCliTokens(token: string) {
  return call<ApiCliToken[]>("/v1/cli/tokens", { method: "GET", token });
}

export function revokeCliToken(token: string, id: string) {
  return call<void>(`/v1/cli/tokens/${encodeURIComponent(id)}`, { method: "DELETE", token });
}

/** The machine waiting on a device code. Read before the approval page offers a button: the one
 *  check a person can make is "is this my terminal", and that needs the device on screen. */
export type ApiPendingCode = { device: string; expiresAt: string };

export function pendingCliCode(token: string, code: string) {
  return call<ApiPendingCode>(`/v1/cli/code/${encodeURIComponent(code)}`, { method: "GET", token });
}

/** Approves a device code as the signed-in person. 404 covers unknown, expired and
 *  already-approved alike — deliberately, so a guesser learns nothing. */
export function approveCliCode(token: string, code: string) {
  return call<void>("/v1/cli/approve", { method: "POST", token, body: JSON.stringify({ code }) });
}

/** Keys are the person's, not a namespace's: no owner on either call. */
export function listKeys(token: string, kind: "ssh" | "signing" = "ssh") {
  return call<ApiCredential[]>(`/v1/keys${kind === "signing" ? "?kind=signing" : ""}`, {
    method: "GET",
    token,
  });
}

export function addKey(token: string, name: string, key: string, signing = false) {
  return call<ApiCredential>("/v1/keys", {
    method: "POST",
    token,
    body: JSON.stringify({ name, key, signing }),
  });
}

/** What a commit's signature amounts to. `unsigned` is the ordinary case, not a
 *  warning; `unverified` always carries a reason written for a person. */
export type ApiVerification = {
  state: "unsigned" | "verified" | "unverified";
  /** GitHub's vocabulary — `valid`, `unknown_key`, `expired_key`, `revoked_key`,
   *  `bad_email`, `invalid`, `unknown_signature_type` — so a client branches on a
   *  fixed set rather than on prose. */
  reasonCode: string;
  signer?: string;
  reason?: string;
};

export function verifyCommit(token: string, owner: string, name: string, sha: string) {
  return call<ApiVerification>(
    `${repoPath(owner, name)}/commits/${encodeURIComponent(sha)}/signature`,
    { method: "GET", token },
  );
}

/** The key the platform issued, which every workspace of the owner's carries. Unlike
 *  `/v1/keys` there is at most one, and it is generated on first read. */
export type ApiPlatformKey = { public: string; fingerprint: string };

export function platformKey(token: string, owner: string) {
  return call<ApiPlatformKey>(`/v1/platform-key?owner=${encodeURIComponent(owner)}`, {
    method: "GET",
    token,
  });
}

/** Replaces the key and revokes the old one — there is no way to keep both. */
export function regeneratePlatformKey(token: string, owner: string) {
  return call<ApiPlatformKey>(`/v1/platform-key?owner=${encodeURIComponent(owner)}`, {
    method: "POST",
    token,
  });
}

export function removeKey(token: string, id: string) {
  return call<void>(`/v1/keys/${encodeURIComponent(id)}`, { method: "DELETE", token });
}

export type ApiPasskey = {
  _id: string;
  user: string;
  publicKey: string;
  counter: number;
  transports: string[];
  name: string;
};

export function listPasskeys(token: string) {
  return call<ApiPasskey[]>("/v1/passkeys", { method: "GET", token });
}

export function addPasskey(
  token: string,
  key: { id: string; publicKey: string; counter: number; transports: string[]; name: string },
) {
  return call<ApiPasskey>("/v1/passkeys", { method: "POST", token, body: JSON.stringify(key) });
}

export function removePasskey(token: string, id: string) {
  return call<void>(`/v1/passkeys/${encodeURIComponent(id)}`, { method: "DELETE", token });
}

/** Sign-in only, so it goes over the peer path: there is no session yet, and the
 *  browser must never be able to ask whose credential an id belongs to. */
export function lookupPasskey(id: string) {
  return call<ApiPasskey>("/v1/passkeys/lookup", {
    method: "POST",
    asUser: "passkey-lookup",
    body: JSON.stringify({ id }),
  });
}

export function passkeyUsed(id: string, counter: number) {
  return call<void>(`/v1/passkeys/${encodeURIComponent(id)}/used`, {
    method: "POST",
    asUser: "passkey-lookup",
    body: JSON.stringify({ counter }),
  });
}
