# Builds go to the owner's builder through the gate; the credential helper is the login. `kl`
# does this setup itself; this is for people who call `docker buildx` directly.
# Never fails the shell that runs it: every branch below falls through. It is RUN by the zsh and fish
# rc files, but /etc/profile SOURCES every /etc/profile.d/*.sh into a POSIX login shell, so a
# trailing `exit` here ended every `sh -l`/`bash -l` before its command ran (build fb3673f1).
if [ -n "${BUILDKIT_HOST:-}" ] && [ -n "${KL_REGISTRY_HOST:-}" ] && command -v docker >/dev/null 2>&1; then
  mkdir -p "$HOME/.docker"
  [ -e "$HOME/.docker/config.json" ] || printf '{"credHelpers":{"%s":"kl"}}\n' "$KL_REGISTRY_HOST" > "$HOME/.docker/config.json"
  docker buildx inspect kl >/dev/null 2>&1 || docker buildx create --name kl --driver remote "$BUILDKIT_HOST" --use >/dev/null 2>&1 || true
fi
