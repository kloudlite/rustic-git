# Workspace egress fence

Date: 2026-10-09. Status: design, awaiting owner review.

## Why

A bench session's permission card waits forever when no client is attached, so the pod never
sleeps (E11). The owner picked option (b): "auto-approve what runs inside the sandbox", then
"we need to fix these two catches". Catch 1 (exec silently unwrapped) shipped in 4efb7100: the
tool server reports `fence: {sandbox, network}` on `GET /tools`, and the harness auto-approves
`exec` only when both read `active` / `fenced`. This spec is catch 2: today `network` is always
`open`, because every workspace pod may reach any public address (`allow-internet-egress`,
`0.0.0.0/0` minus RFC 1918), so an agent's command can send the person's code anywhere.

Owner decisions ("go on" to the four picks, 2026-10-09):

1. The fence covers the whole workspace pod, the person's own terminal included (exec shares the
   pod's network). Per workspace: `fenced` (default) or `open`; an open workspace keeps asking
   before exec.
2. A host not on the list is refused with a message naming the fix; no permission card.
3. Each owner (person or team) keeps one extra allow-list, on top of a platform base list.
4. `web_fetch` keeps asking.

## Shape

One forward proxy per region, `kloudlite-egress`, in `kloudlite-system`. A fenced workspace pod
may reach only the proxy (plus DNS, OTLP, its namespace, builder-gate — unchanged). The proxy
answers `CONNECT host:port`, looks up who is calling by source pod IP, and opens the tunnel only if
the host is on the base list or the caller's owner list.

```
workspace pod ──CONNECT github.com:443──▶ kloudlite-egress ──▶ github.com
   (HTTPS_PROXY set)                       │ pod IP → Workspace → spec.owner
                                           │ base list ∪ Egress/{owner}.spec.allow
                                           └ else 403 + reason
```

Scope: workspace pods only (`kloudlite.io/kind=workspace`). Bench pods, environment services and
builders keep `allow-internet-egress` as today. Consequences, decided:

- Bench stays open, so main-session `bash` and `web_fetch` keep asking (decision 4 holds without
  routing Anthropic traffic through the proxy). The bench branch of `mustAsk`
  (`KLOUDLITE_EGRESS` in the bench process) stays, unset.
- Builders stay open, and a Dockerfile `RUN` can reach the internet, so `container_build` moves to
  `ALWAYS_ASK` (`harness/packages/backend/src/local.ts`). `// ponytail:` note: fencing the builder
  needs proxy build-args on every build; do it when unattended builds matter.

## Pieces

### 1. `bins/egress` (new binary, new image `kloudlite-egress`)

- tokio TCP listeners on `:3128` (CONNECT) and `:2222` (git forwarder, below), no hyper, in the shape of `bins/intercept-proxy` (static musl,
  `FROM scratch`).
- Reads one request head (max 8 KiB). Only `CONNECT host:port HTTP/1.1` is served; anything else
  answers `405` with body `egress: only CONNECT is proxied; use https`. Ports allowed: 443 and 22.
- Caller: the TCP peer address is looked up in a pod reflector (label `kloudlite.io/kind=workspace`,
  cluster-wide) → label `kloudlite.io/workspace` → Workspace reflector → `spec.owner`. Owner list
  from an `Egress` reflector. Reflectors before their first list are UNKNOWN: answer `503`
  `egress: starting`, never treat as empty. A peer IP with no workspace pod answers `403`
  `egress: unknown caller`.
- Host match: exact, or `*.example.com` matching any subdomain (not the apex). Hosts compared
  lowercase, without a trailing dot. An IP literal is refused unless listed literally.
- Refusal: `403`, header `X-Kloudlite-Egress: refused`, body
  `egress: {host} is not on {owner}'s allow-list; add it with: kl net allow {host}`. Logged
  `egress.refused {owner} {workspace} {host}:{port}`; tunnels logged `egress.tunnel` with byte
  counts on close.
- Tunnel: `copy_bidirectional`, idle timeout 10 min, dial timeout 10 s (`502` on failure).
- Base list = const `BASE_ALLOW` in the binary plus env `EGRESS_ALLOW` (comma list, our own hosts
  from the manifest):
  - const: `github.com`, `*.github.com`, `*.githubusercontent.com`, `registry.npmjs.org`,
    `registry.yarnpkg.com`, `crates.io`, `*.crates.io`, `pypi.org`, `files.pythonhosted.org`,
    `proxy.golang.org`, `sum.golang.org`, `cache.nixos.org`, `registry-1.docker.io`,
    `auth.docker.io`, `production.cloudflare.docker.com`, `ghcr.io`, `pkg-containers.githubusercontent.com`.
  - manifest: the `KL_API_URL` host, `KL_REGISTRY_HOST`, `WS_GIT_SSH_HOST`, the binary-cache host.
- Manifest `deploy/k3s/egress.yaml`: Deployment, 2 replicas, label `app=kloudlite-egress`, uid 1001
  read-only root, Service `egress.kloudlite-system.svc` ports 3128, 2222, ServiceAccount with list/watch on
  pods, workspaces, egresses; ingress NetworkPolicy if `kloudlite-system` has one. Image wired into
  `Cargo.toml` members, `Dockerfile`, `deploy/dagger/src/index.ts` (musl list, image method,
  `IMAGE_BUILDERS`), `deploy/pin.sh`.

### 2. `Egress` CRD (cluster-scoped, one per owner slug)

- `crates/workspaces/src/crd/egress.rs`: `spec.allow: Vec<String>` (max 200 entries, each a valid
  host or `*.host`, validated by `egress::parse_host`, shared with the proxy). Registered in
  `all_crds()`, `deploy/k3s/crds.yaml` regenerated (held by `tests/crd_yaml.rs`).
- Writer: `bins/api` only, SSA with `crd::API_FIELD_MANAGER`, the `OwnerKeys` pattern
  (`crates/workspaces/src/api/keys/mod.rs`). RBAC: `api-rbac.yaml` writes, egress SA reads.
- API (user role): `GET /v1/egress/{owner}` → `{base, allow}`; `PUT /v1/egress/{owner}` with
  `{allow}`; both gated by `may_act(caller, owner)`. Ingress allow-list regex gains `egress`.
- CLI `kl net`: `list`, `allow <host>...`, `remove <host>...` (read-modify-PUT), owner = the
  workspace's owner by default, `--owner` to pick a team.

### 3. Workspace `spec.network`

- `crd::Workspace.spec.network: NetworkMode` = `Fenced` (default when absent) | `Open`. Written only
  by `/v1` (create body and the existing workspace patch). Read at pod build, so a change applies
  on the next start; running pods are never restarted for it.
- Pod (`workspace_pod`, `login_env` in `crates/workspaces/src/k8s/workspace.rs`), kind=workspace
  only:
  - label `kloudlite.io/network=fenced|open`;
  - fenced adds env `HTTPS_PROXY`, `HTTP_PROXY`, `https_proxy`, `http_proxy` =
    `http://egress.kloudlite-system.svc:3128`, `NO_PROXY`/`no_proxy` =
    `localhost,127.0.0.1,.svc,.cluster.local,10.0.0.0/8`, and `KLOUDLITE_EGRESS=fenced` (the
    switch the tool server already reads). The seed init container gets the same env.
  - git over SSH, main container: `GIT_SSH_COMMAND=ssh -o ProxyCommand='kl net connect %h %p'`;
    `kl net connect` speaks CONNECT to `$HTTPS_PROXY` and pipes stdio (`kl` is in
    `/usr/local/bin` of every workspace image).
  - git over SSH, seed init container: its image (`alpine/git`) has no `kl` and no proxy-capable
    `nc`, so the proxy also listens on `:2222` as a fixed forwarder to `WS_GIT_SSH_HOST` (the
    `intercept-proxy` shape, no request head). A fenced pod's seed clones from
    `egress.kloudlite-system.svc:2222`; `StrictHostKeyChecking=accept-new` already accepts the
    key on a fresh home.
- Policies (`crates/workspaces/src/k8s/policies.rs`, applied by the agent in `binding.rs`), owner
  namespaces only (`ws-`, `wt-`); environment namespaces untouched:
  - `allow-internet-egress` podSelector becomes `kloudlite.io/network NotIn [fenced]`, so bench
    pods, open workspaces AND running pods built before this change keep the internet (no pod is
    cut off mid-session; standing rule).
  - new `allow-egress-gate`: all pods egress to ns `kloudlite-system` + `app=kloudlite-egress`,
    tcp 3128 and 2222.

### 4. Harness

- `container_build` → `ALWAYS_ASK`. No other gate change: `exec` already auto-approves on
  `active` + `fenced`.

## Known ceilings (`// ponytail:` in code)

- DNS: `allow-dns` still lets a pod query CoreDNS, which resolves public names, so data can leave
  as DNS lookups. Upgrade: CoreDNS view per namespace answering only cluster names plus the
  allow-list.
- Only proxy-aware clients work in a fenced pod; a tool that ignores `HTTPS_PROXY` fails to
  connect (fails closed, never leaks).
- Plain `http://` is refused, not proxied.
- `ssh` other than git (`GIT_SSH_COMMAND`) needs the person's own ProxyCommand.
- Builders unfenced (above).

## Tests

- `bins/egress`: host matcher (exact, wildcard not apex, case, trailing dot, IP literal); request
  head parser (CONNECT, other method → 405, oversize → 400, bad port → 403); decision function
  (UNKNOWN reflector → 503, unknown caller → 403, base hit, owner hit, miss) as pure functions; one
  loopback test: a CONNECT to an allowed `127.0.0.1:{port}` echo server tunnels bytes both ways
  (literal IP listed in the test's list).
- `crates/workspaces`: pod env/labels fenced vs open vs bench; policy shapes (`NotIn [fenced]`,
  `allow-egress-gate`); `Egress` CRD in `crds.yaml`.
- api: `PUT` validates hosts (422), refuses another owner (403), SSA writes.
- Fleet probe `ws.egress.fence`: a fenced workspace `curl https://github.com` succeeds,
  `curl https://example.com` fails with the proxy's 403, `GET /tools` reports
  `network: fenced`; then `kl net allow example.com` and the curl succeeds.
