# Copy to deploy/k3s/env.sh (git-ignored) and edit. NO SECRETS HERE — tokens go via scp/kubectl.
CLOUD=azure                     # azure | oci  — only this file differs between them
RG=rustic-git-k3s
LOC=centralindia                # where the Cosmos accounts and blob storage already live
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

# SSH ingress is scoped to this. A residential IP changes — re-run the ssh rule when it does.
# 0.0.0.0/0 here is a finding, not a default.
ADMIN_CIDR=203.0.113.1/32

WS_REGISTRY_URL=https://git.khost.dev   # server tier's agent work surface (NOT bins/api)
