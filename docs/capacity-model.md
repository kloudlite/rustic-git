# Capacity model

Source: the owner's capacity model sheet, 2026-09-06, section "Slot sizing (M = default session,
4 GB env = default environment)". The sheet's word for a workspace is **session**, and a **session
node** is a node in the workspace pool (`kloudlite.io/session=true`); an **env unit** is one
environment SERVICE. This file uses the product's words, with the sheet's noted once here.

| Slot | Guaranteed (request) | Limit | Where the numbers live |
| --- | --- | --- | --- |
| Workspace ("M session") | 4 GB memory, 2 vCPU | 8 GB, 4 vCPU | `crd::PodResources::default` |
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

The 45% figure is an assumption about how bursty a workspace is over a day. It prices the fleet; it
must never admit a pod beyond the guarantees, because the guarantee is what the person was sold.

1 OCPU = 2 vCPU. Node allocatable already excludes the kubelet's reserved capacity and eviction
threshold, so nothing here adds a second system margin on top of it.
