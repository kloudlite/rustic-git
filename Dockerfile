# The dependency compile is the expensive half of this build — aws-lc-sys, ring and zstd-sys are
# C libraries, and together they dominate a cold build. It is also the half that almost never
# changes, so it gets a layer of its own, keyed on Cargo.toml/Cargo.lock alone.
#
# The obvious Dockerfile (COPY src, then cargo build) puts the source INSIDE the layer that
# compiles the dependencies, so every commit invalidates it and every build recompiles the whole
# tree — which is why no cache, local or remote, could help before this split.
# Tag AND digest: the tag says what this is, the digest is what actually builds. Re-resolve with
# `docker buildx imagetools inspect rust:1-bookworm` when bumping.
FROM rust:1-bookworm@sha256:e70e2eec3d495fd5c8e0be74adda86507dfac7f51a724fbf9813ff59b2b247c7 AS chef
RUN cargo install cargo-chef --locked
WORKDIR /src

# The recipe is a description of the dependency graph with the source stripped out: two commits
# with different code but the same Cargo.lock produce the same recipe, hence the same layer.
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY crates ./crates
COPY bins ./bins
COPY tests ./tests
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=planner /src/recipe.json recipe.json
# Dependencies only. Cached until Cargo.lock moves.
# `PROFILE` selects release (default) or `dev-image` (no LTO, 16 codegen units) for the fast dev
# loop. The cook and the build MUST use the same one: a dependency layer cooked under a different
# profile is not reusable, so a mismatch silently recompiles everything it was meant to cache.
ARG PROFILE=release
RUN cargo chef cook --profile ${PROFILE} --locked --recipe-path recipe.json
# Now the source, which changes constantly — but only this last step reruns.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY crates ./crates
COPY bins ./bins
COPY tests ./tests
ARG PROFILE=release
RUN cargo build --profile ${PROFILE} --locked

FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241 AS server
# openssh-client: the server shells out to ssh-keygen to generate its host key on first start.
# git: the merge worker performs merges by running it (see src/merge_worker.rs) — bookworm ships
# 2.39, past the 2.38 that `merge-tree --write-tree` needs. One image serves all three processes,
# so git also lands on the srv and api pods, where nothing runs it; a few MB of unused binary is
# cheaper than a second image to keep in step with this one.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates openssh-client git \
    && rm -rf /var/lib/apt/lists/*
# All three binaries. One image, three processes: the git server, the api server
# and the merge worker are built from the same source and deployed separately, so
# a shared image keeps them on the same commit without coupling their lifecycles.
# Each Deployment picks its process with `command`, so a binary missing here is a
# CrashLoopBackOff there, not a build error.
ARG PROFILE=release
COPY --from=build /src/target/${PROFILE}/rustic-git /usr/local/bin/rustic-git
COPY --from=build /src/target/${PROFILE}/rustic-git-api /usr/local/bin/rustic-git-api
COPY --from=build /src/target/${PROFILE}/rustic-git-worker /usr/local/bin/rustic-git-worker
# Not root. Nothing here needs a capability: the listeners bind 8080/2222/8081/8082, the host
# key lives in a mounted Secret in the cluster, and the pack cache is a directory. The two
# directories the binaries write are created and owned here so a plain `docker run` (no
# mounts) works; in the cluster both are mounts and `fsGroup` on the pod makes them writable.
# uid 1001 matches web/Dockerfile so one securityContext convention serves both images.
RUN useradd --system --uid 1001 --user-group --no-create-home --shell /usr/sbin/nologin rustic \
    && mkdir -p /var/cache/rustic-git /var/lib/rustic-git \
    && chown rustic:rustic /var/cache/rustic-git /var/lib/rustic-git
ENV RUSTIC_GIT_CACHE_DIR=/var/cache/rustic-git RUSTIC_GIT_HOST_KEY=/var/lib/rustic-git/host_key
USER rustic
EXPOSE 8080 2222
ENTRYPOINT ["rustic-git"]
CMD ["serve"]

# The node controller, from the SAME builder stage.
#
# Two images out of one compile. Built as its own Dockerfile it re-ran `cargo build --release` over
# the same workspace in a separate job with a separate cache — the duplicate dominated CI wall time
# (8m18s against the server image's 25s cache hit). A second runtime stage costs one `apt-get`.
#
# Still a separate IMAGE, not a fourth binary in the server one: this runs as root with btrfs-progs
# and the host pool mounted, and shipping root's toolchain to the three processes that must never
# have it is exactly what the split prevents.
FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241 AS agent
# btrfs-progs: every storage operation shells out to it.
# util-linux: losetup/mount for the block-layer restore path.
# ca-certificates: the registry client and Azure blob store speak TLS.
# git: a workspace can be seeded from a platform repository, which the controller clones into the
# fresh subvolume (`VolumeSource::GitRepo`).
# openssh-client: ssh-keygen makes each workspace's SSH host key.
RUN apt-get update && apt-get install -y --no-install-recommends \
      btrfs-progs util-linux ca-certificates git openssh-client \
    && rm -rf /var/lib/apt/lists/*
ARG PROFILE=release
COPY --from=build /src/target/${PROFILE}/rustic-git-agent /usr/local/bin/rustic-git-agent
# Root, deliberately and unlike the server image: btrfs subvolume operations on the host pool are
# not something a capability set can be narrowed to.
ENTRYPOINT ["rustic-git-agent"]

# The SSH gateway, from the SAME builder stage — a third image out of one compile, for the same
# reason the agent is (see above).
#
# Its own image rather than a fourth binary in the server one: this pod runs with
# NET_BIND_SERVICE to hold hostPort 443 on a pool node, and that capability has no business on the
# git server's pods.
FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241 AS gateway
# ca-certificates only: the gateway talks to the kube API server over TLS and to nothing else.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
ARG PROFILE=release
COPY --from=build /src/target/${PROFILE}/rustic-git-gateway /usr/local/bin/rustic-git-gateway
# uid 1001 as in the server image. Binding 443 is a capability on the pod (NET_BIND_SERVICE), not
# a reason to be root — nothing else here needs one, and the binary writes no files at all.
RUN useradd --system --uid 1001 --user-group --no-create-home --shell /usr/sbin/nologin rustic
USER rustic
EXPOSE 443 8080
ENTRYPOINT ["rustic-git-gateway"]
