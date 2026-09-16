# The bench image: harness-bench (the real program, run from its TypeScript source by Node's type
# stripping), pi (the coding agent, from the harness's own lockfile — the child and the reader SDK
# are one version) and kl (workspace CLI, for a bench to build/push like any workspace). Built by
# .github/workflows/image.yml, context ".", same as the root Dockerfile's stages, so it can reach
# target/x86_64.../kl.
FROM node:24-bookworm-slim AS deps
WORKDIR /opt/harness
COPY harness/package.json harness/package-lock.json ./
# node-pty publishes no linux prebuild, so `npm ci` builds it from source here. Build stage only —
# the runtime stage copies the compiled node_modules and needs none of this.
RUN apt-get update && apt-get install -y --no-install-recommends python3 make g++ \
    && rm -rf /var/lib/apt/lists/*
RUN npm ci --omit=dev

# Node 24: .ts type stripping is unflagged, and it is what the bench suite ran on.
FROM node:24-bookworm-slim
# util-linux: flock. zsh: the bench shell, so a bench prompt looks like a workspace prompt.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates util-linux zsh \
    && rm -rf /var/lib/apt/lists/*

# starship, from its release tarball rather than the install script: the script resolves "latest"
# at build time, which makes the image unreproducible and unpinnable. Version and digest move
# together, by hand.
ARG STARSHIP_VERSION=1.26.0
ARG STARSHIP_SHA256=b7c232b0e8249d8e55a40beb79c5c43a7d370f3f9408bd215deb0170daeaadf3
ADD https://github.com/starship/starship/releases/download/v${STARSHIP_VERSION}/starship-x86_64-unknown-linux-musl.tar.gz /tmp/starship.tar.gz
RUN echo "${STARSHIP_SHA256}  /tmp/starship.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/starship.tar.gz -C /usr/local/bin starship \
    && chmod 0755 /usr/local/bin/starship && rm /tmp/starship.tar.gz

# The same bytes the workspace pod prelude writes (crates/workspaces/src/k8s/shell_rc.rs, held
# equal by `shell_rc::tests::bench_rc_files_match`), so the two images cannot drift apart.
COPY deploy/bench/zshrc /etc/zsh/zshrc
COPY deploy/bench/starship.toml /etc/starship.toml

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
# Node resolves the symlink, so bench/package.json's "type":"module" applies.
RUN chmod 0755 /opt/harness/bench/src/main.ts \
    && ln -s /opt/harness/bench/src/main.ts /usr/local/bin/harness-bench

# node:24-bookworm-slim already ships uid/gid 1000 as `node` — reuse it rather than useradd a
# second uid 1000. harness-bench writes only /bench, $HOME and /tmp, all mounts in the pod.
RUN mkdir -p /bench /home/kl && chown 1000:1000 /bench /home/kl

USER 1000:1000
WORKDIR /home/kl
# The pod sets `command` (harness-bench, or harness-bench --ping via the probe); no default here.
ENTRYPOINT []
