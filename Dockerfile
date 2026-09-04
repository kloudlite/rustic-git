# Runtime-only. The binaries are compiled OUTSIDE docker and copied in from `target/${PROFILE}/`:
#
#   cargo build --release --locked && docker build --target server .   # or agent, gateway
#
# They used to be compiled inside (cargo-chef, `type=gha` cache in `mode=max`). That design cached
# the dependency cook but never the workspace crates — those lived in the layer that also held the
# source, so every commit rebuilt them from scratch: 342 s of `cargo build --release` on EVERY run
# plus 114 s exporting the layer cache, before a single image was pushed. CI now compiles once on
# the runner under Swatinem/rust-cache (incremental across commits) and this file is only the
# apt-get and the COPY. See .github/workflows/image.yml.
#
# The compile MUST link against a glibc no newer than bookworm's 2.36, because these stages are
# bookworm-slim; CI builds inside `rust:1-bookworm` and checks the binary's GLIBC_ requirement.
# Building on a newer host (ubuntu 24.04, glibc 2.39) yields an image that dies at exec.
#
# `PROFILE` names the target dir the binaries come from: `release` (default) or `dev-image` for the
# deploy/k3s/dev-push.sh loop. Only the five kloudlite-git binaries make it into the context — see
# .dockerignore — so a fat `target/` costs nothing to send.

FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241 AS server
# openssh-client: the server shells out to ssh-keygen to generate its host key on first start.
# git: the merge worker performs merges by running it (see crates/pulls/src/merge_worker.rs) —
# bookworm ships 2.39, past the 2.38 that `merge-tree --write-tree` needs. One image serves all
# three processes, so git also lands on the srv and api pods, where nothing runs it; a few MB of
# unused binary is cheaper than a second image to keep in step with this one.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates openssh-client git \
    && rm -rf /var/lib/apt/lists/*
# All three binaries. One image, three processes: the git server, the api server
# and the merge worker are built from the same source and deployed separately, so
# a shared image keeps them on the same commit without coupling their lifecycles.
# Each Deployment picks its process with `command`, so a binary missing here is a
# CrashLoopBackOff there, not a build error.
ARG PROFILE=release
COPY target/${PROFILE}/kloudlite-git /usr/local/bin/kloudlite-git
COPY target/${PROFILE}/kloudlite-git-api /usr/local/bin/kloudlite-git-api
COPY target/${PROFILE}/kloudlite-git-worker /usr/local/bin/kloudlite-git-worker
# Not root. Nothing here needs a capability: the listeners bind 8080/2222/8081/8082, the host
# key lives in a mounted Secret in the cluster, and the pack cache is a directory. The two
# directories the binaries write are created and owned here so a plain `docker run` (no
# mounts) works; in the cluster both are mounts and `fsGroup` on the pod makes them writable.
# uid 1001 matches web/Dockerfile so one securityContext convention serves both images.
RUN useradd --system --uid 1001 --user-group --no-create-home --shell /usr/sbin/nologin kloudlite \
    && mkdir -p /var/cache/kloudlite-git /var/lib/kloudlite-git \
    && chown kloudlite:kloudlite /var/cache/kloudlite-git /var/lib/kloudlite-git
ENV KLOUDLITE_GIT_CACHE_DIR=/var/cache/kloudlite-git KLOUDLITE_GIT_HOST_KEY=/var/lib/kloudlite-git/host_key
USER kloudlite
EXPOSE 8080 2222
ENTRYPOINT ["kloudlite-git"]
CMD ["serve"]

# The node controller. A separate IMAGE, not a fourth binary in the server one: this runs as root
# with btrfs-progs and the host pool mounted, and shipping root's toolchain to the three processes
# that must never have it is exactly what the split prevents.
FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241 AS agent
# btrfs-progs: every storage operation shells out to it.
# util-linux: losetup/mount for the block-layer restore path.
# ca-certificates: the registry client and Azure blob store speak TLS.
# git: a workspace can be seeded from a platform repository, which the controller clones into the
# fresh subvolume (`VolumeSource::GitRepo`).
# openssh-client: ssh-keygen makes each workspace's SSH host key.
# nfs-common: `mount -t nfs` is not built into mount(8) — it execs /sbin/mount.nfs, which ships
# here. Without it the agent's shared-home mount fails with "bad option; ... you might need a
# /sbin/mount.<type> helper program" and, because that mount is fail-closed, the agent refuses to
# start at all rather than serving anyone an empty home.
# netbase: /etc/protocols and /etc/services. `--no-install-recommends` leaves them out, and
# without /etc/protocols mount.nfs cannot resolve `proto=tcp` ("Failed to find 'tcp' protocol"),
# silently abandons the v3 negotiation it was told to use, and asks the server for v4 instead —
# which ZeroFS rejects as "Invalid NFS Version number 4 != 3" and the client reports, uselessly,
# as "Protocol not supported". Two lost deploys came from that chain; keep this package.
RUN apt-get update && apt-get install -y --no-install-recommends \
      btrfs-progs util-linux ca-certificates git openssh-client nfs-common netbase \
    && rm -rf /var/lib/apt/lists/*
ARG PROFILE=release
COPY target/${PROFILE}/kloudlite-git-agent /usr/local/bin/kloudlite-git-agent
# Root, deliberately and unlike the server image: btrfs subvolume operations on the host pool are
# not something a capability set can be narrowed to.
ENTRYPOINT ["kloudlite-git-agent"]

# The SSH gateway. Its own image rather than a fourth binary in the server one: this pod runs with
# NET_BIND_SERVICE to hold hostPort 443 on a pool node, and that capability has no business on the
# git server's pods.
FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241 AS gateway
# ca-certificates only: the gateway talks to the kube API server over TLS and to nothing else.
# libcap2-bin is build-time only, for the setcap below.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libcap2-bin \
    && rm -rf /var/lib/apt/lists/*
ARG PROFILE=release
COPY target/${PROFILE}/kloudlite-git-gateway /usr/local/bin/kloudlite-git-gateway
# The FILE capability is what actually lets uid 1001 bind 443, and it is not optional.
# `capabilities.add: [NET_BIND_SERVICE]` in the pod spec sets the container's BOUNDING and
# permitted sets — but execve empties the permitted set of a non-root process unless the binary
# itself carries the capability, and Kubernetes has no way to request ambient capabilities. So the
# pod grant alone yields EACCES on 443; the pod grant plus this file capability is what works, and
# neither half is sufficient on its own (the bounding set must still permit it).
RUN setcap cap_net_bind_service=+ep /usr/local/bin/kloudlite-git-gateway \
    && apt-get purge -y libcap2-bin && apt-get autoremove -y
# uid 1001 as in the server image: the binary writes no files and needs no other privilege.
RUN useradd --system --uid 1001 --user-group --no-create-home --shell /usr/sbin/nologin kloudlite
USER kloudlite
EXPOSE 443 8080
ENTRYPOINT ["kloudlite-git-gateway"]

# The default workspace image: what `ws-{id}` runs when a workspace names no image of its own.
# Stock alpine plus exactly what the platform itself needs and cannot get from Nix:
#   - libstdc++/libgcc: VS Code Remote-SSH's Alpine server ships a musl `node` that still
#     dlopens both; without them every connect downloads the server and dies with
#     "Error relocating … libstdc++". Nix's copies are glibc-linked and useless to a musl binary.
#   - the `kl` account to log in as and sshd's chroot dir (alpine already ships the `sshd`
#     account it drops privileges to).
#     busybox `adduser -D` writes `!` as the password, which sshd reads as "locked" and refuses
#     even a valid key; `*` is "no password" and is not locked. The login shell is the Nix
#     profile's zsh, mounted at run time — adduser does not check that the path exists yet.
#   - the greeting.
# Everything a person actually uses (git, zsh, fish, starship, …) comes from the Nix profile the
# agent builds per workspace and mounts read-only, so this image stays stock apart from the above.
# Runtime steps that depend on mounts (chown of the volume, seeding rc files, exec sshd) live in
# `k8s::prelude`, not here.
FROM alpine:3.20 AS workspace
RUN apk add --no-cache libstdc++ libgcc \
    && mkdir -p /var/empty \
    && adduser -D -u 1000 -s /nix/profile/current/bin/zsh kl \
    && sed -i 's/^kl:!:/kl:*:/' /etc/shadow \
    && printf '%s\n' 'Kloudlite workspace — you are kl (no root, no sudo).' > /etc/motd
