#!/usr/bin/env bash
# CI, in the pod: the gate CI runs, a release build, and every image (the web's and bench's too) pushed under CI's tag
# shape. Runs under pm2 as `ship` (`pm2 logs ship`). Refuses a dirty or unpushed tree: the tag is
# the commit SHA, and a tag must name a commit some remote actually holds — `platform` during the
# owner's platform-first loop (2026-09-13: push platform → ship from the pod → verify on the fleet
# → push origin only once it's good), `origin` (GitHub) once it has landed there.
#   pod/ship.sh            # test + clippy + build + push
#   pod/ship.sh --no-gate  # skip test + clippy (they already passed this cycle)
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
. "$SCRIPT_DIR/source-provenance.sh"
cd /work/src
SHA=$(source_provenance_head "$PWD")
# The gate's own target dir, never the shared /work/target: cargo keys a workspace member by its
# workspace-RELATIVE path, so a fix/* worktree building `crates/git` at the same time overwrites
# the very rlib the gate is linking against — the ship of 2026-09-12 05:07 failed to compile
# tests/store.rs against a signature that existed only on another branch. Disk is the price.
export CARGO_TARGET_DIR=/work/target-ship
# A one-shot gate never benefits from incremental artifacts; they cost ~40 GB per build and filled
# the disk on the second ship of 2026-09-12 (linking failed at 98 %).
export CARGO_INCREMENTAL=0
# Tokio task dumps on a kube stall (crates/workspaces `stall-dump`, off until ClusterSettings
# `stallDumps`). tokio_unstable must be global — cargo has no stable per-crate cfg — and is set for
# the gate too so gate and build share one artifact set. It only unlocks APIs; nothing changes at
# runtime unless one is called. CI's master images build without it.
export RUSTFLAGS="--cfg tokio_unstable"
FEATURES="--features kloudlite-agent-bin/stall-dump"
GATE_RECORD_DIR=${KL_GATE_RECORD_DIR:-/work/gate-records}
GATE_RECORD="$GATE_RECORD_DIR/$SHA"
GATE_RECORD_TMP=
GATE_RECORD_PENDING=0
HARNESS_GATE_RECORD=/tmp/harness-gate-$SHA
cleanup_gate_record() {
  local status=$?
  if [ "$GATE_RECORD_PENDING" = 1 ]; then
    rm -f "$GATE_RECORD" "$HARNESS_GATE_RECORD"
    if [ -n "$GATE_RECORD_TMP" ]; then rm -f "$GATE_RECORD_TMP"; fi
  fi
  return "$status"
}
trap cleanup_gate_record EXIT
trap 'exit 130' INT TERM HUP
case "${1:-}" in
  ""|--no-gate) ;;
  *) echo "unknown option: $1" >&2; exit 2 ;;
esac
if [ "${1:-}" != "--no-gate" ]; then
  GATE_RECORD_PENDING=1
  rm -f "$GATE_RECORD" "$HARNESS_GATE_RECORD"
fi
source_provenance_require_clean "$PWD"
# Any origin OR platform branch, not only master: a feature branch is verified on the fleet BEFORE
# it merges (fixed means verified on the carrying build), and during the platform-first loop the
# code only lives on `platform` until verification passes. Tolerate one remote being unreachable —
# the pod may only have a route to one of them — but not both.
fetch_ok=0
git fetch -q origin && fetch_ok=1 || echo "warning: fetch origin failed" >&2
git fetch -q platform && fetch_ok=1 || echo "warning: fetch platform failed" >&2
[ "$fetch_ok" = 1 ] || { echo "could not fetch origin or platform" >&2; exit 2; }
git branch -r --contains "$SHA" | grep -qE '^ *(origin|platform)/' \
  || { echo "HEAD is on no origin or platform branch; push first" >&2; exit 2; }

if [ "${1:-}" != "--no-gate" ]; then
  echo "==> gate: clippy + tests (CI's exact commands)"
  # Old test binaries are never collected by cargo; see prune-deps.py for the day they filled the disk.
  /work/src/deploy/dev/pod/prune-deps.py
  # A test binary that aborts (panic = abort in the release profile) would leave a core.<pid> in
  # the checkout — ten empty ones reached a commit on 2026-09-08 through git add -A.
  ulimit -c 0
  # Whole logs to files, filtered only on failure: a filter pipe under pipefail turns "grep found
  # nothing" into a silent exit.
  cargo clippy --workspace --all-targets --locked $FEATURES -- -D warnings > /tmp/ship-clippy.log 2>&1 \
    || { grep -E '^(warning|error)' -A6 /tmp/ship-clippy.log | head -40; exit 1; }
  # nextest, not `cargo test`: the same tests from the same binaries, but the 100-odd binaries
  # run in parallel instead of one after another — `cargo test` left 16 cores idle behind a
  # 59 s wall-clock test. No doctests are lost: the workspace has none.
  # A watcher beside the run: any test process alive past 90 s is dumped with gdb (the pod
  # carries SYS_PTRACE for exactly this), then killed so the gate fails with a stack rather
  # than sitting for hours. The dump is the whole evidence of a hang; keep it.
  cargo nextest run --workspace --locked $FEATURES > /tmp/ship-test.log 2>&1 &
  NX=$!
  while kill -0 $NX 2>/dev/null; do
    sleep 5
    #  on every probe: a test process that exits between pgrep and ps is the common
    # case, and under  a failing substitution here silently ended the whole ship.
    for pid in $(pgrep -f "^$CARGO_TARGET_DIR/debug/deps/" || true); do
      age=$(ps -o etimes= -p "$pid" 2>/dev/null | tr -d ' ' || true)
      [ -n "$age" ] && [ "$age" -gt 90 ] || continue
      echo "HUNG: $(ps -o args= -p "$pid" | cut -c1-200) (${age}s) — stacks in /tmp/ship-hang-$pid.bt" >&2
      gdb -p "$pid" -batch -ex "info threads" -ex "thread apply all bt 40" > "/tmp/ship-hang-$pid.bt" 2>&1
      kill -9 "$pid"
    done
  done
  wait $NX || { grep -E 'FAIL|panicked|^error|Summary' /tmp/ship-test.log | head -20; exit 1; }
  echo "==> harness: typecheck + bench + renderer boot"
  KL_NODE24_BIN=${KL_NODE24_BIN:-/work/review-node24/node_modules/node/bin} \
  KL_HARNESS_GATE_RECORD="$HARNESS_GATE_RECORD" \
    ./deploy/dev/pod/harness-gate.sh > /tmp/ship-harness.log 2>&1 \
    || { tail -40 /tmp/ship-harness.log; exit 1; }
  # The web's own gate (web.yml's exact steps), since the web image ships from here too.
  ( cd web && export PATH=/work/node/bin:/work/bun/bin:$PATH \
    && bun install --frozen-lockfile > /tmp/ship-web.log 2>&1 \
    && bun run typecheck >> /tmp/ship-web.log 2>&1 && bun run lint >> /tmp/ship-web.log 2>&1 && bun run test >> /tmp/ship-web.log 2>&1 ) \
    || { tail -30 /tmp/ship-web.log; exit 1; }
  # A retried-then-passed test (.config/nextest.toml) must not vanish into the log.
  grep -E '^\s*FLAKY' /tmp/ship-test.log || true
  source_provenance_verify "$PWD" "$SHA"
  GATE_RECORD_TMP="$GATE_RECORD.tmp.$$"
  source_provenance_write_record "$GATE_RECORD" "$GATE_RECORD_TMP" "$SHA"
  echo "gate passed: $(grep -oE 'Summary.*' /tmp/ship-test.log | tail -1)"
else
  [ -f "$GATE_RECORD" ] || { echo "no gate record for $SHA at $GATE_RECORD; run without --no-gate first" >&2; exit 2; }
  [ "$(tr -d '\n' < "$GATE_RECORD")" = "$SHA" ] || { echo "gate record does not match $SHA: $GATE_RECORD" >&2; exit 2; }
  echo "verified gate record for $SHA"
fi

source_provenance_verify "$PWD" "$SHA"
# `dev-image`, not `release`: this fleet is the dev fleet, and thin LTO + one codegen unit cost
# 3.5 min of single-threaded relinking per ship for a few percent of runtime. CI's master images
# keep the full release profile (`image.yml`), so a production repin is never built here.
PROFILE=dev-image
echo "==> $PROFILE build"
cargo build --profile $PROFILE --locked --bins $FEATURES 2>&1 | tail -1
# Its own target, so these never land in target/$PROFILE beside the glibc binaries.
# The workspace CLI and the intercept proxy, one musl invocation: the workspace image is Alpine
# and the proxy image is `scratch`, and neither has a glibc to link against.
cargo build --profile $PROFILE --locked -p kl -p kloudlite-intercept-proxy --target x86_64-unknown-linux-musl 2>&1 | tail -1

# The Dockerfile COPYs target/release/* relative to its context and .dockerignore drops the rest,
# so a staging dir with hardlinks to the binaries is the whole context — nothing else is sent.
# From $CARGO_TARGET_DIR, never a literal path: when the gate moved to its own target dir
# (2026-09-12) these links kept pointing at the old shared one, and two ships pushed the previous
# build's binaries under new tags — the tag said 8caa1dfc, the api still forwarded like 4a5cf582.
CTX=/work/ctx; rm -rf "$CTX"; mkdir -p "$CTX/target/$PROFILE"
cp Dockerfile .dockerignore "$CTX/"
# The workspace image COPYs two scripts from deploy/workspace-image (CI's context is `.`, so it
# never notices); a staging context that holds only binaries fails that COPY with "not found".
mkdir -p "$CTX/deploy" && cp -r deploy/workspace-image "$CTX/deploy/"
for b in kloudlite kloudlite-api kloudlite-worker kloudlite-agent kloudlite-gateway kloudlite-builder-gate kloudlite-slo kloudlite-controller kl-connect; do
  ln -f "$CARGO_TARGET_DIR/$PROFILE/$b" "$CTX/target/$PROFILE/$b"
done
mkdir -p "$CTX/target/x86_64-unknown-linux-musl/$PROFILE"
ln -f "$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/$PROFILE/kl" "$CTX/target/x86_64-unknown-linux-musl/$PROFILE/kl"
ln -f "$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/$PROFILE/kloudlite-intercept-proxy" "$CTX/target/x86_64-unknown-linux-musl/$PROFILE/kloudlite-intercept-proxy"
# deploy/bench/Dockerfile COPYs the musl kl from the release path literally (CI builds release);
# the pod only ever builds dev-image, so link it under the name the Dockerfile expects too.
mkdir -p "$CTX/target/x86_64-unknown-linux-musl/release"
ln -f "$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/$PROFILE/kl" "$CTX/target/x86_64-unknown-linux-musl/release/kl"
# Same CTX: the bench image needs the harness sources CI's context carries, none of which
# .dockerignore admits from anywhere but these exact paths (deploy/bench/, harness/{package*,bench,pi}).
cp -r deploy/bench deploy/shell-image "$CTX/deploy/"
mkdir -p "$CTX/harness"
cp harness/package.json harness/package-lock.json "$CTX/harness/"
cp -r harness/bench harness/pi harness/skills "$CTX/harness/"

source_provenance_verify "$PWD" "$SHA"
for t in server:kloudlite agent:kloudlite-agent gateway:kloudlite-gateway controller:kloudlite-controller builder-gate:kloudlite-builder-gate slo:kloudlite-slo workspace:kloudlite-workspace intercept-proxy:kloudlite-intercept-proxy; do
  target=${t%%:*}; image=${t#*:}
  echo "==> $image:$SHA"
  buildctl build --frontend dockerfile.v0 --local context="$CTX" --local dockerfile="$CTX" \
    --opt target="$target" --opt build-arg:PROFILE=$PROFILE \
    --output "type=image,\"name=ghcr.io/kloudlite/$image:$SHA,ghcr.io/kloudlite/$image:latest\",push=true" \
    --progress plain 2>&1 | grep -E '^#[0-9]+ (DONE|ERROR|CACHED)|exporting|pushing|error' | tail -4
done
echo "==> kloudlite-bench:$SHA"
buildctl build --frontend dockerfile.v0 --local context="$CTX" --local dockerfile="$CTX/deploy/bench" \
  --output "type=image,\"name=ghcr.io/kloudlite/kloudlite-bench:$SHA,ghcr.io/kloudlite/kloudlite-bench:latest\",push=true" \
  --progress plain 2>&1 | grep -E '^#[0-9]+ (DONE|ERROR|CACHED)|exporting|pushing|error' | tail -4
echo "==> kloudlite-shell:$SHA"
# The shell sidecar: kl (the musl artifact already linked above), the shared rc files, ttyd from the profile.
buildctl build --frontend dockerfile.v0 --local context="$CTX" --local dockerfile="$CTX/deploy/shell-image" \
  --output "type=image,\"name=ghcr.io/kloudlite/kloudlite-shell:$SHA,ghcr.io/kloudlite/kloudlite-shell:latest\",push=true" \
  --progress plain 2>&1 | grep -E '^#[0-9]+ (DONE|ERROR|CACHED)|exporting|pushing|error' | tail -4
# The web image too, from `web/` as its own context (its Dockerfile runs bun install + next build
# inside the build, so nothing from the pod's node_modules leaks in). CI's web.yml still builds it
# on master; this is the same image under the same SHA tag, so either may land first.
echo "==> kloudlite-web:$SHA"
# `/docs` reads `apps/web/content/docs` in the image (see `lib/docs.ts`); git-ignored, so this
# is the only way it gets there.
rm -rf web/apps/web/content/docs && mkdir -p web/apps/web/content && cp -r docs/product web/apps/web/content/docs
# The web image is the one build that reads the LIVE checkout (`--local context=web`), not the
# frozen $CTX — verify right here so a checkout that moved during the loop fails BEFORE the web
# image is built, instead of at the post-push check, which by then has already pushed it.
source_provenance_verify "$PWD" "$SHA"
buildctl build --frontend dockerfile.v0 --local context=web --local dockerfile=web \
  --output "type=image,\"name=ghcr.io/kloudlite/kloudlite-web:$SHA,ghcr.io/kloudlite/kloudlite-web:latest\",push=true" \
  --progress plain 2>&1 | grep -E '^#[0-9]+ (DONE|ERROR|CACHED)|exporting|pushing|error' | tail -4
source_provenance_verify "$PWD" "$SHA"
echo "shipped $SHA — on the laptop: deploy/pin.sh $SHA $SHA && git commit -am 'Pin every tier to ${SHA:0:8}' && deploy/roll.sh"
GATE_RECORD_PENDING=0
