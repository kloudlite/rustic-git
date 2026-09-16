# `kl` from inside a workspace: packages, environments, containers — and one prompt everywhere

Owner asks (2026-09-16, in the desktop shells session): "I want you to have zsh as shell and I
should be able to install packages from the workspaces too using kl like kl pkg add"; "kl ide
should be hidden"; "put container image build option under kl container sub command"; "there
should be option to switch env using kl env switch"; "use starship there too. design proper prompt
like other workspaces". Design approved in chat ("go").

## What exists

- `kl` (`bins/kl`, musl, in the workspace image) has `build`, `push`, `ide serve`. It has no HTTP
  client and no platform credential; `build`/`push` drive `docker buildx` against the builder gate
  with `registry-token` from the `user-key` Secret (`docker-credential-kl`).
- Packages are edited only from the web/desktop: `PATCH /v1/workspaces/{id}/packages` (list is
  validated and locked by the api, the agent rebuilds the Nix profile) and
  `POST /v1/workspaces/{id}/packages/update`.
- The person's environment in a team is a *space environment*: `GET /v1/me/environments`,
  `PUT /v1/me/environments/{team}` (body `{"environment": <id>}`), `DELETE /v1/me/environments/{team}`;
  `GET /v1/environments?owner=<team>` lists candidates. `/attach` and `/detach` are gone (410).
- A bench's pi tools reach `/v1` with a `bench-tool` JWT: audience allow-list `BENCH_TOOL_ROUTES`,
  `caller_for` verifies it, a Bench must admit it, parent CLI token must be live.
- Workspace image (alpine + Nix profile) already has zsh, fish and starship; `/etc/zshrc` and
  `/etc/starship.toml` are written by the pod prelude; the uid-1000 passwd shell is the profile's
  zsh. The tool-server PTY (`/stream/pty`) forks `$SHELL`, which is `/bin/sh` because `kl ide
  serve` is started through `su -s /bin/sh`.
- Bench image (bookworm-slim) has bash only; `SHELL=/bin/bash`.

## Design

### 1. The prompt: one look in bench and workspace shells

- Tool server PTY: `crates/ide/src/pty.rs::candidates()` prefers the uid's passwd shell
  (`getpwuid_r`, when it exists and is executable), then `$SHELL`, then `/bin/bash`, `/bin/sh`.
  A workspace shell is therefore the Nix zsh, as ssh already is. The forked shell is a login
  shell (unchanged), so `/etc/zshrc` + `ZDOTDIR` + starship apply.
- Bench image: `apt-get install zsh` in the runtime stage; starship from its GitHub release
  tarball, version and sha256 pinned in the Dockerfile (`STARSHIP_VERSION`, `STARSHIP_SHA256`),
  installed to `/usr/local/bin/starship`. `SHELL=/bin/zsh`, `ZDOTDIR=$HOME/.config/zsh`,
  `HISTFILE=$HOME/.local/state/zsh_history` in the bench pod env (`k8s/bench.rs`), and a
  `/etc/zsh/zshrc` + `/etc/starship.toml` in the image whose CONTENT is the same text the
  workspace prelude writes (`k8s/workspace.rs` lines ~141–151): interactive guard, completion,
  starship init, the `STARSHIP_CONFIG=/etc/starship.toml` fallback when the person has none.
  The text is extracted into `crates/workspaces/src/k8s/shell_rc.rs` as `pub const ZSHRC`,
  `pub const STARSHIP_TOML`, `pub fn seed_zshrc(path: &str) -> String`, used by the prelude, and
  rendered into `deploy/bench/zshrc` + `deploy/bench/starship.toml` by a test that fails when the
  files drift (`deploy/slo.md`-style equality), so the two images cannot diverge silently.
  The bench's prompt shows `bench` where a workspace shows the workspace name: starship's
  `$directory` already does that (cwd `$HOME` on a bench, the workspace dir in a workspace);
  nothing extra.

### 2. `kl`'s command tree

```
kl container build -t NAME:TAG [-f FILE] [--build-arg K=V]... [--platform P] [--no-cache] [CONTEXT]
kl container push SRC DST...
kl pkg list                       # spec.packages with their locked versions
kl pkg add ENTRY...               # ENTRY = attr or attr@version; merged, deduplicated, PATCH
kl pkg rm ENTRY...                # by attr (a version suffix on the argument is ignored)
kl pkg update                     # POST packages/update
kl env list                       # environments of the team, current one marked
kl env current                    # the space environment in this team, or "none"
kl env switch NAME-OR-ID          # PUT /v1/me/environments/{team}
kl env clear                      # DELETE /v1/me/environments/{team}
kl ide serve ...                  # hidden (clap `hide = true`), unchanged
```

`kl build`/`kl push` are removed, not aliased: the probes (`ws.build.p95`, `ws.push.*` in
`bins/slo/src/stages/workspace.rs`), the profile script comment and `docs/`/`CLAUDE.md` mentions
move to the new spelling. Every `kl pkg`/`kl env` verb prints one line per effect and exits
non-zero with the api's own sentence on refusal (`422 nodejs@99: not in the index; nearest …`).
`kl pkg add` prints `added nodejs@20 (locked 20.11.1)` per entry and ends with
`the workspace rebuilds its profile; new binaries appear in a fresh shell`.

Identity for those calls comes from the pod env: `KL_WORKSPACE_ID` (the CR name, new var),
`KL_TEAM` (the space: the owner slug of the namespace — a person's handle for a personal
workspace, the team slug for a team workspace; new var), `KL_API_URL` (new var, from the same
source `k8s/bench.rs` uses for the bench's `KL_API_URL`).

### 3. The credential: `workspace-token`

A third item in the `user-key` Secret beside `registry-token`, minted on the same keys beat by
the same code path (`api/workspaces/keys.rs` `write_user_key`), 24 h TTL, re-minted every
`KEYS_RESYNC_SECS`, mounted where `registry-token` already is (`/etc/kloudlite/ssh/workspace-token`).
`kl` reads it per call; never caches it; never prints it.

JWT: `mint_workspace_tool(handle, space) -> (String, WorkspaceToolClaims { sub, space, jti, iat,
exp, typ: "workspace-tool" })`, `verify_workspace_tool`, `expired_workspace_tool` — the bench-tool
trio, copied. In `caller_for`: after the bench-tool branch, the same shape for workspace-tool:

- audience allow-list `WORKSPACE_TOOL_ROUTES`:
  `GET /v1/workspaces/{id}`, `PATCH /v1/workspaces/{id}/packages`,
  `POST /v1/workspaces/{id}/packages/update`, `GET /v1/environments`,
  `GET /v1/me/environments`, `PUT /v1/me/environments/{team}`, `DELETE /v1/me/environments/{team}`.
  Everything else is refused `audience`. The complement test that holds `BENCH_TOOL_ROUTES` to
  the router holds this list too.
- `{team}` must equal `claims.space`, else `space`.
- the `Caller` is `{ name: claims.sub, scope: Some(claims.space) }` — the existing `my_ws`
  check then refuses a workspace the caller does not own in that space (404, as for anyone).
- no parent CLI token (the pod's identity is the person's key projection, revoked by the same
  beat that revokes keys — `OwnerKeys` gone ⇒ Secret rewritten without it within one beat);
  no Bench admission check. A paused or removed member's `user-key` is rewritten by the
  membership beat already; that is the revocation path, and it is tested.
- `mark_via("workspace-tool")` for metrics; refusals logged like `bench_tool_refused`.

Scope note (owner accepted in the design): the token covers the owner's workspaces in that
space, not one workspace — `user-key` is per owner namespace. Blast radius of a stolen token is
"edit the package list or switch the environment of your own other workspaces for 24 h".

### 4. `kl`'s HTTP client

`ureq` 3 with `rustls` (the api is HTTPS behind Cloudflare). `kl` grows by roughly 1.5 MB; the
"no tls" note in `bins/kl/Cargo.toml` is replaced by the reason it now has one. Timeouts: 10 s
connect, 60 s total. Errors are the api's body text when it is text, else `HTTP <status>`.

### 5. Probes

Two hourly ids beside `ws.build.p95` in `deploy/slo.md` and the catalogue, group 0 (they need
the run's workspace), stage `5 · Workspace`:

- `ws.kl.pkg.add` — through the tool server's `exec`: `kl pkg add cowsay` exits 0 and
  `GET /v1/workspaces/{id}` then lists `cowsay` in `packages`; target `99.9 % ≤ 20000 ms`. The
  run's teardown deletes the workspace, so nothing to undo.
- `ws.kl.env.switch` — `kl env switch <the run's environment>` exits 0 and
  `GET /v1/me/environments` (as the probe user) names it; then `kl env clear`; target
  `99.9 % ≤ 10000 ms`.

`ws.build.p95`/push scripts switch to `kl container build|push`. `bench.shell.workspace` gains
one assertion: the first output frame contains `❯` (starship's character) — the prompt is the
product here.

## Out of scope

- `kl pkg search`; per-workspace token scope; `kl` outside a workspace (it still refuses without
  its env vars); fish as a bench shell; the person's own dotfiles on the bench beyond
  `~/.config/zsh` (the bench home already persists them).

## Security summary

One new credential, projected by the path every workspace credential already takes, revoked by
the same beat, with a seven-route audience and an owner check on every workspace-shaped route.
`kl` never logs it; `docker-credential-kl` does not read it. The token file is `0400` uid 1000
like its siblings (the Secret mount's `defaultMode`).
