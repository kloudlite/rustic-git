#!/usr/bin/env bash
# Provision the three k3s nodes. Idempotent enough to re-run after a partial failure: every
# create is guarded, so a half-finished run is resumed rather than duplicated.
#
# Network first, deliberately — the NSG must exist before any NIC, or a VM comes up briefly
# reachable on rules nobody chose.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
cd "$HERE"
. ./env.sh

# Required inputs are resolved BEFORE the first `az create`: a missing API_CLIENTS or a missing
# Cloudflare list must fail on an untouched subscription, not half way through a VNet. CF_CIDRS is
# the same Cloudflare edge list harden-node.sh admits on 80; API_CLIENTS is the api tier's egress,
# which needs 6443. Same required-var style as harden-node.sh: no silent 0.0.0.0/0 default.
CF_IPS_FILE="${CF_IPS_FILE:-$HERE/cloudflare-ips-v4.txt}"
[ -r "$CF_IPS_FILE" ] || { echo "CF_IPS_FILE $CF_IPS_FILE is not readable" >&2; exit 1; }
API_CLIENTS="${API_CLIENTS:?comma-separated CIDRs that may reach 6443 besides the VNet — at least the egress IP of the AKS api tier}"

have() { az "$@" >/dev/null 2>&1; }

az group create -n "$RG" -l "$LOC" -o none

have network vnet show -g "$RG" -n k3s-vnet || az network vnet create -g "$RG" -n k3s-vnet \
  --address-prefix 10.60.0.0/16 --subnet-name nodes --subnet-prefix 10.60.1.0/24 -o none

have network nsg show -g "$RG" -n k3s-nsg || az network nsg create -g "$RG" -n k3s-nsg -o none

# SSH from the operator only.
have network nsg rule show -g "$RG" --nsg-name k3s-nsg -n ssh || \
  az network nsg rule create -g "$RG" --nsg-name k3s-nsg -n ssh --priority 100 \
    --source-address-prefixes "$ADMIN_CIDR" --destination-port-ranges 22 --protocol Tcp --access Allow -o none

# Everything k3s needs is INTRA-cluster only: never expose 6443/10250/8472 to the internet.
have network nsg rule show -g "$RG" --nsg-name k3s-nsg -n k3s-api || \
  az network nsg rule create -g "$RG" --nsg-name k3s-nsg -n k3s-api --priority 200 \
    --source-address-prefixes 10.60.1.0/24 --destination-port-ranges 6443 --protocol Tcp --access Allow -o none
have network nsg rule show -g "$RG" --nsg-name k3s-nsg -n flannel-vxlan || \
  az network nsg rule create -g "$RG" --nsg-name k3s-nsg -n flannel-vxlan --priority 210 \
    --source-address-prefixes 10.60.1.0/24 --destination-port-ranges 8472 --protocol Udp --access Allow -o none
have network nsg rule show -g "$RG" --nsg-name k3s-nsg -n kubelet || \
  az network nsg rule create -g "$RG" --nsg-name k3s-nsg -n kubelet --priority 220 \
    --source-address-prefixes 10.60.1.0/24 --destination-port-ranges 10250 --protocol Tcp --access Allow -o none

# Two rules that lived only in README prose until now — two firewall layers that can drift silently
# is the failure this closes.
#
# `gateway-cloudflare` at 120 is NOT a new name: it is the hand-made rule already live in the NSG.
# Creating a second Allow-80 under another name would leave two rules to keep in sync and only one
# of them in this file — so the script adopts the existing one and stays a no-op on a live NSG.
have network nsg rule show -g "$RG" --nsg-name k3s-nsg -n gateway-cloudflare || \
  az network nsg rule create -g "$RG" --nsg-name k3s-nsg -n gateway-cloudflare --priority 120 \
    --source-address-prefixes $(tr '\n' ' ' < "$CF_IPS_FILE") --destination-port-ranges 80 --protocol Tcp --access Allow -o none
have network nsg rule show -g "$RG" --nsg-name k3s-nsg -n allow-apiserver-api-tier || \
  az network nsg rule create -g "$RG" --nsg-name k3s-nsg -n allow-apiserver-api-tier --priority 240 \
    --source-address-prefixes ${API_CLIENTS//,/ } --destination-port-ranges 6443 --protocol Tcp --access Allow -o none

az network vnet subnet update -g "$RG" --vnet-name k3s-vnet -n nodes --network-security-group k3s-nsg -o none

for spec in "$CP:$CP_SIZE:0" "$SESSION:$SESSION_SIZE:$POOL_DISK_GB" "$ENVN:$ENV_SIZE:$POOL_DISK_GB"; do
  IFS=: read -r name size disk <<<"$spec"
  if ! have vm show -g "$RG" -n "$name"; then
    # `--nsg ""` matters: the subnet NSG is the single place rules live; a per-NIC NSG would
    # silently shadow it.
    az vm create -g "$RG" -n "$name" --image "$IMAGE" --size "$size" \
      --admin-username "$ADMIN" --ssh-key-values "$SSH_KEY" \
      --vnet-name k3s-vnet --subnet nodes --nsg "" --public-ip-sku Standard -o none
  fi
  if [ "$disk" != 0 ] && [ -z "$(az vm show -g "$RG" -n "$name" --query "storageProfile.dataDisks[0].name" -o tsv)" ]; then
    az vm disk attach -g "$RG" --vm-name "$name" -n "$name-pool" \
      --new --size-gb "$disk" --sku Premium_LRS -o none
  fi
done

az vm list -g "$RG" -d -o table

# ---------------------------------------------------------------------------
# OCI equivalent. Everything after this script is identical on both clouds — only node creation
# and the disk attach differ, so there is no second copy of the k3s install to keep in sync.
#
# oci network vcn create --cidr-blocks '["10.60.0.0/16"]' --display-name k3s-vcn
# oci network security-list update --security-list-id "$SL" --ingress-security-rules file://rules.json
#   (the same four: 22 from $ADMIN_CIDR, and 6443/tcp, 8472/udp, 10250/tcp from 10.60.1.0/24)
# oci compute instance launch --shape VM.Standard.E5.Flex \
#   --shape-config '{"ocpus":16,"memoryInGBs":128}'   # OCI ocpus are half the vCPU count
# oci bv volume create --size-in-gbs 1024 && oci compute volume-attachment create --type paravirtualized
# ---------------------------------------------------------------------------
