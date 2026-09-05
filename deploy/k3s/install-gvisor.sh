#!/usr/bin/env bash
# Install gVisor (runsc) on a worker node and register it with k3s's containerd.
#
# WHY: tenants share a node, so without a sandbox they share its kernel — a kernel exploit from any
# workspace is a host compromise, not a namespace escape. gVisor puts a userspace kernel in front of
# the host's, so the tenant's syscalls are serviced by runsc rather than by Linux directly.
#
# Applies to TENANT pods only, via `runtimeClassName: gvisor` (the agent stamps it when
# WS_RUNTIME_CLASS is set). The controller itself must never be sandboxed: it drives btrfs against
# the host pool, which is exactly the access a sandbox exists to remove.
#
# Run on each pooled node, as root.
set -euo pipefail

# A dated release, not `release/latest`: this binary IS the tenant/kernel boundary, and `latest`
# moves under every node provisioned on a different day — version skew with no record of it. The
# checksums are recorded HERE rather than fetched from the bucket beside the binary: a checksum
# from the same origin only proves the download was not corrupted, never that the origin was not
# replaced. Bumping is: change VERSION, fetch the four `.sha512` files from the new release
# directory, paste the values, and re-run on every pool node in one sitting (runtimeclass.yaml
# schedules on the label, not the version, so two versions across nodes is invisible).
#   for a in x86_64 aarch64; do for f in runsc containerd-shim-runsc-v1; do
#     curl -sS "https://storage.googleapis.com/gvisor/releases/release/$VERSION/$a/$f.sha512"; done; done
VERSION=20260817.0
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64)
    RUNSC_SHA512=84936438d583ec976800f464e75a83e1515f0890b451b9b4db219c4472b54ca9b106a6772ee683f1e64cce2128871d7637b14d800591f8451b8137f6c39fb2ef
    SHIM_SHA512=b60d1c418b841ab046951cc7a91f490a221198fbe81ec55dc364432578fddd44e97063793ce6651be397af2f64ec47170dff77a45db277819c3fb08fec9f3ced
    ;;
  aarch64)
    RUNSC_SHA512=6394fd161a4af0dc9a2c29f75c3016d05275a55744f124e12023fa7666a9f161c68d6ce3803ad49205c6a7b5bee0ad2ccf48edff340db344fdafec678c788aa4
    SHIM_SHA512=a7c0147f635938225e41c9660b95ba5235121a142c11830794e2a52b472783547e2b089bdba4f1344d30f65aeee651b377a1cafadd262a134f5e1ac10c6bf4bb
    ;;
  *) echo "no pinned checksum for $ARCH — add one above before installing" >&2; exit 1 ;;
esac
URL="https://storage.googleapis.com/gvisor/releases/release/${VERSION}/${ARCH}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "==> fetching runsc ${VERSION}"
cd "$TMP"
wget -q "${URL}/runsc" "${URL}/containerd-shim-runsc-v1"
# The checksums are the whole reason this is not a curl-into-bash: a sandbox binary fetched without
# verification is a rootkit with extra steps.
printf '%s  runsc\n%s  containerd-shim-runsc-v1\n' "$RUNSC_SHA512" "$SHIM_SHA512" | sha512sum -c
install -m 755 runsc containerd-shim-runsc-v1 /usr/local/bin/

echo "==> registering the runtime with k3s containerd"
# k3s regenerates config.toml from this template on every start, so editing config.toml directly is
# undone by the next restart. The template is the only durable place.
TPL=/var/lib/rancher/k3s/agent/etc/containerd/config-v3.toml.tmpl
[ -f "$TPL" ] || cp /var/lib/rancher/k3s/agent/etc/containerd/config.toml "$TPL"
if ! grep -q 'runtimes.runsc' "$TPL"; then
  cat >> "$TPL" <<'TOML'

[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.runsc]
  runtime_type = 'io.containerd.runsc.v1'
TOML
fi

echo "==> restarting k3s-agent"
systemctl restart k3s-agent
sleep 5
runsc --version | head -1
cat <<'MSG'
installed. Enabling it is three more steps, on EVERY pool node before the last one:
  kubectl label node <this node> kloudlite.io/gvisor=true     # runtimeclass.yaml schedules on it
  kubectl apply -f deploy/k3s/runtimeclass.yaml
  add WS_RUNTIME_CLASS=gvisor to the kloudlite-agent Secret and restart the DaemonSet
  (see the comment on WS_RUNTIME_CLASS in agent-daemonset.yaml — a node without the label
  leaves every tenant pod placed there Pending).
MSG
