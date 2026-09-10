#!/usr/bin/env bash
# CI, in the pod: the gate CI runs, a release build, and every image (the web's too) pushed under CI's tag
# shape. Runs under pm2 as `ship` (`pm2 logs ship`). Refuses a dirty or unpushed tree: the tag is
# the commit SHA, and a tag must mean exactly the code GitHub has under that SHA.
#   pod/ship.sh            # test + clippy + build + push
#   pod/ship.sh --no-gate  # skip test + clippy (they already passed this cycle)
set -euo pipefail
cd /work/src
git diff --quiet && git diff --cached --quiet || { echo "the tree is dirty; commit first" >&2; exit 2; }
SHA=$(git rev-parse HEAD)
# Any origin branch, not only master: a feature branch is verified on the fleet BEFORE it merges
# (fixed means verified on the carrying build), and the invariant is only that GitHub holds
# exactly this code under this SHA.
git fetch -q origin
git branch -r --contains "$SHA" | grep -q '^ *origin/' || { echo "HEAD is on no origin branch; push first" >&2; exit 2; }

if [ "${1:-}" != "--no-gate" ]; then
  echo "==> gate: clippy + tests (CI's exact commands)"
  # A test binary that aborts (panic = abort in the release profile) would leave a core.<pid> in
  # the checkout — ten empty ones reached a commit on 2026-09-08 through git add -A.
  ulimit -c 0
  # Whole logs to files, filtered only on failure: a filter pipe under pipefail turns "grep found
  # nothing" into a silent exit.
  cargo clippy --workspace --all-targets --locked -- -D warnings > /tmp/ship-clippy.log 2>&1 \
    || { grep -E '^(warning|error)' -A6 /tmp/ship-clippy.log | head -40; exit 1; }
  # nextest, not `cargo test`: the same tests from the same binaries, but the 100-odd binaries
  # run in parallel instead of one after another — `cargo test` left 16 cores idle behind a
  # 59 s wall-clock test. No doctests are lost: the workspace has none.
  # A watcher beside the run: any test process alive past 150 s is dumped with gdb (the pod
  # carries SYS_PTRACE for exactly this), then killed so the gate fails with a stack rather
  # than sitting for hours. The dump is the whole evidence of a hang; keep it.
  cargo nextest run --workspace --locked > /tmp/ship-test.log 2>&1 &
  NX=$!
  while kill -0 $NX 2>/dev/null; do
    sleep 5
    #  on every probe: a test process that exits between pgrep and ps is the common
    # case, and under  a failing substitution here silently ended the whole ship.
    for pid in $(pgrep -f '^/work/target/debug/deps/' || true); do
      age=$(ps -o etimes= -p "$pid" 2>/dev/null | tr -d ' ' || true)
      [ -n "$age" ] && [ "$age" -gt 150 ] || continue
      echo "HUNG: $(ps -o args= -p "$pid" | cut -c1-200) (${age}s) — stacks in /tmp/ship-hang-$pid.bt" >&2
      gdb -p "$pid" -batch -ex "info threads" -ex "thread apply all bt 40" > "/tmp/ship-hang-$pid.bt" 2>&1
      kill -9 "$pid"
    done
  done
  wait $NX || { grep -E 'FAIL|panicked|^error|Summary' /tmp/ship-test.log | head -20; exit 1; }
  # The web's own gate (web.yml's exact steps), since the web image ships from here too.
  ( cd web && export PATH=/work/node/bin:/work/bun/bin:$PATH \
    && bun install --frozen-lockfile > /tmp/ship-web.log 2>&1 \
    && bun run typecheck >> /tmp/ship-web.log 2>&1 && bun run lint >> /tmp/ship-web.log 2>&1 && bun run test >> /tmp/ship-web.log 2>&1 ) \
    || { tail -30 /tmp/ship-web.log; exit 1; }
  echo "gate passed: $(grep -oE 'Summary.*' /tmp/ship-test.log | tail -1)"
fi

# `dev-image`, not `release`: this fleet is the dev fleet, and thin LTO + one codegen unit cost
# 3.5 min of single-threaded relinking per ship for a few percent of runtime. CI's master images
# keep the full release profile (`image.yml`), so a production repin is never built here.
PROFILE=dev-image
echo "==> $PROFILE build"
cargo build --profile $PROFILE --locked --bins 2>&1 | tail -1
# The workspace CLI, for the Alpine workspace image: its own target, so it never lands in
# target/$PROFILE beside the glibc binaries.
cargo build --profile $PROFILE --locked -p kl --target x86_64-unknown-linux-musl 2>&1 | tail -1

# The Dockerfile COPYs target/release/* relative to its context and .dockerignore drops the rest,
# so a staging dir with hardlinks to the binaries is the whole context — nothing else is sent.
CTX=/work/ctx; rm -rf "$CTX"; mkdir -p "$CTX/target/$PROFILE"
cp Dockerfile .dockerignore "$CTX/"
# The workspace image COPYs two scripts from deploy/workspace-image (CI's context is `.`, so it
# never notices); a staging context that holds only binaries fails that COPY with "not found".
mkdir -p "$CTX/deploy" && cp -r deploy/workspace-image "$CTX/deploy/"
for b in kloudlite kloudlite-api kloudlite-worker kloudlite-agent kloudlite-gateway kloudlite-builder-gate kloudlite-slo kl-connect; do
  ln -f /work/target/$PROFILE/$b "$CTX/target/$PROFILE/$b"
done
mkdir -p "$CTX/target/x86_64-unknown-linux-musl/$PROFILE"
ln -f /work/target/x86_64-unknown-linux-musl/$PROFILE/kl "$CTX/target/x86_64-unknown-linux-musl/$PROFILE/kl"

for t in server:kloudlite agent:kloudlite-agent gateway:kloudlite-gateway builder-gate:kloudlite-builder-gate slo:kloudlite-slo workspace:kloudlite-workspace; do
  target=${t%%:*}; image=${t#*:}
  echo "==> $image:$SHA"
  buildctl build --frontend dockerfile.v0 --local context="$CTX" --local dockerfile="$CTX" \
    --opt target="$target" --opt build-arg:PROFILE=$PROFILE \
    --output "type=image,\"name=ghcr.io/kloudlite/$image:$SHA,ghcr.io/kloudlite/$image:latest\",push=true" \
    --progress plain 2>&1 | grep -E '^#[0-9]+ (DONE|ERROR|CACHED)|exporting|pushing|error' | tail -4
done
# The web image too, from `web/` as its own context (its Dockerfile runs bun install + next build
# inside the build, so nothing from the pod's node_modules leaks in). CI's web.yml still builds it
# on master; this is the same image under the same SHA tag, so either may land first.
echo "==> kloudlite-web:$SHA"
buildctl build --frontend dockerfile.v0 --local context=web --local dockerfile=web \
  --output "type=image,\"name=ghcr.io/kloudlite/kloudlite-web:$SHA,ghcr.io/kloudlite/kloudlite-web:latest\",push=true" \
  --progress plain 2>&1 | grep -E '^#[0-9]+ (DONE|ERROR|CACHED)|exporting|pushing|error' | tail -4
echo "shipped $SHA — on the laptop: deploy/pin.sh $SHA $SHA && git commit -am 'Pin every tier to ${SHA:0:8}' && deploy/roll.sh"
