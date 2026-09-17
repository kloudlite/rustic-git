# The shell image: a terminal and nothing else.
#
# Every workspace pod and every bench pod carries one of these beside its main container
# (spec §2, 2026-09-17). It mounts the HOME and the Nix profile — never the workspace directory,
# never a token, never `/opt/harness` — so a person has a shell for their own convenience and no
# session has one where it runs. The binaries a person types (zsh, starship, coreutils, ttyd) all
# come from the workspace's own profile, so this image holds almost nothing: `kl`, the rc files,
# and the prelude that waits for the profile and execs ttyd.
#
# No sshd (ssh lands in the workspace container), no `kl ide serve`, no harness, no credential.
# Built by .github/workflows/image.yml, context ".", so it reaches the kl-musl artifact the same
# way deploy/bench/Dockerfile does.
FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241

# ca-certificates for `kl`'s HTTPS; nothing else. The shell's own tools are the profile's.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd -g 1000 kl \
    && useradd -u 1000 -g 1000 -m -d /home/kl -s /bin/sh -p '*' kl

# The same static musl binary the workspace and bench images carry, from the same CI artifact.
COPY target/x86_64-unknown-linux-musl/release/kl /usr/local/bin/kl
RUN chmod 0755 /usr/local/bin/kl

# Derived state the platform places inside a workspace directory, ignored by git globally — the
# same file the workspace image ships, so a repo cloned from either shell behaves identically.
COPY deploy/workspace-image/gitignore-global /etc/kloudlite/gitignore-global

COPY deploy/shell-image/prelude.sh /usr/local/bin/prelude
RUN chmod 0755 /usr/local/bin/prelude

# uid 1000, like every other tenant container: the home is the person's, and nothing here needs
# root once the image is built.
USER 1000:1000
ENTRYPOINT ["/usr/local/bin/prelude"]
