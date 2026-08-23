# The dependency compile is the expensive half of this build — aws-lc-sys, ring and zstd-sys are
# C libraries, and together they dominate a cold build. It is also the half that almost never
# changes, so it gets a layer of its own, keyed on Cargo.toml/Cargo.lock alone.
#
# The obvious Dockerfile (COPY src, then cargo build) puts the source INSIDE the layer that
# compiles the dependencies, so every commit invalidates it and every build recompiles the whole
# tree — which is why no cache, local or remote, could help before this split.
FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /src

# The recipe is a description of the dependency graph with the source stripped out: two commits
# with different code but the same Cargo.lock produce the same recipe, hence the same layer.
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=planner /src/recipe.json recipe.json
# Dependencies only. Cached until Cargo.lock moves.
RUN cargo chef cook --release --locked --recipe-path recipe.json
# Now the source, which changes constantly — but only this last step reruns.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
# openssh-client: the server shells out to ssh-keygen to generate its host key on first start
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates openssh-client \
    && rm -rf /var/lib/apt/lists/*
# All three binaries. One image, three processes: the git server, the api server
# and the merge worker are built from the same source and deployed separately, so
# a shared image keeps them on the same commit without coupling their lifecycles.
# Each Deployment picks its process with `command`, so a binary missing here is a
# CrashLoopBackOff there, not a build error.
COPY --from=build /src/target/release/rustic-git /usr/local/bin/rustic-git
COPY --from=build /src/target/release/rustic-git-api /usr/local/bin/rustic-git-api
COPY --from=build /src/target/release/rustic-git-worker /usr/local/bin/rustic-git-worker
# Not root. Nothing here needs a capability: the listeners bind 8080/2222/8081/8082, the host
# key lives in a mounted Secret in the cluster, and the pack cache is a directory. The two
# directories the binaries write are created and owned here so a plain `docker run` (no
# mounts) works; in the cluster both are mounts and `fsGroup` on the pod makes them writable.
# uid 1001 matches web/Dockerfile so one securityContext convention serves both images.
RUN useradd --system --uid 1001 --no-create-home --shell /usr/sbin/nologin rustic \
    && mkdir -p /var/cache/rustic-git /var/lib/rustic-git \
    && chown rustic:rustic /var/cache/rustic-git /var/lib/rustic-git
ENV RUSTIC_GIT_CACHE_DIR=/var/cache/rustic-git RUSTIC_GIT_HOST_KEY=/var/lib/rustic-git/host_key
USER rustic
EXPOSE 8080 2222
ENTRYPOINT ["rustic-git"]
CMD ["serve"]
