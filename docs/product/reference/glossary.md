# Glossary

**Attach** — Point a workspace's DNS at an environment so its services resolve by name.

**Builder** — A hidden per-owner BuildKit that `kl build` uses. Started on demand, stopped when idle, never listed.

**Clone** — A new workspace or environment from another's current bytes.

**Detached volume** — A volume whose working copy was deleted; it lives while a snapshot references it.

**Environment** — A set of services with one shared disk, in its own namespace.

**Home** — `/home/kl`: one directory per person per region, mounted into every workspace of theirs.

**Intercept** — Route one environment service's traffic to a workspace by rewriting its endpoints.

**Interrupted** — A running workspace or environment whose node is down. It resumes when the node returns; a workspace can be cloned meanwhile.

**Lock** — The resolved store path for a pinned package, frozen into the workspace.

**Owner** — A person's handle or a team's slug. Everything belongs to one.

**Placement** — The node a working copy runs on.

**Push** — Take a snapshot you keep, with an optional message. Also: `kl push`, copying an image to another name.

**Quota** — Six per-owner ceilings: workspaces, environments, snapshots, disk, cpu, memory.

**Region** — One cluster: nodes, home export, nixpkgs pin, gateway.

**Replica** — A copy of a volume's newest sync point on another node.

**Request** — An ask an administrator decides: quota, access, region, other.

**Restore** — A new working copy from a snapshot; re-attaches the volume.

**Restore in place** — Swap an environment's disk for a snapshot's without changing its identity.

**Service** — One container in an environment, named by its DNS label.

**Snapshot** — A read-only point-in-time copy of a tree or disk. Pushed snapshots are history; sync points are internal.

**Sync point** — A snapshot the platform cuts on its own for replication. Never listed, never a restore target.

**Volume** — The disk under a working copy, with its snapshots and replicas.

**Workspace** — A running pod with your tree, your packages, and ssh.
