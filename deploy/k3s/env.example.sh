# Copy to deploy/k3s/env.sh (git-ignored) and edit. NO SECRETS HERE — tokens go via scp/kubectl.
CLOUD=azure                     # azure | oci  — only this file differs between them
RG=kloudlite-git-k3s
LOC=centralindia                # where the directory's Cosmos account and blob storage already live
IMAGE=Canonical:ubuntu-24_04-lts:server:latest
ADMIN=azureuser
SSH_KEY=~/.ssh/id_rsa.pub

# Sizing. These are vCPU counts, not Oracle OCPUs: 32 vCPU / 128 GB for sessions, 16 / 128 for
# environments. Azure resizes in place, so starting here and growing later costs one reboot.
CP_SIZE=Standard_D2s_v5         # control plane: 2 vCPU / 8 GB, hosts no workloads
SESSION_SIZE=Standard_D32s_v5   # session worker: 32 vCPU / 128 GB
ENV_SIZE=Standard_E16s_v5       # env worker:     16 vCPU / 128 GB
POOL_DISK_GB=1024               # per-worker dedicated data disk -> btrfs -> /wspool-prod

CP=k3s-cp; SESSION=session-0; ENVN=env-0

# Build box. NOT a cluster node, deliberately: the toolchain used to live on session-0, and a Rust
# build tree plus Docker filled its OS disk, tainted the node `disk-pressure`, stopped all
# scheduling and had the kubelet garbage-collect the agent image. F-series because a build is
# CPU-bound, and a large OS disk because that is what actually ran out.
BUILD=build-0
BUILD_SIZE=Standard_F16s_v2
BUILD_DISK_GB=256

# SSH ingress is scoped to this. A residential IP changes — re-run the ssh rule when it does.
# 0.0.0.0/0 here is a finding, not a default.
ADMIN_CIDR=203.0.113.1/32

# CIDRs that may reach 6443 besides the VNet — at least the AKS api tier's egress IP.
API_CLIENTS=198.51.100.1/32

# One CIDR per line, tr'd into the NSG rule's --source-address-prefixes list. Defaults to the
# checked-in cloudflare-ips-v4.txt; only set this to point elsewhere.
CF_IPS_FILE=cloudflare-ips-v4.txt

