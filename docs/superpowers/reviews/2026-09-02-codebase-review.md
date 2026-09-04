# Codebase review — 2026-09-02

Whole-tree, read-only. Six passes (core/storage/server, registry/pulls/worker, workspaces/agent,
deploy/security, web ×2 — the second read the repo-browse and PR UI the first skipped), every finding carries a file:line the reviewer read, and the
highest-severity ones were re-checked by hand against the tree at `7cc1e720`. Nothing here is
fixed yet; this document is the list to work from.

Totals: **Critical 0 · High 8 · Medium 27 · Low 30.** The load-bearing rules in CLAUDE.md were
checked one by one and hold everywhere (blob deletion sites, verbatim manifests, `Digest::parse`,
routing before auth, keep-bias in every reaper, the peer-secret argv split). The Highs are gaps
beside those rules, not violations of them.

## Fix first (High)

| # | Where | What | Fix |
|---|---|---|---|
| 1 | `deploy/k3s/agent-admission.yaml:83-87` vs `crates/workspaces/src/crd.rs:752-757` | Team workspaces live in `wt-{owner}-{tail}` namespaces; the agent's admission policy admits only `ws-`/`env-`, with `failurePolicy: Fail`. Every team workspace is denied at namespace, host-key Secret and both RoleBindings. Confirmed by reading both. | Add `startsWith('wt-')` to all three branches. Add a unit test asserting every `ws_namespace` output matches the policy's prefix set. |
| 2 | `deploy/k3s/agent-admission.yaml:66-77`, `agent-rbac.yaml:191-193`, `workspace-admission.yaml:79-84` | The agent holds cluster-wide `pods: create` (also statefulsets, services, networkpolicies, limitranges); the tenant-namespace policy covers only namespaces/secrets/rolebindings and the pod fence excludes kube-system. A compromised agent creates a privileged hostPath pod in kube-system: node root becomes cluster root. Confirmed. | Extend policy 2's resourceRules to those five kinds with the same namespace-prefix test. |
| 3 | `deploy/k3s/zerofs.yaml:65,148-160` | Every owner's home is an unauthenticated NFSv3 export (AUTH_SYS, `nolock`) on a ClusterIP with no NetworkPolicy in `kloudlite-git-system`, beside the internet-facing gateway. Tenant pods are kept off it only by their egress policy's RFC-1918 exclusion. | Default-deny NetworkPolicy in `kloudlite-git-system` plus one ingress rule for 2049 from `app=kloudlite-git-agent` in kube-system, same shape as `agent-peer.yaml`. |
| 4 | `bins/server/src/router/git.rs:126-130` | Upload-pack negotiation body is capped at `max_body()` (2 GiB) and buffered in memory, reachable anonymously on public repos. Its own comment says "kilobytes". A few concurrent posts OOM the pod and move repo ownership on the attacker's schedule. Confirmed. | Own constant (a few MiB) for the negotiation; keep `max_body` for receive-pack's streamed path. Add the missing test beside `tests/git_http_limits.rs:81`. |
| 5 | `crates/workspaces/src/k8s.rs:847,868,905,429` | `workspace_pod` is the one pod builder that does not re-validate its input: `spec.name` goes verbatim into a root `/bin/sh -c` prelude, sshd `SetEnv` and the container `mount_path`. Its siblings (`git_init_container:792`, `service_statefulset:1020`) re-check and say why. Only `/v1` checks the name (`api.rs:507`). Blast radius is the tenant's own pod. | Make `workspace_pod` fallible on `model::valid_ws_name`, settle `Permanent/InvalidName` at the call site. |
| 6 | `bins/agent/src/peer.rs:324-1002` | `pull_beat` issues ~13 cluster-wide LISTs per node per beat, plus a full `VolumeReplica` list per volume inside `pull_volume` (`:501`) and a pod LIST per commit per source (`:532`). O(V) per node, N× cluster-wide, every 300 s. | Hoist Volumes, VolumeReplicas, Workspaces, Environments into one listing at the top of `pull_beat_with` and thread them through, as `nodes/floor/now` already are. Same refactor fixes retention's cluster scans (`snapshot.rs:215,231`) and collapses four copies of the "parents on this node by volumeRef" query. |
| 8 | `web/apps/web/src/lib/api.ts:936` vs `crates/workspaces/src/api.rs:2027` | `/v1/volumes/{name}/history` emits `createdAt`; the TS type and all eight readers use `created_at`, so every snapshot time renders `Invalid Date` and the restore-cutoff and ordering comparisons in `env-snapshots.tsx:349,362` silently degrade. Confirmed. | Rename the TS field to `createdAt` and update the readers; add a type test against a recorded response. |
| 7 | `deploy/k3s/workspace-admission.yaml:49-70` | gVisor is live (the running pod shows `runtimeClass=gvisor`, set via the Secret), but nothing enforces it: no validation requires `spec.runtimeClassName == 'gvisor'`, so the kernel boundary rests on one env var. The reviewer's stronger claim ("installed but not enabled") was wrong; this residual stands. | Fourth validation in the pod fence requiring the runtimeClass. |

## Medium

### Security and isolation
- `workspace-admission.yaml:55-62` — the pod fence checks `privileged`/`allowPrivilegeEscalation` only; `capabilities.add` is unchecked (`k8s.rs:668-671` says the fixed list is "the only thing that does"). Assert `add` ⊆ the seven `hardened()` grants.
- `workspace-admission.yaml:63-70` — hostPath allow-list is "anything under `/wspool-prod/`", so a builder bug can mount another tenant's volume or home. Require the four known subtrees and the pod's owner segment.
- `agent-admission.yaml:29-32`, `agent-rbac.yaml:147-156` — spec-read-only policy matches UPDATE on four resources; the agent can CREATE a Volume with any spec and PATCH a Snapshot's `parent` (grafts history). Extend to CREATE and to snapshots/volumereplicas; restrict Snapshot patch to metadata.
- `api-rbac.yaml:59-68` — `kloudlite-git-api-secrets` is full CRUD on every Secret in a workspace namespace, including the agent's `ws-ssh-{id}` host keys. `resourceNames: [git-token, user-key]`.
- `bins/kl/src/config.rs:95-106` — `pin_host_key` overwrites the stored key on every connect; it is not a pin. With the previous item, an API compromise MITMs every workspace SSH silently. Compare and refuse loudly.
- `harden-node.sh:59-62` — blanket `cni0`/pod-CIDR accept admits any pod to kubelet 10250 and API 6443; only the tenant egress policy stands between a pod and node takeover. Per-port accepts, or document the pod network as trusted.
- `bins/agent/src/controller.rs:1910`, `engine/ops.rs` (`ensure_homecache`) — `spec.owner` becomes a root-chowned path with no `valid_owner` check; blocked today only because `heal_labels` runs first and the API server rejects the label. One `valid_owner` guard at the top of `apply_workspace`/`apply_environment`, and a CRD `pattern`.
- `crates/storage/src/index.rs:60-90`, `crates/api/src/repos.rs:60` — repo description may contain `\n`, forging `created_by=` in listing markers. Reject control chars in `check_description`.
- `crates/app/src/lib.rs:411-422` — unauthenticated claim amplification: any invented repo path costs the single elected writer one map write per lease TTL. Build the token bucket the `ponytail:` note names, or claim only if the prefix exists.
- `bins/server/src/router/route.rs:557-562` — `/metrics` on the peer listener without the peer secret; any pod reads repo names and ownership state. Own port, or require the secret.

### Correctness
- `crates/registry/src/blobs.rs:24-27`, `uploads.rs:872` — `max_layer` defaults to 10 GiB but the multipart fast path ends in a server-side copy capped at 5 GiB: a 6 GiB layer uploads, hashes, then 500s. Default 5 GiB or clamp when `store.mp` is set.
- `crates/registry/src/store.rs:210-242` — blob-row backfill runs inline on a stranger's pull, unbounded, unlocked, and turns a transient store error into a 500. `keyed_lock` + `buffered(16)` + treat a failed GET as not-held.
- `bins/server/src/browse_api/pulls.rs:160-192` — `base`/`head` stored unvalidated; a change on an illegal ref is permanently unmergeable and re-announced every 30 s. Reject at open with `check-ref-format` basics.
- `pulls.rs:521-554` — `api_pull_mergeability` accepts a stale verdict from a lapsed lane (the outcome route guards against exactly this at `:463`). Stamp with the tips it was computed from.
- `crates/workspaces/src/api.rs:1465` — `delete_env` lists every Workspace cluster-wide and silently swallows the list error, leaving workspaces attached to a deleted environment. Select by owner label; log the Err.
- `bins/agent/src/controller.rs:2006`, `:1685` — `ensure_shared_home` (up to 65 s of `mount`/`umount`/`ls` syscalls) and `write_resolv_conf` run synchronously on the reconcile reactor; the next line uses `spawn_blocking`. Wrap both. (Introduced by this week's mount-repair change.)
- `deploy/k3s/agent-peer.yaml:9-11` — the ingress policy silently blocks the agent's 9464 metrics port; the comment claims it restricts 8444 only. Add the 9464 rule.
- `agent-daemonset.yaml:163`, `zerofs.yaml:104` — `alpine/git:2.45.2` (runs with the owner's platform key) and `zerofs:2.3.2` pinned by mutable tag while the same files pin others by digest. Pin `@sha256`.
- `zerofs.yaml` — no memory limit, no liveness probe, on the control-plane node. Both.
- `agent-daemonset.yaml:271-286` — nix-daemon runs privileged with no requests/limits beside tenants.
- `agent-rbac.yaml:53,55` — the header table (which "IS the role") miscounts `/status` (6, says 5) and `/finalizers` (4, says 5).

### Web
- `src/components/repo/commits.tsx:39` — hand-rolled ref resolution instead of `resolveRef`, so `?ref=<sha>` (links the app itself generates) lists the default branch's history. Use `resolveRef`.
- `src/lib/browse.ts:63` — `filePath` escapes segments but lets `.`/`..` through, so a `%2e%2e` segment collapses the api URL onto a different endpoint. Not an authz bypass (token plus api re-check), but the file's stated guarantee is one filter away. Reject those segments.

### Performance
- `crates/registry/src/routes.rs:52`, `manifests.rs:189-193` — unbounded `join_all` fan-out where `gc.rs` bounds the same work at 16. Use `buffered(16)`.
- `bins/agent/src/controller.rs:2997` — `flush_gate` lists every VolumeReplica per 15 s tick per stopping parent. Add `.spec.volume` to the CRD's `selectableFields` (it already has `.spec.node`, `.status.phase`) and the client-side filter goes away here and in `peer.rs:501`.
- `bins/agent/src/snapshot.rs:215,231` — retention lists every parent cluster-wide on every push. Select on `VOLUME_LABEL`.

## Low (24)
Core: `mem://` accepted as a fleet store (`storage/config.rs:119`), `forget_pack_public` alias (`store.rs:620`), `ssh_fingerprint` duplicated across `boot.rs:38` and `credentials.rs`, `revoke_tokens_for` full list (`auth.rs:117`), `io::ErrorKind::Other` echoed to clients (`git.rs:498`), three missing tests (negotiation cap, newline forgery, credential-half mismatch).
Registry: `delete_blob` leaves hold rows (`blobs.rs:206`), `?n=0` ends the catalog (`lib.rs:118`), pull counted before existence (`manifests.rs:326`), duplicate suffix-delete loops (`store.rs:182` / `referrers.rs:63`), merge cache lock held across the whole job (`worker/main.rs:255`), `last` unstripped (`routes.rs:157`), no test that manifest DELETE spares blobs.
Workspaces: per-node `NamespaceReady` gate read off a cluster-wide condition (`binding.rs:44/66/140`), team slugs interpolated into a label selector (`api.rs:1739`), `Done::lineage_tip` dead (`controller.rs:189`), `live_state` always Null (`api.rs:410`), phantom `WS_PEER_RECV_TIMEOUT_SECS` in a comment (`peer.rs:248`), `dir_bytes("/nix/store")` walk every 10 min (`janitor.rs:42`, already marked).
Deploy: `allow-dns` reaches every kube-system pod on 53 (`k8s.rs:1176`), backup uses unauthenticated AES-CBC (`backup-controlplane.sh:66`), gateway jti replay across replicas (`tunnel.rs:38`, marked), NSG vs nftables rules kept in prose (`provision-azure.sh` / README), long-lived k3s kubeconfig Secret for the api tier with no rotation step (`kloudlite-git.yaml:452`).
Web: environment `id` reaches `revalidatePath` unvalidated in the env actions (add `safeSegment`), `StateBadge` throws on an unknown pull state (fallback to `open`), README fallback is a serial extra round trip, unbounded file/commit/PR lists (all already marked), one dead export in `src/lib`, `dangerouslySetInnerHTML` on shiki output (`code-block.tsx:12`, shiki escapes; leave a comment), rounded utilities on dots/pills only (deliberate).

## Tests worth adding (highest value first)
1. `k8s::attach_egress/attach_ingress` (`k8s.rs:1291,1312`): assert `namespaceSelector` and `podSelector` sit in ONE peer. The comments say the two-peer form opens every sshd to every pod. Pure JSON assertion, untested.
2. `workspace_pod` with a hostile `spec.name` (High #5).
3. Upload-pack negotiation cap (High #4).
4. `write_resolv_conf` inode survives a rewrite (the "never rename" invariant a refactor will break).
5. `mkdir_env_mounts` refuses a traversing folder at the controller.
6. `retain`'s transient arm spares commits and other worktrees' sync points.
7. Every `ws_namespace` output satisfies the admission policy's prefix set (High #1).

## Architecture
1. **One "parents and volumes on this node" listing per beat**, threaded through `interesting_volumes`, `pull_volume`, `release_dead_volumes`, `hosted_volumes`, `retire_pass`, `sync::live_worktrees`, `snapshot::worktree_heads`. Removes ~9 of 13 listings, the O(V) term, and four copies of the same query. Plumbing only; no decision logic moves.
2. **Split `bins/agent/src/controller.rs` (3162 lines)** along its existing seams: `mod.rs` (Ctx/Work), `run.rs`, `status.rs`, `volume.rs`, `workspace.rs`, `environment.rs`, `stop.rs`. Start with `stop.rs` and `status.rs`; both are already kind-agnostic. The remaining Workspace/Environment duplication (the ~60-line worktree materialize block at `2013-2119` vs `2530-2574`, `unclaim_kind`'s four closures, `claim_workspace/claim_environment`, the api's `restore/push/start/stop` pairs) becomes obvious once the files are small.
3. **Uniform untrusted-CR validation at the agent**: one `validate_spec` at the top of `apply_workspace`/`apply_environment` (name, owner, team, packages) instead of relying on a label patch to fail first. Closes High #5 and the `spec.owner` Medium together.
4. **Admission headers are the design doc, and both policies are narrower than their headers claim** (High #1, #2, Medium ×3). Every naming rule that lives in Rust and is enforced in CEL needs a test tying them together.
5. **Everything expensive on the git server happens before authentication** (claim in `App::route`, body buffering in `read_body`). Existence-gated claims and a route-sized body cap close High #4 and the amplification Medium with no new machinery.
6. **Tenant isolation is three independent things**: pod fence, NetworkPolicy, gVisor. Make the fence require the third, bound the first (caps, hostPath subtree), and put ZeroFS behind a policy so the second is not the only line to the homes.

## Suggested order
1. Manifests, one commit, apply immediately: High #1, #2, #3, #7; Medium admission/RBAC items; the two image digests; agent-peer 9464; RBAC table counts.
2. Server: High #4 with its test; description newline check; claim token bucket.
3. Agent: High #5 + owner guard (+ tests 1, 2, 4, 5); `spawn_blocking` the two sync calls; `selectableFields` for `spec.volume`.
4. Registry: `max_layer` default; backfill lock; `buffered(16)`; ref-name validation at PR open.
5. Web: `createdAt` rename (High #8); `resolveRef` in commits; `..` filter in `browse.ts`; `safeSegment` on env ids.
6. Refactors: shared per-beat listing; controller split.
