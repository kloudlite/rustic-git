#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
NODE24_BIN=${KL_NODE24_BIN:-/work/review-node24/node_modules/node/bin}
if [ -x "$NODE24_BIN/node" ]; then
  PATH="$NODE24_BIN:$PATH"
fi
export PATH

major=$(node -p 'process.versions.node.split(".")[0]')
[ "$major" = 24 ] || { echo "harness gate requires Node 24, found $(node --version)" >&2; exit 2; }
command -v npm >/dev/null || { echo "harness gate requires npm" >&2; exit 2; }
command -v xvfb-run >/dev/null || { echo "harness gate requires xvfb-run" >&2; exit 2; }

cd "$ROOT/harness"
npm ci
npm run typecheck
xvfb-run -a npm run bench:test
xvfb-run -a node --test 'bench/test/renderer-boot.test.ts'

if [ -n "${KL_HARNESS_GATE_RECORD:-}" ]; then
  sha=$(git -C "$ROOT" rev-parse HEAD)
  [ -z "${GITHUB_SHA:-}" ] || [ "$GITHUB_SHA" = "$sha" ] || { echo "checkout SHA $sha differs from GITHUB_SHA $GITHUB_SHA" >&2; exit 2; }
  mkdir -p "$(dirname "$KL_HARNESS_GATE_RECORD")"
  tmp="$KL_HARNESS_GATE_RECORD.tmp.$$"
  printf '%s\n' "$sha" > "$tmp"
  mv "$tmp" "$KL_HARNESS_GATE_RECORD"
fi
