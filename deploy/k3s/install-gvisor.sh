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

ARCH="$(uname -m)"
URL="https://storage.googleapis.com/gvisor/releases/release/latest/${ARCH}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "==> fetching runsc"
cd "$TMP"
wget -q "${URL}/runsc" "${URL}/runsc.sha512" "${URL}/containerd-shim-runsc-v1" "${URL}/containerd-shim-runsc-v1.sha512"
# The checksums are the whole reason this is not a curl-into-bash: a sandbox binary fetched without
# verification is a rootkit with extra steps.
sha512sum -c runsc.sha512
sha512sum -c containerd-shim-runsc-v1.sha512
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
echo "installed. Apply deploy/k3s/runtimeclass.yaml, then set WS_RUNTIME_CLASS=gvisor in the agent Secret."
