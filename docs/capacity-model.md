# Capacity model

Source: the owner's capacity model sheet, 2026-09-06, section "Slot sizing (M = default session,
4 GB env = default environment)". The sheet's word for a workspace is **session**; an **env unit** is
one environment SERVICE. This file uses the product's words, with the sheet's noted once here.
There is one kind of node (`kloudlite.io/pool=true`) hosting both, admitted up to 100 % of the
guarantee; the sheet's separate "env node" packed to 80 % never existed on the fleet and was removed
from the code on 2026-09-09.

| Slot | Guaranteed (request) | Limit | Where the numbers live |
| --- | --- | --- | --- |
| Workspace ("M session") | 1 GB memory, 500m vCPU | 8 GB, 4 vCPU | `crd::PodResources::default` |
| Environment service ("env unit") | 2730Mi memory, 250m cpu | 4 GB, 2 vCPU | `k8s::env_unit_resources` |

Packing rules, and what reads them:

| Rule | Value | Read by |
| --- | --- | --- |
| Pack on the GUARANTEE, never the limit | — | `claim::fits` |
| Guaranteed CPU is not oversubscribed on workspace nodes | 1× | `claim::fits` (100% of allocatable) |
| Environment memory oversubscription (limit ÷ packed) | 1.5× | the 2730Mi request itself |
| Env-node target packing — envs are steady state, pack tighter | 80% | `claim::fits` (env pool only) |
| Workspace-node average utilisation | 45% | **pricing only** — never a scheduling allowance |
| Env pool on preemptible | 0 | decided, nothing to read |

The 45% figure is an assumption about how bursty a workspace is over a day. It prices the fleet.

**The workspace request was cut to 1 GB / 500m on 2026-09-17** — the sheet's "M session" row is the
LIMIT now, not the request. Requesting the peak while a workspace sat idle declined ten hourly
starts for minutes across three 8-core nodes that were themselves nearly idle
(`claim.declined.capacity gate=start`). What a person was sold is the burst, and the burst is the
limit; the request is what an idle workspace occupies, so a node holds the workspaces people really
keep open. The consequence is deliberate and stated here rather than discovered: CPU is now
oversubscribed at peak (128 workspaces × 4 vCPU of limit on 64), which is exactly the 45%
utilisation assumption above being spent. Memory is still the binding dimension.

1 OCPU = 2 vCPU. Node allocatable already excludes the kubelet's reserved capacity and eviction
threshold, so nothing here adds a second system margin on top of it.
