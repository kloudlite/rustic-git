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
  # Whole logs to files, filtered only on failure: a filter pipe under pipefail turns "grep found
  # nothing" into a silent exit.
  cargo clippy --workspace --all-targets --locked -- -D warnings > /tmp/ship-clippy.log 2>&1 \
    || { grep -E '^(warning|error)' -A6 /tmp/ship-clippy.log | head -40; exit 1; }
  cargo test --locked > /tmp/ship-test.log 2>&1 \
    || { grep -E '^test result|FAILED|panicked|^error' /tmp/ship-test.log | grep -v ': ok' | head -20; exit 1; }
  # The web's own gate (web.yml's exact steps), since the web image ships from here too.
  ( cd web && export PATH=/work/node/bin:/work/bun/bin:$PATH \
    && bun install --frozen-lockfile > /tmp/ship-web.log 2>&1 \
    && bun run typecheck >> /tmp/ship-web.log 2>&1 && bun run lint >> /tmp/ship-web.log 2>&1 && bun run test >> /tmp/ship-web.log 2>&1 ) \
    || { tail -30 /tmp/ship-web.log; exit 1; }
  echo "gate passed: $(grep -c '^test result: ok' /tmp/ship-test.log || true) test binaries green"
fi

echo "==> release build"
cargo build --release --locked --bins 2>&1 | tail -1

# The Dockerfile COPYs target/release/* relative to its context and .dockerignore drops the rest,
# so a staging dir with hardlinks to the binaries is the whole context — nothing else is sent.
CTX=/work/ctx; rm -rf "$CTX"; mkdir -p "$CTX/target/release"
cp Dockerfile .dockerignore "$CTX/"
for b in kloudlite kloudlite-api kloudlite-worker kloudlite-agent kloudlite-gateway kloudlite-builder-gate kloudlite-slo kl; do
  ln -f /work/target/release/$b "$CTX/target/release/$b"
done

for t in server:kloudlite agent:kloudlite-agent gateway:kloudlite-gateway builder-gate:kloudlite-builder-gate slo:kloudlite-slo workspace:kloudlite-workspace; do
  target=${t%%:*}; image=${t#*:}
  echo "==> $image:$SHA"
  buildctl build --frontend dockerfile.v0 --local context="$CTX" --local dockerfile="$CTX" \
    --opt target="$target" --opt build-arg:PROFILE=release \
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
