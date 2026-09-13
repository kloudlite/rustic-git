# The bench image: pi (the coding agent), kl (workspace CLI, for a bench to build/push like any
# workspace) and harness-bench (real binary once harness/bench/dist/ exists, else this plan's stub —
# same interface either way, see harness-bench-stub.sh). Built by .github/workflows/image.yml,
# context ".", same as the root Dockerfile's stages, so it can reach target/x86_64.../kl.
FROM node:22-bookworm-slim AS harness-bin
WORKDIR /out
RUN --mount=type=bind,target=/ctx \
    if [ -f /ctx/harness/bench/dist/harness-bench ]; then \
      cp /ctx/harness/bench/dist/harness-bench harness-bench; \
    else \
      cp /ctx/deploy/bench/harness-bench-stub.sh harness-bench; \
    fi \
    && chmod +x harness-bench

FROM node:22-bookworm-slim
# util-linux: flock. socat: the 7789 listener. procps: pgrep, for --ping (see the stub's own
# comment on why --ping must not dial the listener).
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates util-linux socat procps \
    && rm -rf /var/lib/apt/lists/*

# Pinned to the exact version harness/package-lock.json locks, so the image never drifts from
# what the harness was built and tested against.
RUN npm i -g @mariozechner/pi-coding-agent@0.73.1

# musl binary — statically linked, so it runs fine on this glibc base — built by the same CI job
# that builds this image and downloaded as the kl-musl artifact.
COPY target/x86_64-unknown-linux-musl/release/kl /usr/local/bin/kl
RUN chmod 0755 /usr/local/bin/kl

COPY --from=harness-bin /out/harness-bench /usr/local/bin/harness-bench
RUN chmod 0755 /usr/local/bin/harness-bench

# node:22-bookworm-slim already ships uid/gid 1000 as `node` with /home/node — reuse it rather
# than useradd a second uid 1000, which apt's base image install (nss-wrapper aside) refuses.
RUN mkdir -p /bench /home/kl && chown 1000:1000 /bench /home/kl

USER 1000:1000
WORKDIR /home/kl
# The pod sets `command` (harness-bench, or harness-bench --ping via the probe); no default here.
ENTRYPOINT []
