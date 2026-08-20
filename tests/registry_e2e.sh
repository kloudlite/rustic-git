#!/usr/bin/env bash
# Pushes and pulls a real image with a real client. Requires docker (or podman) and a running
# node. Not part of `cargo test`: it needs a daemon and a registry reachable over TLS or listed as
# an insecure registry.
#
# Two independent halves:
#   1. curl-only:  auth, blob round-trip, manifest round-trip, tag list, catalog. Needs only a
#      running `rustic-git serve` — no container daemon. Always runs.
#   2. docker/podman: build, push, pull, cross-tag mount. Needs a reachable daemon. Skipped with a
#      loud, early message (not a mid-script failure) when one isn't reachable.
set -euo pipefail

REG="${REG:-localhost:8080}"
OWNER="${OWNER:-acme}"
TOKEN="${TOKEN:?run: cargo run --bin rustic-git -- admin add-token acme, and export TOKEN}"
CLI="${CLI:-docker}"

echo "==> [1/2] curl-only half: auth, blobs, manifests, tags, catalog"

echo "  -- /v2/ carries the version header"
curl -fsS -D - -u "$OWNER:$TOKEN" "http://$REG/v2/" -o /dev/null | grep -qi '^docker-distribution-api-version: registry/2.0'

echo "  -- /v2/token mints a bearer"
bearer=$(curl -fsS -u "$OWNER:$TOKEN" "http://$REG/v2/token?service=$REG&scope=repository:$OWNER/e2e-curl:pull,push" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
[ -n "$bearer" ] || { echo "no bearer token in /v2/token response" >&2; exit 1; }

echo "  -- blob PUT then GET round-trips"
blob=$(mktemp); trap 'rm -f "$blob"' EXIT
printf 'e2e blob %s' "$(date +%s)" > "$blob"
digest="sha256:$(shasum -a 256 "$blob" | cut -d' ' -f1)"
curl -fsS -X POST -u "$OWNER:$TOKEN" \
  --data-binary "@$blob" \
  "http://$REG/v2/$OWNER/e2e-curl/blobs/uploads/?digest=$digest" -o /dev/null
curl -fsS -u "$OWNER:$TOKEN" "http://$REG/v2/$OWNER/e2e-curl/blobs/$digest" -o "$blob.got"
cmp -s "$blob" "$blob.got" || { echo "blob GET did not match what was pushed" >&2; exit 1; }
rm -f "$blob.got"

echo "  -- manifest PUT then GET returns byte-identical bytes"
manifest=$(mktemp)
cat > "$manifest" <<EOF
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"$digest","size":$(wc -c < "$blob")},"layers":[]}
EOF
curl -fsS -X PUT -u "$OWNER:$TOKEN" \
  -H 'Content-Type: application/vnd.oci.image.manifest.v1+json' \
  --data-binary "@$manifest" \
  "http://$REG/v2/$OWNER/e2e-curl/manifests/v1" -o /dev/null
curl -fsS -u "$OWNER:$TOKEN" \
  -H 'Accept: application/vnd.oci.image.manifest.v1+json' \
  "http://$REG/v2/$OWNER/e2e-curl/manifests/v1" -o "$manifest.got"
cmp -s "$manifest" "$manifest.got" || { echo "manifest GET did not byte-match what was pushed" >&2; exit 1; }
rm -f "$manifest" "$manifest.got"

echo "  -- tags/list carries the tag we just pushed"
curl -fsS -u "$OWNER:$TOKEN" "http://$REG/v2/$OWNER/e2e-curl/tags/list" | grep -q '"v1"'

echo "  -- _catalog carries the image we just pushed"
curl -fsS -u "$OWNER:$TOKEN" "http://$REG/v2/_catalog" | grep -q "$OWNER/e2e-curl"

echo "  curl-only half: OK"

echo
echo "==> [2/2] docker half: build, push, pull, cross-repo mount"
if ! "$CLI" info >/dev/null 2>&1; then
  cat >&2 <<MSG
No $CLI daemon is reachable ($CLI info failed). This half needs a running container
daemon and is skipped — it is not something this script may start as a side effect.
Start one (Docker Desktop, colima, or podman machine) and re-run to exercise it.
MSG
  exit 0
fi

echo "==> login"
echo "$TOKEN" | "$CLI" login "$REG" --username "$OWNER" --password-stdin

echo "==> build a tiny image"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
printf 'FROM scratch\nCOPY hello /hello\n' > "$tmp/Dockerfile"
echo hello > "$tmp/hello"
"$CLI" build -t "$REG/$OWNER/e2e:v1" "$tmp"

echo "==> push"
"$CLI" push "$REG/$OWNER/e2e:v1"

echo "==> pull it back from a clean local state"
"$CLI" rmi "$REG/$OWNER/e2e:v1"
"$CLI" pull "$REG/$OWNER/e2e:v1"

echo "==> the catalog and the tag list agree"
curl -fsS -u "$OWNER:$TOKEN" "http://$REG/v2/_catalog" | grep -q "$OWNER/e2e"
curl -fsS -u "$OWNER:$TOKEN" "http://$REG/v2/$OWNER/e2e/tags/list" | grep -q v1

echo "==> a second push of the same layers mounts rather than re-uploads"
"$CLI" tag "$REG/$OWNER/e2e:v1" "$REG/$OWNER/e2e-two:v1"
"$CLI" push "$REG/$OWNER/e2e-two:v1"

echo "OK"
