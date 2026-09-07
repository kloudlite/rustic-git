# Package versions: `name@version` locks

Ships with the package-versions plan. A workspace's `spec.packages` entry may now be `attr@version`;
the api resolves it to a `spec.locks` row before the write, the agent builds the locked store path.

## Migration

None. Every existing list is bare attributes, which have no lock and build exactly as before; the
new `spec.locks` field defaults to empty. Apply the regenerated `deploy/k3s/crds.yaml`, roll the
api and the agents in either order (an agent ahead of the api sees no locks; an api ahead of the
agents writes locks an old agent ignores — the pin takes effect on the agent roll).

## Configuration

| what | where | default |
|---|---|---|
| Nixhub | `KLOUDLITE_NIXHUB_URL` on the api | `https://search.devbox.sh` |
| resolution cache | object store `pkgs/x86_64-linux/{attr}/{version}` | 24 h |
| mirror | object store `index/pkgs/versions.json`, refreshed daily by the api's `user` role from `fzakaria/nixpkgs-multiverse` (`index/versions.json` + `revisions.json`) | first tick at boot |

## How to verify

```sh
# A pinned workspace carries a lock with a real revision and (for a Nixhub lock) a store path.
kubectl get workspace <id> -o jsonpath='{.spec.locks}'
# The mirror exists and is fresh.
<object-store CLI> stat index/pkgs/versions.json
# Log line on a failed refresh (the previous file is kept): packages.mirror.failed
```

Hourly probe ids: `ws.packages.pin`, `ws.packages.pin.unknown`, `ws.packages.update`,
`ws.packages.pin.lockshape`.
