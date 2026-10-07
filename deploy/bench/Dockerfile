# The bench image: the graphcode TUI (under bun), ttyd and sshd, all supervised by runit
# (pid 1 is `runsvdir`, see `bench/sv/*/run`), plus the `sessions` node service, the pod's
# idle/readiness probe only. Built by
# .github/workflows/image.yml, context ".", same as the root Dockerfile's stages, so it can reach
# target/x86_64.../kl.
#
# Node 24 (plan R5: overrides the spec's 22) — .ts type stripping is unflagged, needed by the
# `sessions` service.
FROM node:24-bookworm-slim

# ca-certificates: the TUI and `kl` both talk TLS. runit: the pid-1 supervisor (R2/R3) —
# a crash in ttyd/sshd/sessions restarts in place, no second pod phase to fall back to.
# openssh-server: `sshd`, each login runs the graphcode TUI via `ForceCommand` in bench/sshd_config.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates runit openssh-server \
    && rm -rf /var/lib/apt/lists/*

# ttyd: the browser terminal, one graphcode TUI per connection. No Debian release packages it
# (bookworm and trixie both have no candidate), so the upstream static build, pinned by digest.
ADD --checksum=sha256:8a217c968aba172e0dbf3f34447218dc015bc4d5e59bf51db2f2cd12b7be4f55 \
    https://github.com/tsl0922/ttyd/releases/download/1.7.7/ttyd.x86_64 /usr/local/bin/ttyd
RUN chmod 0755 /usr/local/bin/ttyd

# bun runs the graphcode TUI (kloudlite/harness, pinned by commit; bump here and in
# deploy/dagger/src/index.ts HARNESS_REV). Deps install in the image: @opentui/core is native.
COPY --from=oven/bun:1.3.14@sha256:e10577f0db68676a7024391c6e5cb4b879ebd17188ab750cf10024a6d700e5c4 /usr/local/bin/bun /usr/local/bin/bun
ADD https://github.com/kloudlite/harness.git#c71c2cd7f115e2fb62782ff47714590dd5fb01fb /opt/kl/harness
RUN cd /opt/kl/harness && bun install --frozen-lockfile

# The musl `kl` binary — statically linked, so it runs fine on this glibc base — built by the same
# CI job that builds this image and downloaded as the kl-musl artifact. A bench builds/pushes like
# any workspace.
COPY target/x86_64-unknown-linux-musl/release/kl /usr/local/bin/kl
RUN chmod 0755 /usr/local/bin/kl

# The `sessions` node service: `/idle` on 127.0.0.1:8917, answers `--ping`.
COPY bench/sessions/package.json bench/sessions/package-lock.json /opt/kl/sessions/
RUN cd /opt/kl/sessions && npm ci --omit=dev
COPY bench/sessions /opt/kl/sessions

# The browser terminal's front door (image paste from the person's clipboard, see main.ts) and the
# `xclip` stand-in the TUI reads that image through.
COPY bench/term /opt/kl/term
RUN install -m 0755 /opt/kl/term/xclip /usr/local/bin/xclip

# runit service directories: each `run` script is `exec`'d last, so runit supervises the real
# process directly (no wrapper pid to lose signals through). Owned by kl: runsv runs as uid 1000
# and creates `supervise/` beside each `run` (root-owned, every service died on supervise/lock).
COPY --chown=1000:1000 bench/sv /etc/kl/sv
RUN chmod 0755 /etc/kl/sv/*/run

COPY bench/sshd_config /etc/kl/

# TMPDIR: a private dir under the shared /tmp emptyDir, made by bench/sv/sessions/run.
ENV TMPDIR=/tmp/kl

# node:24-bookworm-slim ships uid/gid 1000 as `node`; rename rather than useradd a second uid 1000
# (plan R3). Home is the workspace volume (2026-09-22 ruling): /home/kl IS the mounted volume.
# No `tty` group: sshd runs as kl, and OpenSSH (sshpty.c pty_setowner) fatals when it cannot chown a
# new pty to that group, dropping every `ssh -t` session right after auth. Without the group it
# wants kl:kl 0600, which the pty already is.
RUN usermod -l kl -d /home/kl node && groupmod -n kl node && groupdel tty \
    && mkdir -p /home/kl && chown kl:kl /home/kl

USER 1000:1000
WORKDIR /home/kl
# The pod sets `command` (runsvdir /etc/kl/sv); no default here.
ENTRYPOINT []
