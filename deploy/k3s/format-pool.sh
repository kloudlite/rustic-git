#!/usr/bin/env bash
# Format a worker's dedicated data disk as the btrfs workspace pool and mount it at /wspool-prod.
#
# Run on the worker, as root, with the DEVICE as the only argument. Identify that device by SIZE
# (`lsblk -dno NAME,SIZE,TYPE`), never by a remembered `/dev/sdc` — device names reorder across
# reboots, and formatting the wrong one is unrecoverable.
set -euo pipefail
DEV=${1:?device, e.g. /dev/sdb}

# The whole safety of this script: an existing filesystem means this disk is already somebody's
# pool, so re-running can never eat one.
if blkid "$DEV" >/dev/null 2>&1; then
  echo "refusing: $DEV already has a filesystem" >&2
  exit 1
fi

mkfs.btrfs -L wspool "$DEV"
mkdir -p /wspool-prod
UUID=$(blkid -s UUID -o value "$DEV")
# By UUID, for the same reason the device is chosen by size: a name is not a stable identity.
grep -q "$UUID" /etc/fstab || echo "UUID=$UUID /wspool-prod btrfs defaults,noatime 0 0" >> /etc/fstab
systemctl daemon-reload
mount /wspool-prod
# Per-filesystem, once: without it every `btrfs qgroup limit` the agent applies (a volume's
# `spec.quotaGb`) fails and the Volume reports `QuotaEnforced=False`. An existing pool gets this
# by hand — `btrfs quota enable /wspool-prod` — and rescans once.
btrfs quota enable /wspool-prod
findmnt -no TARGET,FSTYPE /wspool-prod
