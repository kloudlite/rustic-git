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
set -euo pipefail
ADMIN_CIDR="${ADMIN_CIDR:?the operator's CIDR (SSH, kubectl) — the NSG's ssh rule source}"
# Who may reach the k3s API besides the VNet: the operator, and the AKS api tier's egress IP (it
# writes every Workspace/Environment spec into this cluster). Control plane only.
API_CLIENTS="${API_CLIENTS:-}"
VNET="${VNET:-10.60.1.0/24}"
POD_CIDR="${POD_CIDR:-10.42.0.0/16}"
IFACE="$(ip -o -4 route show default | awk '{print $5}' | head -1)"

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
    # Everything else from the internet is dropped, silently.
  }
}
NFT
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
