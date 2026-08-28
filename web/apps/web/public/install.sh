#!/bin/sh
# curl -fsSL https://dev.kloudlite.io/install.sh | sh
#
# Installs the `kl` CLI from the latest kl-v* GitHub release. Nothing here needs root: the default
# target is ~/.local/bin, and BIN_DIR overrides it.
set -eu

REPO=${REPO:-kloudlite/rustic-git}
BIN_DIR=${BIN_DIR:-"$HOME/.local/bin"}

case "$(uname -s)" in
  Linux) os=unknown-linux-gnu ;;
  Darwin) os=apple-darwin ;;
  *) echo "kl: unsupported OS $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64) arch=x86_64 ;;
  arm64|aarch64) arch=aarch64 ;;
  *) echo "kl: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac
asset="kl-$arch-$os"

# The release tag, straight from the redirect the /latest URL issues — no jq, no API token.
tag=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" | sed 's#.*/tag/##')
[ -n "$tag" ] || { echo "kl: could not find the latest release" >&2; exit 1; }
url="https://github.com/$REPO/releases/download/$tag/$asset"

# The staging directory sits INSIDE $BIN_DIR so the final `mv` is a same-filesystem rename: an
# atomic swap, never a half-written kl that someone's ssh then runs.
mkdir -p "$BIN_DIR"
tmp=$(mktemp -d "$BIN_DIR/.kl-install.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
echo "Downloading $asset ($tag)…"
curl -fsSL "$url" -o "$tmp/kl"

sum() {
  if command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1"
  elif command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"
  fi | cut -d' ' -f1
}

# Best effort: a release cut before sha256sums existed must not block an install, but it says so.
if curl -fsSL "https://github.com/$REPO/releases/download/$tag/sha256sums" -o "$tmp/sha256sums"; then
  want=$(awk -v a="$asset" '$2 == a || $2 == "*"a {print $1}' "$tmp/sha256sums")
  got=$(sum "$tmp/kl")
  if [ -z "$want" ]; then
    echo "WARNING: $tag publishes no checksum for $asset; installing unverified." >&2
  elif [ -z "$got" ]; then
    echo "WARNING: neither shasum nor sha256sum found; installing unverified." >&2
  elif [ "$want" != "$got" ]; then
    echo "kl: checksum mismatch for $asset" >&2; exit 1
  fi
else
  echo "WARNING: $tag has no sha256sums file; installing unverified." >&2
fi

chmod +x "$tmp/kl"
mv "$tmp/kl" "$BIN_DIR/kl"
echo "Installed $BIN_DIR/kl"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "Add it to your PATH:  export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac
echo "Next: kl login"
