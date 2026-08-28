#!/usr/bin/env bash
# Harden one k3s node. Idempotent; run as root on the node (`ssh azureuser@<node> sudo bash -s < harden-node.sh`).
#
# WHY each piece:
# - nftables default-drop on the public interface. The Azure NSG already gates inbound, but an NSG
#   is one console click from open; the node's own firewall is the second lock. Intra-VNet and the
#   pod overlay stay open (k3s, flannel, kubelet, the agent's peer calls), the Azure wire server
#   too (walinuxagent handshakes on it), everything else from the internet is dropped — including
#   the future SSH gateway port, which the agent opens per authorized session.
# - unattended-upgrades: security patches without anyone remembering to.
# - sshd: keys only, no root login. The cloud image already disables passwords; pinning it here
#   survives a package upgrade rewriting the config.
# - CF_CIDRS: the gateway's 80 is reachable only from the edge (TLS ends at Cloudflare). Cloudflare proxies
#   `ws-<region>.khost.dev` at the pool nodes, so anything reaching node:80 directly (not through
#   Cloudflare) is either a scanner or an attacker with the node's raw IP — admit the edge's
#   published ranges only, same list `deploy/ingress-nginx-config.yaml` trusts for the AKS side.
set -euo pipefail
ADMIN_CIDR="${ADMIN_CIDR:?the operator's CIDR (SSH, kubectl) — the NSG's ssh rule source}"
# Who may reach the k3s API besides the VNet: the operator, and the AKS api tier's egress IP (it
# writes every Workspace/Environment spec into this cluster). Control plane only.
API_CLIENTS="${API_CLIENTS:-}"
VNET="${VNET:-10.60.1.0/24}"
POD_CIDR="${POD_CIDR:-10.42.0.0/16}"
IFACE="$(ip -o -4 route show default | awk '{print $5}' | head -1)"
# Cloudflare's published v4 ranges for the gateway's 80, environment-only: this script is run
# streamed (`ssh … sudo bash -s < harden-node.sh`, per the doc comment above), which gives it no
# file of its own on the remote box to fall back to — `$0`/`BASH_SOURCE` is unbound stdin under
# `set -u` in that mode. Build the value locally instead:
#   CF_CIDRS="$(paste -sd, deploy/k3s/cloudflare-ips-v4.txt)"
# and pass it through explicitly, e.g.
#   ssh azureuser@<node> "sudo CF_CIDRS='$CF_CIDRS' ADMIN_CIDR='$ADMIN_CIDR' bash -s" \
#     < deploy/k3s/harden-node.sh
# Empty/unset means no 80 rule at all — 80 stays closed until a list is supplied, never
# open-by-default.
CF_CIDRS="${CF_CIDRS:-}"

cat > /etc/nftables.conf <<NFT
#!/usr/sbin/nft -f
# Written by deploy/k3s/harden-node.sh. Inbound-only: k3s's own iptables rules handle forwarding
# and NAT for pods, and we do not touch those chains.
table inet node {
  chain input {
    type filter hook input priority -10; policy drop;
    iif lo accept
    ct state established,related accept
    ct state invalid drop
    ip protocol icmp accept
    ip6 nexthdr icmpv6 accept
    # Azure wire server / instance metadata: the guest agent's control channel.
    ip saddr 168.63.129.16 accept
    # The cluster's own traffic: control plane, kubelet, flannel VXLAN, the agent's peer calls.
    ip saddr $VNET accept
    ip saddr $POD_CIDR accept
    iifname "cni0" accept
    iifname "flannel.1" accept
    # Operator SSH.
    iifname "$IFACE" tcp dport 22 ip saddr $ADMIN_CIDR accept
$(for c in $(printf "%s\n" $ADMIN_CIDR ${API_CLIENTS//,/ } | sort -u); do echo "    iifname \"$IFACE\" tcp dport 6443 ip saddr $c accept"; done)
$(if [ -n "$CF_CIDRS" ]; then echo "    iifname \"$IFACE\" tcp dport 80 ip saddr { ${CF_CIDRS} } accept"; fi)
    # Everything else from the internet is dropped, silently.
  }
}
NFT
# Validate before destroying anything: a malformed ruleset (bad CF_CIDRS syntax, say) must fail
# here, before the old table is gone, not leave the node with no table at all.
nft -c -f /etc/nftables.conf || { echo "nftables ruleset is invalid, aborting before touching the live table" >&2; exit 1; }
# Replace only OUR table: a `flush ruleset` would also wipe the iptables-nft rules k3s and flannel
# program for the pod network, and take every pod off the network with it.
nft delete table inet node 2>/dev/null || true
nft -f /etc/nftables.conf
systemctl enable --now nftables >/dev/null

apt-get install -y -qq unattended-upgrades >/dev/null
cat > /etc/apt/apt.conf.d/20auto-upgrades <<APT
APT::Periodic::Update-Package-Lists "1";
APT::Periodic::Unattended-Upgrade "1";
APT
systemctl enable --now unattended-upgrades >/dev/null

cat > /etc/ssh/sshd_config.d/90-hardened.conf <<SSH
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin no
PubkeyAuthentication yes
SSH
sshd -t && systemctl reload ssh
echo "hardened: nftables (drop by default on $IFACE), unattended-upgrades, sshd keys-only"
