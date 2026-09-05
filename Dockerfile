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
# deploy/k3s/dev-push.sh loop. Only the five kloudlite binaries make it into the context — see
# .dockerignore — so a fat `target/` costs nothing to send.

FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241 AS server
# openssh-client: the server shells out to ssh-keygen to generate its host key on first start.
# git: the merge worker performs merges by running it (see crates/pulls/src/merge_worker.rs) —
# bookworm ships 2.39, past the 2.38 that `merge-tree --write-tree` needs. One image serves all
# three processes, so git also lands on the srv and api pods, where nothing runs it; a few MB of
# unused binary is cheaper than a second image to keep in step with this one.
# curl: the srv preStop hook POSTs /peer/v1/drain to its own peer port to hand ownership over
# before the pod goes (deploy/kloudlite.yaml). Nothing in the processes shells out to it.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates openssh-client git curl \
    && rm -rf /var/lib/apt/lists/*
# All three binaries. One image, three processes: the git server, the api server
# and the merge worker are built from the same source and deployed separately, so
# a shared image keeps them on the same commit without coupling their lifecycles.
# Each Deployment picks its process with `command`, so a binary missing here is a
# CrashLoopBackOff there, not a build error.
ARG PROFILE=release
COPY target/${PROFILE}/kloudlite /usr/local/bin/kloudlite
COPY target/${PROFILE}/kloudlite-api /usr/local/bin/kloudlite-api
COPY target/${PROFILE}/kloudlite-worker /usr/local/bin/kloudlite-worker
# Not root. Nothing here needs a capability: the listeners bind 8080/2222/8081/8082, the host
# key lives in a mounted Secret in the cluster, and the pack cache is a directory. The two
# directories the binaries write are created and owned here so a plain `docker run` (no
# mounts) works; in the cluster both are mounts and `fsGroup` on the pod makes them writable.
# uid 1001 matches web/Dockerfile so one securityContext convention serves both images.
RUN useradd --system --uid 1001 --user-group --no-create-home --shell /usr/sbin/nologin kloudlite \
    && mkdir -p /var/cache/kloudlite /var/lib/kloudlite \
    && chown kloudlite:kloudlite /var/cache/kloudlite /var/lib/kloudlite
ENV KLOUDLITE_CACHE_DIR=/var/cache/kloudlite KLOUDLITE_HOST_KEY=/var/lib/kloudlite/host_key
USER kloudlite
EXPOSE 8080 2222
ENTRYPOINT ["kloudlite"]
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
COPY target/${PROFILE}/kloudlite-agent /usr/local/bin/kloudlite-agent
# Root, deliberately and unlike the server image: btrfs subvolume operations on the host pool are
# not something a capability set can be narrowed to.
ENTRYPOINT ["kloudlite-agent"]

# The SSH gateway. Its own image rather than a fourth binary in the server one: this pod runs with
# NET_BIND_SERVICE to hold hostPort 443 on a pool node, and that capability has no business on the
# git server's pods.
FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241 AS gateway
# ca-certificates only: the gateway talks to the kube API server over TLS and to nothing else.
# libcap2-bin is build-time only, for the setcap below.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libcap2-bin \
    && rm -rf /var/lib/apt/lists/*
ARG PROFILE=release
COPY target/${PROFILE}/kloudlite-gateway /usr/local/bin/kloudlite-gateway
# The FILE capability is what actually lets uid 1001 bind 443, and it is not optional.
# `capabilities.add: [NET_BIND_SERVICE]` in the pod spec sets the container's BOUNDING and
# permitted sets — but execve empties the permitted set of a non-root process unless the binary
# itself carries the capability, and Kubernetes has no way to request ambient capabilities. So the
# pod grant alone yields EACCES on 443; the pod grant plus this file capability is what works, and
# neither half is sufficient on its own (the bounding set must still permit it).
RUN setcap cap_net_bind_service=+ep /usr/local/bin/kloudlite-gateway \
    && apt-get purge -y libcap2-bin && apt-get autoremove -y
# uid 1001 as in the server image: the binary writes no files and needs no other privilege.
RUN useradd --system --uid 1001 --user-group --no-create-home --shell /usr/sbin/nologin kloudlite
USER kloudlite
EXPOSE 443 8080
ENTRYPOINT ["kloudlite-gateway"]

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

# The SLO probe. Its own image because it is the only one that carries a toolbox — git, ssh,
# crane, kubectl, dig, openssl — and shipping that to the three server processes would hand a
# compromised request handler everything it needs to talk to the cluster.
FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241 AS slo
# git + openssh-client: stage 2 pushes and clones over both transports, with a real client, because
# a probe that used our own library would pass on a bug only a real client trips.
# curl is the build-time tool fetch below; the edge stage dials the origin with reqwest.
# openssl + bind9-dnsutils: `edge.cert` reads the served certificate and `edge.dns` resolves the
# hostnames without trusting the pod's resolver cache.
# bash: the edge stage opens `/dev/tcp/host/port` to prove a listener answers, which is a bash
# builtin — debian's default /bin/sh is dash and has no such thing. Explicit rather than relying
# on bookworm-slim shipping it, because a base-image slim-down would break the edge stage silently.
RUN apt-get update && apt-get install -y --no-install-recommends \
      bash ca-certificates git openssh-client curl openssl bind9-dnsutils \
    && rm -rf /var/lib/apt/lists/*
# crane and kubectl are pinned by CONTENT, not by tag: both are fetched from a release URL a
# third party controls, and a moved tag would silently change what the probe runs. A checksum
# mismatch fails the build, which is the only place it can be caught.
ARG CRANE_VERSION=v0.20.3
ARG CRANE_SHA256=36c67a932f489b3f2724b64af90b599a8ef2aa7b004872597373c0ad694dc059
ARG KUBECTL_VERSION=v1.31.5
ARG KUBECTL_SHA256=fbecbfd375b3686002c2e81d51c390172f5ffba3d6b47920d55342cb03f557af
RUN set -eux; \
    curl -fsSL -o /tmp/crane.tgz "https://github.com/google/go-containerregistry/releases/download/${CRANE_VERSION}/go-containerregistry_Linux_x86_64.tar.gz"; \
    echo "${CRANE_SHA256}  /tmp/crane.tgz" | sha256sum -c -; \
    tar -xzf /tmp/crane.tgz -C /usr/local/bin crane; \
    curl -fsSL -o /usr/local/bin/kubectl "https://dl.k8s.io/release/${KUBECTL_VERSION}/bin/linux/amd64/kubectl"; \
    echo "${KUBECTL_SHA256}  /usr/local/bin/kubectl" | sha256sum -c -; \
    chmod +x /usr/local/bin/crane /usr/local/bin/kubectl; \
    rm -f /tmp/crane.tgz
ARG PROFILE=release
COPY target/${PROFILE}/kloudlite-slo /usr/local/bin/kloudlite-slo
# `kl` is the user CLI, built by the same `cargo build`: stage 1's `id.cli.flow` and stage 5's
# tunnel checks exercise the CLI a person actually runs, not a reimplementation of it.
COPY target/${PROFILE}/kl /usr/local/bin/kl
# uid 1001 as everywhere else. No home directory: the pod's root filesystem is read-only and
# everything git, ssh and crane write goes under the /tmp emptyDir (HOME is set to it in the
# CronJob), so a home baked in here would only be a read-only trap.
RUN useradd --system --uid 1001 --user-group --no-create-home --shell /usr/sbin/nologin kloudlite
USER kloudlite
ENTRYPOINT ["kloudlite-slo"]
