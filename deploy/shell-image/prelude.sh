#!/bin/sh
# The shell container's whole program: wait for the workspace's Nix profile, then hand the port to
# ttyd. Nothing else runs here — no sshd, no tool server, no harness.
#
# The profile is the AGENT's to publish (`{profiles_dir}/{id}/current`, mounted read-only at
# /nix/profile); a pod can start before it exists, and a shell without it has no zsh, no starship
# and no ttyd. So this waits rather than failing: the person opening a terminal a second after the
# pod starts gets a shell a moment later, not a container crash-looping behind them.
set -eu

PROFILE=/nix/profile/current
TTYD="$PROFILE/bin/ttyd"

waited=0
while [ ! -x "$TTYD" ]; do
  # One line, once: a pod whose profile never arrives must say so somewhere a person looks, and a
  # line per second would bury it.
  if [ "$waited" -eq 30 ]; then
    echo "shell: still waiting for the workspace profile at $PROFILE" >&2
  fi
  sleep 1
  waited=$((waited + 1))
done

# The person's own shell when the profile has published one, else a plain one: a terminal that
# opens NOW beats a prompt that is prettier in a minute, and nothing about this container is worth
# waiting for (spec §2.1 — it is the person's convenience).
SHELL_BIN="$PROFILE/bin/zsh"
[ -x "$SHELL_BIN" ] || SHELL_BIN=/bin/sh

cd "$HOME"
# `-W` is what makes the terminal WRITABLE: ttyd is read-only without it, which reads as "my
# keystrokes do nothing" and is the one flag nobody guesses. `-i 0.0.0.0` because the only way in
# is the pod's own network namespace, fenced by the NetworkPolicy; ttyd's own basic auth is not the
# fence and is deliberately not used.
exec "$TTYD" \
  -p 7790 \
  -i 0.0.0.0 \
  -W \
  -t disableLeaveAlert=true \
  -t 'fontFamily=IBM Plex Mono' \
  -t fontSize=13 \
  "$SHELL_BIN" -l
