#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
. "$SCRIPT_DIR/source-provenance.sh"

ROOT=$(cd "$SCRIPT_DIR/../../.." && pwd)
RECORD=${KL_HARNESS_GATE_RECORD:-}
RECORD_TMP=
RECORD_PENDING=0
cleanup_record() {
  local status=$?
  if [ "$RECORD_PENDING" = 1 ]; then
    rm -f "$RECORD"
    if [ -n "$RECORD_TMP" ]; then rm -f "$RECORD_TMP"; fi
  fi
  return "$status"
}
trap cleanup_record EXIT
trap 'exit 130' INT TERM HUP
if [ -n "$RECORD" ]; then
  RECORD_PENDING=1
  rm -f "$RECORD"
fi
SHA=$(source_provenance_capture "$ROOT")
[ -z "${GITHUB_SHA:-}" ] || [ "$GITHUB_SHA" = "$SHA" ] || { echo "checkout SHA $SHA differs from GITHUB_SHA $GITHUB_SHA" >&2; exit 2; }

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

# Unconditional, not only under RECORD: a recordless run is a plain pass/fail a human reads, so it
# must catch a checkout that moved during the gate too.
source_provenance_verify "$ROOT" "$SHA"
if [ -n "$RECORD" ]; then
  RECORD_TMP="$RECORD.tmp.$$"
  source_provenance_write_record "$RECORD" "$RECORD_TMP" "$SHA"
  RECORD_PENDING=0
fi
