# `kl` CLI and shell prompt implementation plan

> **For agentic workers:** implemented task by task by Opus implementers dispatched from the main
> session (Fable), which reviews each diff itself. Steps use `- [ ]` for tracking.

**Goal:** from a shell inside a workspace, `kl pkg add`, `kl env switch` and `kl container build`
work; every shell on the bench and in a workspace is zsh with the same starship prompt; `kl ide`
is hidden.

**Architecture:** a `workspace-token` JWT projected into the `user-key` Secret on the keys beat,
verified by `caller_for` against a seven-route audience; `kl` gains an HTTPS client and three
subcommand groups; the PTY picks the passwd shell; the bench image gains zsh + starship with rc
files rendered from one Rust constant the workspace prelude also uses.

**Spec:** `docs/superpowers/specs/2026-09-16-kl-cli-and-shell-prompt-design.md` — binding.

## Global constraints

- Worktree `/Volumes/kdisk/rustic-git-wt/desktop-login`, branch `desktop-login`. Prefix every
  shell command with `cd` there; the cwd resets between calls. Never `cargo` in
  `/Users/karthik/rustic-git`.
- Rust gates: `cargo clippy --workspace --all-targets -- -D warnings` plus the touched crates'
  `--lib` tests. Web: `cd web && bun test` when `deploy/slo.md` changes.
- Comments explain WHY; `//!` module docs carry context; files under ~800 lines; keep
  `// ponytail:` markers.
- Commits: imperative sentence case, no attribution lines. Do not push.
- Wire and env names exactly as the spec: `workspace-token`, `workspace-tool`,
  `KL_WORKSPACE_ID`, `KL_TEAM`, `KL_API_URL`, `WORKSPACE_TOOL_ROUTES`.
- A token is never logged, printed, or put in an error string.

---

### Task 1: the `workspace-token` — mint, project, verify (Rust, api + core)

**Files**
- Modify: `crates/core/src/jwt.rs` — `WorkspaceToolClaims { sub, space, jti, iat, exp, typ }`,
  `mint_workspace_tool(handle, space) -> Result<(String, WorkspaceToolClaims)>` (24 h,
  `typ: "workspace-tool"`), `verify_workspace_tool`, `expired_workspace_tool`; tests mirroring
  the bench-tool ones (round trip, wrong typ refused, expired detected).
- Modify: `crates/workspaces/src/k8s/secrets.rs` — `user_key_secret` gains a `workspace_token:
  &str` parameter and the `"workspace-token"` item; update every caller and the k8s tests.
- Modify: `crates/workspaces/src/api/workspaces/keys.rs` — `write_user_key` mints
  `mint_workspace_tool(owner_handle, space)` beside `mint_registry`. `space` = the namespace's
  owner slug: read how `ns`/`owner` are derived there (`ws-{owner}` → space = owner handle;
  `wt-{team}-{owner}` → space = team) and use the existing helper if one exists
  (`crd::ws_namespace`'s inverse); if none exists, add `crd::space_of_namespace(ns) -> Option<&str>`
  with a test for both shapes.
- Modify: `crates/workspaces/src/api/mod.rs` — `WORKSPACE_TOOL_ROUTES` (the seven routes from
  the spec), `workspace_tool_route()` (reuse `bench_tool_route`'s matcher by generalising it to
  take the table: `fn route_in(table, method, path)`), the `caller_for` branch after the
  bench-tool one (verify → `{team}` segment must equal `claims.space` else "space" → `Caller {
  name: sub, superadmin: false, parent: None, scope: Some(space), jti8 }` → `mark_via("workspace-tool")`),
  `workspace_tool_refused` logging like `bench_tool_refused`. Extend the router-complement test
  so every `WORKSPACE_TOOL_ROUTES` entry exists in the router and every other route refuses a
  workspace-tool caller with `audience`.
- Modify: `crates/workspaces/src/k8s/workspace.rs` `login_env` — add `KL_WORKSPACE_ID` (the CR
  name), `KL_TEAM` (the space, passed in by the caller that knows the namespace), `KL_API_URL`
  (from the agent's `WS_API_URL`, exactly as `k8s/bench.rs` line ~70 does; unset when empty).
  Update the snapshot tests.

- [ ] jwt trio + tests
- [ ] secret item + keys beat mint (+ space derivation)
- [ ] `caller_for` branch + route table + complement test
- [ ] pod env vars + tests
- [ ] gates; commit `Project a workspace-tool token into every workspace and admit it on seven routes`

### Task 2: `kl` subcommands (Rust, bins/kl)

**Files**
- Modify: `bins/kl/Cargo.toml` — `ureq = { version = "3", default-features = false, features
  ["rustls", "json"] }` (check the workspace's `Cargo.lock` for an existing `ureq`/`rustls` pin
  and reuse it); replace the "no tls" comment with why there is one now. `serde`/`serde_json`
  from the workspace.
- Create: `bins/kl/src/api.rs` — `struct Api { base: String, token_path: PathBuf }` from env
  (`KL_API_URL`, token at `/etc/kloudlite/ssh/workspace-token` — the same directory as
  `registry-token`, use the constant the docker helper uses if one exists); `get(path)`,
  `patch_json(path, body)`, `post(path)`, `put_json`, `delete` → `Result<serde_json::Value,
  String>`; error = api body text when `text/plain` or `{"error":…}`, else `HTTP <status>`;
  timeouts 10 s connect / 60 s total; the token read fresh per call.
- Create: `bins/kl/src/pkg.rs` — `list`, `add(entries)`, `rm(entries)`, `update`: GET the
  workspace, merge/dedupe (by attr, an `@version` on `rm`'s argument ignored), PATCH
  `/v1/workspaces/{KL_WORKSPACE_ID}/packages` `{"packages":[…]}`; print per the spec's lines,
  using the response's `locks` to print `(locked X)`.
- Create: `bins/kl/src/env.rs` — `list` (GET `/v1/environments?owner={KL_TEAM}`; mark the current
  from GET `/v1/me/environments`), `current`, `switch(name_or_id)` (resolve name→id from the
  list; ambiguous names refused naming both ids; PUT `/v1/me/environments/{KL_TEAM}`
  `{"environment": id}`), `clear` (DELETE).
- Modify: `bins/kl/src/main.rs` — command tree per the spec: `Container { Build{…}, Push{…} }`,
  `Pkg { List, Add{entries}, Rm{entries}, Update }`, `Env { List, Current, Switch{target},
  Clear }`, `Ide` with `#[command(hide = true)]`. Docker setup (`ensure_cred_helper`,
  `ensure_builder`, `wait_builder`) runs ONLY for `Container`. Exit codes: 0 ok, 1 refusal/error
  (message on stderr, the api's sentence verbatim), 2 usage (clap's).
- Tests: `api.rs` error mapping (text body, json error, status only) against a tiny in-process
  `std::net::TcpListener` HTTP server; `pkg.rs` merge/dedupe/rm rules; `env.rs` name resolution
  (unique, ambiguous, id passthrough); `main.rs` — `kl --help` lists `container pkg env` and
  not `ide`; `kl build` is unknown.

- [ ] Cargo + api client + tests
- [ ] pkg + env + tests
- [ ] command tree, hidden ide, docker setup scoped to container
- [ ] gates (`cargo test -p kl` or the crate's real name, clippy); commit `Give kl container,
      pkg and env subcommands and hide ide`

### Task 3: one prompt — PTY shell choice, bench zsh + starship, shared rc constants

**Files**
- Modify: `crates/ide/src/pty.rs` `candidates()` — first the passwd shell of `getuid()` via
  `libc::getpwuid_r` (only if non-empty and `access(X_OK)` succeeds), then `$SHELL`, then
  `/bin/bash`, `/bin/sh`; test: with `SHELL=/bin/sh`, `candidates()[0]` is the passwd shell of the
  test uid when that file exists.
- Create: `crates/workspaces/src/k8s/shell_rc.rs` — `pub const ZSHRC: &str`, `pub const
  STARSHIP_TOML: &str`, `pub fn seed_zshrc(path_env: &str) -> String`, lifted VERBATIM from the
  printf strings in `k8s/workspace.rs` (~141–151); `workspace.rs` uses them (the prelude output
  must be byte-identical — assert in the existing prelude test by comparing before/after
  strings captured in the test).
- Create: `deploy/bench/zshrc`, `deploy/bench/starship.toml` — the rendered constants; test
  `k8s/shell_rc.rs::bench_rc_files_match` reads both files (`include_str!`) and asserts equality
  with the constants, so drift fails the build.
- Modify: `deploy/bench/Dockerfile` runtime stage: `apt-get install zsh`; starship from
  `https://github.com/starship/starship/releases/download/v${STARSHIP_VERSION}/starship-x86_64-unknown-linux-musl.tar.gz`
  with `ARG STARSHIP_VERSION=1.20.1` (or the current latest — check) and a pinned
  `STARSHIP_SHA256` verified with `sha256sum -c`; `COPY deploy/bench/zshrc /etc/zsh/zshrc`,
  `COPY deploy/bench/starship.toml /etc/starship.toml`.
- Modify: `crates/workspaces/src/k8s/bench.rs` env: `SHELL=/bin/zsh`, `ZDOTDIR=/home/kl/.config/zsh`,
  `HISTFILE=/home/kl/.local/state/zsh_history`; the pod prelude (or harness-bench at start, if the
  pod has no prelude — check `bench.rs`'s `command`) creates `~/.config/zsh` and seeds
  `.zshrc` with `seed_zshrc(PATH)` when absent, the way the workspace prelude does. Update tests.

- [ ] pty candidates + test
- [ ] shell_rc constants, prelude uses them, rendered files + equality test
- [ ] bench Dockerfile + env + seed
- [ ] gates; commit `Give the bench the workspace's zsh and starship prompt and pick the passwd
      shell for PTYs`

### Task 4: probes and docs

**Files**
- Modify: `bins/slo/src/stages/workspace.rs` — `build_script`/push script use `kl container
  build|push`; new steps `ws.kl.pkg.add` and `ws.kl.env.switch` per the spec (through the tool
  server `exec` the build step already uses; env switch needs the run's environment id — group 0
  creates one, find its id in `Ctx`); `bins/slo/src/stages/bench.rs` `shell_workspace` asserts
  `❯` in the output.
- Modify: `crates/workspaces/src/slo/catalogue.rs`, `deploy/slo.md` — two rows, group 0,
  hourly, `5 · Workspace`; targets `99.9 % ≤ 20000 ms` and `99.9 % ≤ 10000 ms`.
- Modify: `CLAUDE.md` (the `kl` paragraph: `kl container build`, `kl pkg`, `kl env`, hidden
  `ide`), `deploy/workspace-image/kl-build.sh` comment, `docs/superpowers/specs/2026-09-09-workspace-kl-design.md`
  gets a one-line "superseded by 2026-09-16-kl-cli-and-shell-prompt-design.md for the command
  tree" note at the top.
- Gates: `cargo test --manifest-path bins/slo/Cargo.toml`, `cargo test -p kloudlite-workspaces
  --lib slo`, clippy, `cd web && bun test`.

- [ ] probe scripts + two new steps
- [ ] catalogue + slo.md + fixture if needed
- [ ] docs
- [ ] commit `Probe kl pkg add and kl env switch hourly and document the kl command tree`

### Task 5: ship and verify (main session)

- [ ] owner pushes origin; pod pull + ship; pin; roll AKS + k3s; restart the owner's bench pod
      and workspace pod once (image); manual hourly: `ws.kl.pkg.add`, `ws.kl.env.switch`,
      `bench.shell.workspace` with `❯`, `ws.build.p95` on `kl container build` all pass.
- [ ] owner: bench shell shows the starship prompt; `kl pkg add cowsay` in a workspace shell.

### Task 6: the desktop mirrors the platform (owner add, 20:25 IST: "whenever the env change or package installed via workspace tools or shell the data outside should also update")

**Files**
- Modify: `harness/src/connect/platform.ts` — `myEnvironment(api, token, team): Promise<string |
  undefined>` = `GET /v1/me/environments`, the row for `team` (read `crates/workspaces/src/api/me.rs`
  `list_my_environments` for the exact response shape), returning its environment id;
  `setMyEnvironment(api, token, team, id)` = `PUT /v1/me/environments/{team}` `{"environment": id}`;
  `clearMyEnvironment` = DELETE.
- Modify: `harness/src/main.ts` — IPC `platform:myEnvironment`, `platform:setMyEnvironment(id)`,
  `platform:clearMyEnvironment` through the existing `platform()` wrapper; `harness/src/preload.ts`
  exposes them under `window.harness.platform`.
- Modify: `harness/src/renderer/App.tsx` — `connected` is no longer a local choice: `refresh()`
  also reads `platform.myEnvironment()` and sets `connected` from it (empty string when none, and
  then `environment()` is `undefined`, never `environments()[0]`); whatever UI sets `connected`
  today (find `setConnected` callers) calls `platform.setMyEnvironment(id)` / `clearMyEnvironment()`
  first and refreshes after. The beat goes from 30 s to 10 s while visible; a shell tab's exit
  (`pty:exit`, any code) and every pi `tool_execution_end` whose tool name starts with `kl_` or
  `workspace_`/`environment_` (read `harness/src/renderer/live.ts` line ~244 for where the event
  lands and `harness/pi/workspace-tools.ts` + `harness/pi/kloudlite.ts` for tool names) trigger one
  `refresh()` (coalesced by the existing `refreshing` flag).
- Modify: `harness/src/renderer/components/MachinePanel.tsx` (or wherever a workspace's packages are
  rendered) — nothing structural; confirm the list re-renders from `workspaces()`.
- Tests: `harness/bench/test/desktop-platform.test.ts` gains `myEnvironment`/`setMyEnvironment`
  against the fake api the file already uses; a small pure helper `shouldRefreshOn(ev): boolean`
  in `harness/src/renderer/rows.ts` or a new `harness/src/renderer/refresh.ts` with a table test.
- Gates: `cd harness && npm run typecheck && npm run bench:test && npm run build`.

- [ ] platform.ts + main + preload
- [ ] App.tsx: connected from the platform; beat 10 s; refresh on shell exit and kl/workspace tool end
- [ ] tests; commit `Mirror the space environment and refresh the desktop on shell and tool changes`

## Self-review

- Spec §1 → T3; §2 → T2 (+T4 probes/docs); §3 → T1; §4 → T2; §5 → T4. Out-of-scope items
  not planned.
- Names: `WorkspaceToolClaims.space`, `KL_TEAM` = space, `KL_WORKSPACE_ID`; `Api` client in
  `bins/kl/src/api.rs`; `shell_rc::{ZSHRC, STARSHIP_TOML, seed_zshrc}`.
- T6 touches only `harness/src/**` and runs in parallel with any of T1–T4.
- T1 and T2 share no files (T2 reads env names only); T3 touches `crates/ide` and
  `k8s/{workspace,bench}.rs` + `deploy/bench`; T1 touches `k8s/workspace.rs` `login_env` too —
  T3 waits for T1. T4 waits for T2 (command names) and T3 (prompt assertion).
