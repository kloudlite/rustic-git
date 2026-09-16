# Builds go to the owner's builder through the gate; the credential helper is the login. `kl`
# does this setup itself; this is for people who call `docker buildx` directly.
# Never fails the shell that runs it: every branch below falls through. It is RUN by the zsh and fish
# rc files, but /etc/profile SOURCES every /etc/profile.d/*.sh into a POSIX login shell, so a
# trailing `exit` here ended every `sh -l`/`bash -l` before its command ran (build fb3673f1).
if [ -n "${BUILDKIT_HOST:-}" ] && [ -n "${KL_REGISTRY_HOST:-}" ] && command -v docker >/dev/null 2>&1; then
  mkdir -p "$HOME/.docker"
  [ -e "$HOME/.docker/config.json" ] || printf '{"credHelpers":{"%s":"kl"}}\n' "$KL_REGISTRY_HOST" > "$HOME/.docker/config.json"
  # No `docker buildx create` here: with the remote driver that dials the builder gate, which starts
  # the owner's buildkitd on demand — up to builder_start_secs of a LOGIN SHELL blocked on it (the
  # 2026-09-16 bench.shell.workspace probe timed out at 30 s on exactly this). `kl build` makes
  # the builder itself, idempotently, the first time somebody builds (docker::ensure_builder).
fi
