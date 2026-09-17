# The bench image: harness-bench (the real program, run from its TypeScript source by Node's type
# stripping), pi (the coding agent, from the harness's own lockfile — the child and the reader SDK
# are one version) and kl (workspace CLI, for a bench to build/push like any workspace). Built by
# .github/workflows/image.yml, context ".", same as the root Dockerfile's stages, so it can reach
# target/x86_64.../kl.
FROM node:24-bookworm-slim AS deps
WORKDIR /opt/harness
COPY harness/package.json harness/package-lock.json ./
# No toolchain: nothing left in the bench's dependency tree builds from source (node-pty went with
# the pty — a bench shell is now the workspace container's, spliced over 127.0.0.1:7788).
RUN npm ci --omit=dev

# Node 24: .ts type stripping is unflagged, and it is what the bench suite ran on.
FROM node:24-bookworm-slim
# util-linux: flock. No shell furniture — harness-bench spawns no shell of its own; `/pty` splices
# to the workspace container's tool server, which has the person's Nix profile and prompt.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates util-linux \
    && rm -rf /var/lib/apt/lists/*

# musl binary — statically linked, so it runs fine on this glibc base — built by the same CI job
# that builds this image and downloaded as the kl-musl artifact.
COPY target/x86_64-unknown-linux-musl/release/kl /usr/local/bin/kl
RUN chmod 0755 /usr/local/bin/kl

# The harness tree, never under a node_modules path (type stripping refuses those). rpc-child's
# default pi is ../../node_modules/.bin/pi from bench/src, i.e. /opt/harness/node_modules/.bin/pi,
# and its extensions default to /opt/harness/pi — so no HARNESS_PI_* env is needed.
COPY harness/package.json harness/package-lock.json /opt/harness/
COPY --from=deps /opt/harness/node_modules /opt/harness/node_modules
COPY harness/bench/package.json /opt/harness/bench/package.json
COPY harness/bench/src /opt/harness/bench/src
COPY harness/pi /opt/harness/pi
COPY harness/skills /opt/harness/skills
# Node resolves the symlink, so bench/package.json's "type":"module" applies.
RUN chmod 0755 /opt/harness/bench/src/main.ts \
    && ln -s /opt/harness/bench/src/main.ts /usr/local/bin/harness-bench

# node:24-bookworm-slim already ships uid/gid 1000 as `node` — reuse it rather than useradd a
# second uid 1000. harness-bench writes only its --dir, $HOME and /tmp, all mounts in the pod.
RUN mkdir -p /bench /home/kl && chown 1000:1000 /bench /home/kl

USER 1000:1000
WORKDIR /home/kl
# The pod sets `command` (harness-bench, or harness-bench --ping via the probe); no default here.
ENTRYPOINT []
