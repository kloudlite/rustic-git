# The bench image: the agent CLI (@anthropic-ai/claude-code), tmux, ttyd and sshd, all supervised
# by runit (pid 1 is `runsvdir`, see `bench/sv/*/run`), plus the `sessions` node service that owns
# the pod's WebSocket/HTTP surface and the `kloudlite` mod it loads as a plugin. Built by
# .github/workflows/image.yml, context ".", same as the root Dockerfile's stages, so it can reach
# target/x86_64.../kl.
#
# Node 24 (plan R4: overrides the spec's 22) — .ts type stripping is unflagged, needed by the
# `sessions` service and the mod.
FROM node:24-bookworm-slim

# ca-certificates: the agent CLI and `kl` both talk TLS. runit: the pid-1 supervisor (R2/R3) —
# a crash in tmux/ttyd/sshd/sessions restarts in place, no second pod phase to fall back to. tmux:
# the one session a person's ssh/ttyd logins and the agent CLI all share. ttyd: the browser
# terminal, attaches to that same tmux session. openssh-server: `sshd`, same tmux session via
# `ForceCommand` in bench/sshd_config.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates runit tmux ttyd openssh-server \
    && rm -rf /var/lib/apt/lists/*

# Pinned exact (plan R5): `npm view @anthropic-ai/claude-code version` on 2026-10-05.
RUN npm install -g @anthropic-ai/claude-code@2.1.289

# The musl `kl` binary — statically linked, so it runs fine on this glibc base — built by the same
# CI job that builds this image and downloaded as the kl-musl artifact. A bench builds/pushes like
# any workspace.
COPY target/x86_64-unknown-linux-musl/release/kl /usr/local/bin/kl
RUN chmod 0755 /usr/local/bin/kl

# The `sessions` node service: owns the WebSocket/HTTP surface on 127.0.0.1:8917, answers `--ping`.
COPY bench/sessions/package.json bench/sessions/package-lock.json /opt/kl/sessions/
RUN cd /opt/kl/sessions && npm ci --omit=dev
COPY bench/sessions /opt/kl/sessions

# The `kloudlite` mod: a Claude Code plugin read via CLAUDE_CODE_PLUGIN_DIRS below.
COPY bench/mod /opt/kl/mod

# runit service directories: each `run` script is `exec`'d last, so runit supervises the real
# process directly (no wrapper pid to lose signals through).
COPY bench/sv /etc/kl/sv
RUN chmod 0755 /etc/kl/sv/*/run

COPY bench/sshd_config /etc/kl/sshd_config

# Managed settings: `apiKeyHelper` reads the provider token off the mounted `user-key` Secret, so
# the token is never an env var (ps e, crash dumps, child processes all see env).
RUN mkdir -p /etc/claude-code
COPY bench/settings.json /etc/claude-code/managed-settings.json

ENV CLAUDE_CODE_PLUGIN_DIRS=/opt/kl/mod

# node:24-bookworm-slim ships uid/gid 1000 as `node`; rename rather than useradd a second uid 1000
# (plan R3). Home is the workspace volume (2026-09-22 ruling): /home/kl IS the mounted volume.
RUN usermod -l kl -d /home/kl node && groupmod -n kl node \
    && mkdir -p /home/kl && chown kl:kl /home/kl

USER 1000:1000
WORKDIR /home/kl
# The pod sets `command` (runsvdir /etc/kl/sv); no default here.
ENTRYPOINT []
