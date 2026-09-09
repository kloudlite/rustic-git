# `kl` inside a workspace: build and push with one tool

**Date:** 2026-09-09
**Status:** approved for planning
**Builds on:** `2026-09-09-workspace-image-builds-design.md` (the hidden builder, the gate, the
credential helper), and the rename of the laptop CLI to `kl-connect` (commit `0a101b83`).

## 1. Problem

A workspace can build and push an image today, but only through one exact incantation —
`docker buildx build --push -t cr.khost.dev/<owner>/<name>:<tag> .` — and every standard habit
around it fails in a way that looks broken rather than deliberate: `docker push` says "cannot
connect to the Docker daemon", `docker build` without `--push` builds an image that then exists
nowhere, and a short tag (`hello:1`) goes to docker.io. There is no daemon in a workspace and we
are not adding one (§6), so the fix is a tool whose verbs match what the platform can do.

## 2. Decision

A binary named `kl` ships in the workspace image with two verbs:

- `kl build` — build on the owner's builder and push the result to the owner's registry.
- `kl push` — promote an image that is already in the registry under another name.

Nothing else. No `run`, `pull`, `images`, `login`: each needs an engine or state the workspace
does not have, and a verb that half-works is worse than one that is absent. `kl --help` says so
in one line.

The laptop CLI is `kl-connect` from this change on (login, `ws list`, `ws ssh`, `builder status`).
The two binaries share a name prefix and nothing else: different crates, different hosts,
different credentials, no shared code. Sharing a crate would drag TLS, tokio and the api client
into a binary whose whole job is to exec `docker`.

## 3. `kl build`

```
kl build -t NAME[:TAG] [-t …] [-f DOCKERFILE] [--build-arg K=V]… [--platform P] [--no-cache] [CONTEXT]
```

Runs `docker buildx build --builder kl --push`, with every flag passed through, on `CONTEXT`
(default `.`). At least one `-t` is required: with no name there is nothing to push to and the
build would be discarded, which is the exact confusion this tool exists to remove.

**Reference expansion** (`kl::expand`, pure, unit-tested):

| given | pushed as |
|---|---|
| `hello` | `{KL_REGISTRY_HOST}/{KL_OWNER}/hello:latest` |
| `hello:1` | `{KL_REGISTRY_HOST}/{KL_OWNER}/hello:1` |
| `acme/hello:1` | `{KL_REGISTRY_HOST}/acme/hello:1` |
| `ghcr.io/x/y:1`, `localhost:5000/y` | unchanged — first segment carries a `.` or `:` |

The first two rows are the point: the owner never types the registry host or their own slug.
The third lets a team member push under the team; the registry's `may_act` decides whether that
is allowed, exactly as for a git push. The last row is docker's own rule for "this is a host",
kept so a full reference behaves as everywhere else.

**Environment.** `KL_OWNER`, `KL_REGISTRY_HOST` and `BUILDKIT_HOST` are in every workspace pod's
environment (`k8s::login_env`). A missing one is a one-line refusal naming it — it means `kl` is
being run somewhere that is not a workspace.

**Self-sufficient.** Before running the build, `kl` makes sure of the two things
`/etc/profile.d/kl-build.sh` sets up for a login shell — the `kl` buildx builder exists
(`docker buildx create --name kl --driver remote $BUILDKIT_HOST`) and `~/.docker/config.json`
names the credential helper for the registry host — so `kl build` works from any process in the
pod, including a non-login `ws_exec` and an editor's terminal, not only a shell that sourced
`profile.d`. Idempotent: an existing builder or config is left alone. `kl-build.sh` stays for
people who use `docker buildx` directly.

**Output.** buildx's own progress on stderr; on success the last line on stdout is the pushed
reference with its digest (`cr.khost.dev/acme/hello:1@sha256:…`), one per `-t`, read from
buildx's `--metadata-file`. The exit code is buildx's.

**Waiting for the builder.** A cold builder takes up to `builder_start_secs` to answer, and
buildx's remote driver gives the dial ~20 s. `kl build` runs `docker buildx inspect --bootstrap
kl` first and retries it for up to 150 s (the same loop the probe carries today, moved to where
people can benefit from it), printing "starting your builder…" once so the wait is explained.

## 4. `kl push`

```
kl push SRC DST [DST…]
```

There is no local image store, so "push" cannot mean what it means on a laptop. It means
**promote**: copy the image the registry already holds as `SRC` to each `DST`, registry-side,
through `docker buildx imagetools create -t DST SRC` — manifests and layers are re-referenced,
never pulled to the workspace. Both sides go through the same expansion table; `kl push hello:1
hello:latest` is the whole "tag it latest" workflow, and `kl push hello:1 acme/hello:1` hands an
image to the team.

`kl push hello:1` with no destination is refused with the one sentence people need:
"a build pushes as it finishes — `kl build -t hello:1 .`; `kl push` copies an image the
registry already has to another name".

## 5. Packaging

- New crate `bins/kl` (package and binary `kl`). Dependencies: `clap` only. It runs `docker`
  and reads environment variables; that is all a build tool in this position needs, and it is
  what keeps the musl build trivial.
- The workspace image is Alpine, so the binary is built for `x86_64-unknown-linux-musl`
  (`rustup target add`; pure Rust, no C, so no cross toolchain). `deploy/dev/pod/ship.sh` and
  `image.yml` build that one target for that one crate beside the glibc release build;
  `.dockerignore` admits it; the `workspace` Dockerfile stage copies it to `/usr/local/bin/kl`.
- `kl-connect` is unaffected: still glibc, still `kl-connect.yml`, still never deployed.

## 6. What this is not

- Not a daemon. `docker ps/run/pull/images` keep failing and `kl` offers no substitute. If a
  container runtime in workspaces is ever wanted it is a builder-side design (a runtime beside
  buildkitd on the hidden pod), a separate spec.
- Not a wrapper around `docker`. `docker build` and `docker push` behave as the docker CLI
  behaves; `kl` is the documented path, not a shim that changes another tool's meaning.
- Not a second api client. `kl` holds no token and calls no `/v1` route; `kl-connect builder
  status` remains the window onto the builder's state.

## 7. Verification

- Unit: `expand` (the four rows above plus `KL_OWNER` with a team slug), argv construction for
  both verbs, the "no destination" refusal text.
- Fleet, hourly: `ws.build.p95` switches its script to `kl build -t slo-build:{run_id} /tmp/d`
  from a non-login exec, so the probe runs what a person runs and proves the self-sufficiency
  clause. A new `ws.build.promote` step (target 99.9 % ≤ 30 s) runs `kl push slo-build:{run_id}
  slo-build:latest` and reads the digest back with `docker buildx imagetools inspect`. Both ids
  are read by result from `default.otel_logs`.
- The catalogue test holds `deploy/slo.md` equal to the Rust catalogue as before.
