# Authentication

Three credentials, each scoped to one job. A session signs you into the console. A bearer token calls `/v1`. An ssh key opens a workspace and pushes to git. Keys and tokens belong to a person, never to a team or a workspace.

## Console sign-in

The console signs you in with a provider, a passkey, or an emailed sign-in link. A deployment with no OAuth provider yet may also offer email + a shared password, and that is gated by two variables together: `AUTH_SHARED_PASSWORD` (the password) and `AUTH_SHARED_PASSWORD_ENABLED=1` (the decision to open it). With either missing the provider is not registered at all — the sign-in page shows no password form — so a password left in a secret store never opens the door on its own. Addresses are still limited to `AUTH_ALLOWED_EMAILS`, and wrong passwords are counted and locked out per account.

## Bearer tokens

Every `/v1` request carries `Authorization: Bearer <token>`. Two ways to get one:

| Token | Made by | Lifetime | Use |
|---|---|---|---|
| CLI token | `kl-connect login` (browser approval) | Until `kl-connect logout` or revoked | Your machine's `kl-connect`, and scripts on it |
| Personal token | `POST /v1/tokens` from a signed-in session | As requested | Agents, CI, anything not on your machine |

```bash [API]
# Mint a personal token from a browser session (cookie auth), then use it as a bearer.
curl -sS https://dev.kloudlite.io/v1/tokens -b "$SESSION_COOKIE" \
  -H 'content-type: application/json' -d '{"name": "ci"}'

curl -sS https://dev.kloudlite.io/v1/workspaces -H "Authorization: Bearer $KL_TOKEN"
```

List and revoke tokens with `GET /v1/tokens` and `DELETE /v1/tokens/{id}`; CLI tokens live under `GET /v1/cli/tokens` and `DELETE /v1/cli/tokens/{id}`. A revoked token fails on its next request.

::: note
A token acts as you. Anything you may do to a team's objects, a token you minted may do too, through team membership rather than a per-token scope.
:::

## ssh keys

Add a public key once; it opens every workspace you own or may act on, and authenticates `git@git.khost.dev`.

```bash [API]
curl -sS https://dev.kloudlite.io/v1/keys -H "Authorization: Bearer $KL_TOKEN" \
  -H 'content-type: application/json' \
  -d "{\"name\": \"laptop\", \"key\": \"$(cat ~/.ssh/id_ed25519.pub)\", \"signing\": false}"
```

The key reaches a running workspace's `authorized_keys` without a restart; so does a revoke. `signing: true` registers a commit-signing key instead of an access key. A body carrying `owner` is refused with `400`: the owner is always the caller.

## What a credential may act on

Access is by ownership. You may act on your own objects, and on a team's objects while you are a member of that team. There is no per-workspace access list. See [Teams](platform/teams.md).

## Inside a workspace

A workspace carries a registry credential for `kl build` and `kl push`, projected into the pod and rotated on a beat. It never appears on a command line. Nothing else is projected: an agent that needs the API from inside a workspace brings its own token.
